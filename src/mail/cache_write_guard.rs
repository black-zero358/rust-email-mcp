//! A dirty fence for existing writes. Holding the guard does not hold a DB lock.
use crate::{
    accounts::AccountKey,
    cache::{Cache, DirtyTicket, MAX_FOLDERS_PER_ACCOUNT},
    input_limits::ReadInputLimits,
};
use std::{sync::Arc, time::Duration};
use tokio::{
    runtime::Handle,
    sync::{OwnedSemaphorePermit, Semaphore},
};

pub(super) struct WriteGuardPool {
    permits: Arc<Semaphore>,
    capacity: u32,
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
        }
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
