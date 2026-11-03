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
fn write_private(path: &Path, bytes: &[u8]) {
    private_options()
        .create_new(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}
fn orphan(root: &Path, suffix: &str) -> PathBuf {
    root.join(format!("{PREFIX}{}.{suffix}", Uuid::new_v4().simple()))
}

#[tokio::test]
async fn completed_file_is_private_hashed_chunked_and_deleted_with_both_permits() {
    let (_directory, root, permits) = root();
    let bytes = "合成CSV,😀\r\n1,2\r\n".as_bytes();
    let stage = begin(&root, &permits)
        .await
        .write_chunk(bytes.to_vec())
        .await
        .unwrap();
    assert!(private_mode(&fs::metadata(&root.path).unwrap(), 0o700));
    assert!(private_mode(
        &fs::metadata(&paths(&root)[0]).unwrap(),
        0o600
    ));
    let proof = completion_for_test(stage.id());
    let digest = Sha256::digest(bytes).into();
    let ready = stage
        .finish(
            proof,
            ExpectedFile {
                decoded_size: Some(bytes.len()),
                sha256: Some(digest),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        ready.metadata(),
        ReadyMetadata {
            decoded_size: bytes.len(),
            sha256: digest
        }
    );
    assert_eq!(paths(&root).len(), 1);
    assert_eq!(paths(&root)[0].extension().unwrap(), "ready");
    let (ready, chunk) = ready.read_chunk().await.unwrap();
    assert_eq!(chunk, bytes);
    let (ready, end) = ready.read_chunk().await.unwrap();
    assert!(end.is_empty());
    assert_eq!(root.live_files(), 1);
    assert_eq!(permits.available_permits(), 3);
    drop(ready);
    assert_eq!(root.live_files(), 0);
    assert_eq!(permits.available_permits(), 4);
    assert!(paths(&root).is_empty());
}

#[tokio::test]
async fn exact_empty_and_unknown_sizes_work_but_size_hash_and_foreign_proofs_fail_closed() {
    let (_directory, root, permits) = root();
    let stage = begin(&root, &permits).await;
    let proof = completion_for_test(stage.id());
    let ready = stage
        .finish(
            proof,
            ExpectedFile {
                decoded_size: Some(0),
                sha256: Some(Sha256::digest([]).into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(ready.metadata().decoded_size, 0);
    drop(ready);
    let stage = begin(&root, &permits)
        .await
        .write_chunk(b"synthetic".to_vec())
        .await
        .unwrap();
    let proof = completion_for_test(stage.id());
    let ready = stage.finish(proof, ExpectedFile::default()).await.unwrap();
    assert_eq!(ready.metadata().decoded_size, 9);
    drop(ready);
    for expected in [
        ExpectedFile {
            decoded_size: Some(99),
            sha256: None,
        },
        ExpectedFile {
            decoded_size: None,
            sha256: Some([0; 32]),
        },
    ] {
        let stage = begin(&root, &permits)
            .await
            .write_chunk(b"synthetic".to_vec())
            .await
            .unwrap();
        let proof = completion_for_test(stage.id());
        assert!(stage.finish(proof, expected).await.is_err());
        assert!(paths(&root).is_empty());
        assert_eq!(permits.available_permits(), 4);
    }
    let first = begin(&root, &permits).await;
    let foreign = completion_for_test(first.id());
    drop(first);
    let second = begin(&root, &permits).await;
    assert_eq!(
        second.finish(foreign, ExpectedFile::default()).await.err(),
        Some(Error::CompletionMismatch)
    );
    assert!(paths(&root).is_empty());
}

#[tokio::test]
async fn four_files_and_ready_consumers_share_one_bounded_file_quota() {
    let (_directory, root, permits) = root();
    let mut stages = Vec::new();
    for _ in 0..MAX_LIVE_FILES {
        stages.push(begin(&root, &permits).await);
    }
    let other_pool = Arc::new(Semaphore::new(1));
    let rejected = Store {
        root: Some(root.clone()),
    }
    .begin(other_pool.clone().acquire_owned().await.unwrap())
    .await;
    assert_eq!(rejected.err(), Some(Error::Capacity));
    assert_eq!(other_pool.available_permits(), 1);
    let stage = stages.pop().unwrap();
    let proof = completion_for_test(stage.id());
    let ready = stage.finish(proof, ExpectedFile::default()).await.unwrap();
    assert_eq!(root.live_files(), 4);
    assert_eq!(permits.available_permits(), 0);
    assert_eq!(MAX_LIVE_FILES * MAX_FILE_BYTES, 128 * 1024 * 1024);
    drop((stages, ready));
    assert_eq!(root.live_files(), 0);
    assert_eq!(permits.available_permits(), 4);
}

#[tokio::test]
async fn chunks_and_full_file_limits_are_checked_before_writing() {
    let (_directory, root, permits) = root();
    let stage = begin(&root, &permits).await;
    assert_eq!(
        stage.write_chunk(vec![0; MAX_CHUNK_BYTES + 1]).await.err(),
        Some(Error::InvalidChunk)
    );
    assert!(paths(&root).is_empty());
    assert_eq!(permits.available_permits(), 4);
    let mut stage = begin(&root, &permits).await;
    for _ in 0..512 {
        stage = stage
            .write_chunk(vec![b'x'; MAX_CHUNK_BYTES])
            .await
            .unwrap();
    }
    assert_eq!(stage.bytes, MAX_FILE_BYTES);
    let proof = completion_for_test(stage.id());
    let ready = stage
        .finish(
            proof,
            ExpectedFile {
                decoded_size: Some(MAX_FILE_BYTES),
                sha256: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(ready.metadata().decoded_size, MAX_FILE_BYTES);
    drop(ready);
    let mut stage = begin(&root, &permits).await;
    for _ in 0..512 {
        stage = stage
            .write_chunk(vec![b'x'; MAX_CHUNK_BYTES])
            .await
            .unwrap();
    }
    assert_eq!(
        stage.write_chunk(vec![b'x']).await.err(),
        Some(Error::SizeLimit)
    );
    assert!(paths(&root).is_empty());
    assert_eq!(permits.available_permits(), 4);
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

#[tokio::test]
async fn timeout_and_partial_consumer_drop_cleanup_files() {
    let (_directory, root, permits) = root();
    let operation = async {
        let stage = begin(&root, &permits)
            .await
            .write_chunk(b"synthetic timeout".to_vec())
            .await
            .unwrap();
        std::future::pending::<()>().await;
        drop(stage);
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(10), operation)
            .await
            .is_err()
    );
    wait_until(|| permits.available_permits() == 4).await;
    assert_eq!(root.live_files(), 0);
    assert!(paths(&root).is_empty());
    let stage = begin(&root, &permits)
        .await
        .write_chunk(vec![1; MAX_CHUNK_BYTES])
        .await
        .unwrap()
        .write_chunk(vec![2; MAX_CHUNK_BYTES])
        .await
        .unwrap();
    let proof = completion_for_test(stage.id());
    let ready = stage.finish(proof, ExpectedFile::default()).await.unwrap();
    let (ready, _) = ready.read_chunk().await.unwrap();
    drop(ready);
    assert_eq!(permits.available_permits(), 4);
    assert!(paths(&root).is_empty());
}

#[tokio::test]
async fn download_begin_timeout_keeps_transfer_permit_until_blocking_io_finishes() {
    use super::super::{AttachmentStore, Part, Transfer, download_staged_selected};
    let (_directory, root, permits) = root();
    let gate = IoGate::new();
    *root.gate.lock().unwrap() = Some((IoPhase::Begin, gate.clone()));
    let store = Arc::new(AttachmentStore {
        handles: Default::default(),
        transfers: permits.clone(),
        staging: Store {
            root: Some(root.clone()),
        },
    });
    let mut config = (*crate::config::fixture()).clone();
    config.timeout = Duration::from_millis(100);
    let transfer = Transfer {
        principal: "synthetic".into(),
        account: crate::accounts::AccountKey::synthetic(),
        mailbox: "INBOX".into(),
        uid: 7,
        validity: 9,
        part: Part::default(),
        expires: std::time::Instant::now() + Duration::from_secs(600),
    };
    let response = tokio::spawn(async move {
        download_staged_selected(
            &store,
            &config,
            transfer,
            "synthetic-request".into(),
            |_, _, _| async { panic!("backend must not start before staging completes") },
        )
        .await
    });
    wait_until(|| gate.entered.load(Ordering::SeqCst)).await;
    let response = response.await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::GATEWAY_TIMEOUT);
    let body = axum::body::to_bytes(response.into_body(), 2048)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], "TIMEOUT");
    assert_eq!(body["error"]["phase"], "staging");
    assert_eq!(permits.available_permits(), 3);
    assert_eq!(root.live_files(), 0);
    gate.release();
    wait_until(|| permits.available_permits() == 4).await;
    assert_eq!(root.live_files(), 0);
    assert!(paths(&root).is_empty());
}

#[tokio::test]
async fn actual_read_failure_releases_file_and_global_transfer_permit() {
    let (_directory, root, permits) = root();
    let stage = begin(&root, &permits)
        .await
        .write_chunk(b"synthetic read failure".to_vec())
        .await
        .unwrap();
    let proof = completion_for_test(stage.id());
    let mut ready = stage.finish(proof, ExpectedFile::default()).await.unwrap();
    let path = paths(&root).pop().unwrap();
    drop(ready.owned.file.take());
    ready.owned.file = Some(OpenOptions::new().write(true).open(&path).unwrap());
    assert!(matches!(ready.read_chunk().await, Err(Error::Io(_))));
    assert!(!path.exists());
    assert_eq!(root.live_files(), 0);
    assert_eq!(permits.available_permits(), 4);
}

#[test]
fn recovery_scan_is_bounded_and_preserves_unrecognized_files() {
    let (_directory, root, _permits) = root();
    let path = root.path.clone();
    drop(root);
    for index in 0..MAX_RECOVERY_ENTRIES {
        fs::write(path.join(format!("unknown-{index}")), b"synthetic").unwrap();
    }
    assert_eq!(
        StagingRoot::open(&path).err(),
        Some(Error::RecoveryEntryLimit)
    );
    assert_eq!(
        fs::read_dir(path).unwrap().count(),
        MAX_RECOVERY_ENTRIES + 1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn lifecycle_lock_outlives_root_handle_and_is_exclusive_in_another_process() {
    let (directory, root, permits) = root();
    let stage = begin(&root, &permits).await;
    let proof = completion_for_test(stage.id());
    let ready = stage.finish(proof, ExpectedFile::default()).await.unwrap();
    let path = root.path.clone();
    drop(root);
    assert_eq!(StagingRoot::open(&path).err(), Some(Error::InUse));
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "mail::attachments::staging::tests::subprocess_lock_probe",
        ])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stdout)
    );
    drop(ready);
    assert!(StagingRoot::open(&path).is_ok());
}

#[test]
#[ignore = "invoked explicitly by the parent lifecycle lock regression"]
fn subprocess_lock_probe() {
    let path = std::env::current_dir().unwrap().join("private");
    if path.exists() {
        assert_eq!(StagingRoot::open(&path).err(), Some(Error::InUse));
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

#[cfg(unix)]
#[tokio::test]
async fn failed_cleanup_keeps_quota_reserved_and_poisoned_until_locked_recovery() {
    use std::os::unix::fs::PermissionsExt;
    let (_directory, root, permits) = root();
    let path = root.path.clone();
    let stage = begin(&root, &permits)
        .await
        .write_chunk(b"synthetic residue".to_vec())
        .await
        .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();
    drop(stage);
    assert_eq!(root.live_files(), 1);
    assert!(root.is_poisoned());
    assert_eq!(permits.available_permits(), 4);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        !Store {
            root: Some(root.clone())
        }
        .is_available()
    );
    drop(root);
    let reopened = StagingRoot::open(&path).unwrap();
    assert_eq!(reopened.recovery_report().removed_files, 1);
}

#[tokio::test]
async fn write_failure_and_publish_collision_cleanup_without_overwriting_other_file() {
    let (_directory, root, permits) = root();
    let mut stage = begin(&root, &permits).await;
    let path = paths(&root).pop().unwrap();
    let owned = stage.owned.as_mut().unwrap();
    drop(owned.file.take());
    owned.file = Some(File::open(&path).unwrap());
    assert!(matches!(
        stage.write_chunk(vec![1]).await,
        Err(Error::Io(_))
    ));
    assert!(!path.exists());
    let stage = begin(&root, &permits).await;
    let collision = root
        .path
        .join(format!("{PREFIX}{}.ready", stage.id.0.simple()));
    write_private(&collision, b"synthetic existing file");
    let proof = completion_for_test(stage.id());
    assert!(stage.finish(proof, ExpectedFile::default()).await.is_err());
    assert_eq!(fs::read(collision).unwrap(), b"synthetic existing file");
    assert!(root.is_poisoned());
}

#[cfg(unix)]
#[test]
fn configuration_paths_are_nonfatal_explicit_and_checked_before_canonicalization() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let mut config = (*crate::config::fixture()).clone();
    assert!(!Store::from_config(&config).is_available());
    config.database = directory
        .path()
        .join("operations.sqlite")
        .to_str()
        .unwrap()
        .into();
    assert_eq!(
        configured_root(&config).unwrap(),
        directory.path().join("mail-downloads")
    );
    let store = Store::from_config(&config);
    assert!(store.is_available());
    drop(store);
    config.attachment_staging_dir = Some(PathBuf::new());
    assert!(!Store::from_config(&config).is_available());
    config.attachment_staging_dir = Some(directory.path().join("missing/../outside"));
    assert!(!Store::from_config(&config).is_available());
    let link = directory.path().join("linked");
    symlink(directory.path(), &link).unwrap();
    config.attachment_staging_dir = Some(link.join("private"));
    assert!(!Store::from_config(&config).is_available());
    config.attachment_staging_dir = None;
    config.database = link.join("operations.sqlite").to_str().unwrap().into();
    assert!(!Store::from_config(&config).is_available());
    assert!(
        absolute_without_parent(Path::new("./data/downloads"))
            .unwrap()
            .is_absolute()
    );
    assert!(absolute_without_parent(Path::new("../downloads")).is_err());
}
