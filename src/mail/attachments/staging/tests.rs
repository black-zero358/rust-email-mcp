use super::*;
use crate::mail::parts::fetch::completion_for_test;
use std::{
    sync::{Condvar, Mutex},
    time::Duration,
};
use tokio::sync::Semaphore;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum IoPhase {
    Begin,
    Write,
    Finish,
    Read,
}
pub(super) struct IoGate {
    entered: AtomicBool,
    released: Mutex<bool>,
    changed: Condvar,
}
impl IoGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: AtomicBool::new(false),
            released: Mutex::new(false),
            changed: Condvar::new(),
        })
    }
    fn wait(&self) {
        self.entered.store(true, Ordering::SeqCst);
        let guard = self.released.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout_while(guard, Duration::from_secs(5), |released| !*released)
            .unwrap();
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
impl StagingRoot {
    pub(super) fn io_gate(&self, phase: IoPhase) {
        let gate = self
            .gate
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(selected, _)| *selected == phase)
            .map(|(_, gate)| gate.clone());
        if let Some(gate) = gate {
            gate.wait();
        }
    }
}
async fn wait_until(check: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}
fn root() -> (tempfile::TempDir, Arc<StagingRoot>, Arc<Semaphore>) {
    let directory = tempfile::tempdir().unwrap();
    let root = StagingRoot::open(&directory.path().join("private")).unwrap();
    (directory, root, Arc::new(Semaphore::new(MAX_LIVE_FILES)))
}
async fn begin(root: &Arc<StagingRoot>, permits: &Arc<Semaphore>) -> Stage {
    Store {
        root: Some(root.clone()),
    }
    .begin(permits.clone().acquire_owned().await.unwrap())
    .await
    .unwrap()
}
fn paths(root: &StagingRoot) -> Vec<PathBuf> {
    fs::read_dir(&root.path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| recognized(path.file_name().unwrap()))
        .collect()
}
#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) {
    private_options()
        .create_new(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}
#[cfg(unix)]
fn orphan(root: &Path, suffix: &str) -> PathBuf {
    root.join(format!("{PREFIX}{}.{suffix}", Uuid::new_v4().simple()))
}

#[tokio::test]
async fn cancelling_any_blocking_phase_keeps_file_and_global_quota_until_worker_finishes() {
    for phase in [
        IoPhase::Begin,
        IoPhase::Write,
        IoPhase::Finish,
        IoPhase::Read,
    ] {
        let (_directory, root, permits) = root();
        let mut stage = if phase == IoPhase::Begin {
            None
        } else {
            Some(begin(&root, &permits).await)
        };
        let ready = if phase == IoPhase::Read {
            let stage = stage
                .take()
                .unwrap()
                .write_chunk(b"synthetic".to_vec())
                .await
                .unwrap();
            let proof = completion_for_test(stage.id());
            Some(stage.finish(proof, ExpectedFile::default()).await.unwrap())
        } else {
            None
        };
        let gate = IoGate::new();
        *root.gate.lock().unwrap() = Some((phase, gate.clone()));
        let task = match phase {
            IoPhase::Begin => {
                let store = Store {
                    root: Some(root.clone()),
                };
                let permit = permits.clone().acquire_owned().await.unwrap();
                tokio::spawn(async move {
                    let _ = store.begin(permit).await;
                })
            }
            IoPhase::Write => {
                let stage = stage.unwrap();
                tokio::spawn(async move {
                    let _ = stage.write_chunk(vec![1]).await;
                })
            }
            IoPhase::Finish => {
                let stage = stage.unwrap();
                let proof = completion_for_test(stage.id());
                tokio::spawn(async move {
                    let _ = stage.finish(proof, ExpectedFile::default()).await;
                })
            }
            IoPhase::Read => {
                let ready = ready.unwrap();
                tokio::spawn(async move {
                    let _ = ready.read_chunk().await;
                })
            }
        };
        wait_until(|| gate.entered.load(Ordering::SeqCst)).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(permits.available_permits(), 3);
        assert_eq!(root.live_files(), usize::from(phase != IoPhase::Begin));
        gate.release();
        wait_until(|| permits.available_permits() == 4).await;
        assert_eq!(root.live_files(), 0);
        assert!(paths(&root).is_empty());
    }
}

#[cfg(unix)]
#[test]
fn recovery_removes_only_identified_regular_files_and_refuses_unsafe_entries() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (_directory, root, _permits) = root();
    let path = root.path.clone();
    drop(root);
    let part = orphan(&path, "part");
    let ready = orphan(&path, "ready");
    write_private(&part, b"synthetic");
    write_private(&ready, b"synthetic");
    let unknown = path.join("preserved.txt");
    fs::write(&unknown, b"synthetic unknown").unwrap();
    let reopened = StagingRoot::open(&path).unwrap();
    assert_eq!(reopened.recovery_report().removed_files, 2);
    drop(reopened);
    assert_eq!(fs::read(&unknown).unwrap(), b"synthetic unknown");
    let target = path.join("target");
    fs::write(&target, b"synthetic target").unwrap();
    let link = orphan(&path, "part");
    symlink(&target, &link).unwrap();
    assert_eq!(StagingRoot::open(&path).err(), Some(Error::UnsafeEntry));
    assert_eq!(fs::read(&target).unwrap(), b"synthetic target");
    fs::remove_file(link).unwrap();
    let owned_directory = orphan(&path, "ready");
    fs::create_dir(&owned_directory).unwrap();
    assert_eq!(StagingRoot::open(&path).err(), Some(Error::UnsafeEntry));
    fs::remove_dir(owned_directory).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(StagingRoot::open(&path).err(), Some(Error::UnsafeRoot));
}
