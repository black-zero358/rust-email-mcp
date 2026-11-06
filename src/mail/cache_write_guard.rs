//! A dirty fence for existing writes. Holding the guard does not hold a DB lock.
use crate::{
    accounts::AccountKey,
    cache::{Cache, DirtyTicket, MAX_FOLDERS_PER_ACCOUNT},
    input_limits::ReadInputLimits,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    runtime::Handle,
    sync::{Notify, OwnedSemaphorePermit, Semaphore},
    time::Instant,
};

pub(super) struct WriteGuardPool {
    permits: Arc<Semaphore>,
    capacity: u32,
    admission: Arc<WriteLifecycle>,
}

struct WriteLifecycle {
    state: Mutex<WriteState>,
    changed: Notify,
}

#[derive(Default)]
struct WriteState {
    sealed: bool,
    active: usize,
}

#[must_use = "hold write admission until the authorized mutation lifecycle ends"]
pub(super) struct WriteAdmission(Arc<WriteLifecycle>);

impl Drop for WriteAdmission {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.state.lock() {
            state.active -= 1;
            if state.active == 0 {
                self.0.changed.notify_one();
            }
        }
    }
}

impl Default for WriteGuardPool {
    fn default() -> Self {
        Self::with_capacity(super::EXECUTION_SLOTS as u32)
    }
}

impl WriteGuardPool {
    fn with_capacity(capacity: u32) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(capacity as usize)),
            capacity,
            admission: Arc::new(WriteLifecycle {
                state: Mutex::default(),
                changed: Notify::new(),
            }),
        }
    }

    /// Call before an authorized write enters the execution queue, including
    /// cache-off and degraded runs. Sealing does not revoke existing tickets.
    pub(super) fn admit(&self) -> Option<WriteAdmission> {
        let mut state = self.admission.state.lock().ok()?;
        if state.sealed {
            return None;
        }
        state.active = state.active.checked_add(1)?;
        Some(WriteAdmission(self.admission.clone()))
    }

    /// Close admission permanently before publishing a clean receipt. The
    /// same budget covers queued writes, active writes and detached SQLite
    /// work; a timeout leaves the durable run unclean.
    pub(super) async fn seal_and_drain(&self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        match self.admission.state.lock() {
            Ok(mut state) => state.sealed = true,
            Err(_) => return false,
        }
        loop {
            match self.admission.state.lock() {
                Ok(state) if state.active == 0 => break,
                Ok(_) => {}
                Err(_) => return false,
            }
            if tokio::time::timeout_at(deadline, self.admission.changed.notified())
                .await
                .is_err()
            {
                return false;
            }
        }
        self.drain(deadline.saturating_duration_since(Instant::now()))
            .await
    }

    /// The caller has already stopped write admission. A timeout leaves the
    /// process unclean so an unfinished fence cannot produce a clean receipt.
    pub(super) async fn drain(&self, wait: Duration) -> bool {
        matches!(
            tokio::time::timeout(wait, self.permits.clone().acquire_many_owned(self.capacity))
                .await,
            Ok(Ok(_))
        )
    }

    /// Call after authorization and protocol preconditions, before sending the
    /// first mutating command. A failed fence closes offline reads before the
    /// caller continues the existing online write path.
    pub(super) async fn begin(
        &self,
        cache: &Arc<Cache>,
        account: &AccountKey,
        mailboxes: &[&str],
        wait: Duration,
    ) -> Option<DirtyWriteGuard> {
        if mailboxes.is_empty() {
            return None;
        }
        if wait.is_zero()
            || mailboxes.len() > MAX_FOLDERS_PER_ACCOUNT
            || mailboxes
                .iter()
                .any(|mailbox| mailbox.len() > ReadInputLimits::TEXT_UTF8_BYTES)
        {
            cache.disable_offline();
            return None;
        }
        let Ok(permit) = self.permits.clone().try_acquire_owned() else {
            cache.disable_offline();
            return None;
        };
        let scopes = mailboxes
            .iter()
            .map(|mailbox| (*mailbox).to_owned())
            .collect::<Vec<_>>();
        let account = account.clone();
        let storage = cache.clone();
        let runtime = Handle::current();
        let task = tokio::task::spawn_blocking(move || {
            let mailboxes = scopes.iter().map(String::as_str).collect::<Vec<_>>();
            let ticket = storage.begin_dirty(&account, &mailboxes)?;
            Ok::<_, crate::cache::CacheError>(DirtyWriteGuard {
                pending: Some(PendingFence {
                    storage,
                    ticket,
                    _permit: permit,
                }),
                runtime,
            })
        });

        match tokio::time::timeout(wait, task).await {
            Ok(Ok(Ok(guard))) => Some(guard),
            _ => {
                cache.disable_offline();
                None
            }
        }
    }
}

#[must_use = "keep the dirty fence alive through the possible remote side effects"]
pub(super) struct DirtyWriteGuard {
    pending: Option<PendingFence>,
    runtime: Handle,
}

struct PendingFence {
    storage: Arc<Cache>,
    ticket: DirtyTicket,
    _permit: OwnedSemaphorePermit,
}

impl Drop for DirtyWriteGuard {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            // The detached cleanup owns the same permit until SQLite work
            // ends. It removes only the in-flight claim. A crash or an unknown
            // command result still leaves the durable scope dirty.
            self.runtime.spawn_blocking(move || {
                let PendingFence {
                    storage,
                    ticket,
                    _permit,
                } = pending;
                if storage.finish_dirty(ticket).is_err() {
                    storage.disable_offline();
                }
            });
        }
    }
}

#[cfg(test)]
#[path = "cache_write_guard/tests.rs"]
mod tests;
