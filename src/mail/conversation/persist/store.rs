use super::*;
use crate::cache::cursor::{
    CursorBackend, CursorBinding, CursorError, CursorExpected, CursorScope, CursorWrite,
};
use crate::mail::conversation::graph::{Boundary, Folder};
use std::{
    sync::atomic::{AtomicI64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
#[path = "../tests/durable.rs"]
mod tests;

pub(super) use crate::cache::cursor::CursorRow;

type ScopeFilter = dyn Fn(&[u8; 32], &str) -> bool + Send + Sync;
type GenerationFilter = dyn Fn(&[u8; 32], &str, u32) -> bool + Send + Sync;

pub(super) struct Persistence {
    backend: CursorBackend,
    scope: Box<ScopeFilter>,
    generation_matches: Box<GenerationFilter>,
    available: Box<ScopeFilter>,
    wall_high: AtomicI64,
}

impl Persistence {
    pub(super) fn configured(&self, state: &State) -> bool {
        state
            .folders()
            .iter()
            .all(|folder| (self.scope)(state.namespace(), &folder.name))
    }

    fn available(&self, state: &State) -> bool {
        self.configured(state)
            && state
                .folders()
                .iter()
                .all(|folder| (self.available)(state.namespace(), &folder.name))
    }

    fn now_ms(&self) -> Result<i64, StoreError> {
        let now = utc_millis()?;
        if now < self.wall_high.fetch_max(now, Ordering::Relaxed) {
            return Err(StoreError::Unavailable);
        }
        Ok(now)
    }
}

const TTL_MS: i64 = 600_000;

fn utc_millis() -> Result<i64, StoreError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::Unavailable)?
        .as_millis()
        .try_into()
        .map_err(|_| StoreError::Unavailable)
}

fn remaining(row: CursorRow, now_ms: i64) -> Result<Duration, StoreError> {
    let elapsed_limit = row
        .expires_ms
        .checked_sub(row.last_progress_ms)
        .ok_or(StoreError::Unavailable)?;
    if row.last_progress_ms > now_ms
        || !(1..=TTL_MS).contains(&elapsed_limit)
        || row.expires_ms <= now_ms
    {
        return Err(StoreError::Unavailable);
    }
    Ok(Duration::from_millis((row.expires_ms - now_ms) as u64))
}

fn cursor_error(error: CursorError) -> StoreError {
    match error {
        CursorError::Quota => StoreError::Capacity,
        _ => StoreError::Unavailable,
    }
}

impl From<Binding> for CursorBinding {
    fn from(binding: Binding) -> Self {
        Self {
            namespace: binding.namespace,
            principal: binding.principal,
        }
    }
}

impl From<CursorBinding> for Binding {
    fn from(binding: CursorBinding) -> Self {
        Self {
            namespace: binding.namespace,
            principal: binding.principal,
        }
    }
}

fn token(bytes: [u8; 16]) -> Cursor {
    Cursor(Uuid::from_bytes(bytes))
}

impl CursorStore {
    pub(in crate::mail::conversation) fn durable_scope(
        &self,
        namespace: &[u8; 32],
        folders: &[Folder],
    ) -> bool {
        self.1.as_ref().is_some_and(|persistence| {
            folders
                .iter()
                .all(|folder| (persistence.scope)(namespace, &folder.name))
        })
    }

    /// Reserve all durable rows before accepting new memory-only cursors. Mail
    /// facts are decoded lazily on authorized checkout, within this reservation.
    pub(in crate::mail) fn with_backend(
        backend: CursorBackend,
        scope: impl Fn(&[u8; 32], &str) -> bool + Send + Sync + 'static,
        generation_matches: impl Fn(&[u8; 32], &str, u32) -> bool + Send + Sync + 'static,
        available: impl Fn(&[u8; 32], &str) -> bool + Send + Sync + 'static,
    ) -> Result<Self, &'static str> {
        Self::restore(
            backend,
            scope,
            generation_matches,
            available,
            Instant::now(),
        )
        .map_err(|_| "conversation cache unavailable")
    }

    fn restore(
        backend: CursorBackend,
        scope: impl Fn(&[u8; 32], &str) -> bool + Send + Sync + 'static,
        generation_matches: impl Fn(&[u8; 32], &str, u32) -> bool + Send + Sync + 'static,
        available: impl Fn(&[u8; 32], &str) -> bool + Send + Sync + 'static,
        now: Instant,
    ) -> Result<Self, StoreError> {
        let now_ms = utc_millis()?;
        let reservations = backend.reservations(now_ms).map_err(cursor_error)?;
        if reservations.len() > MAX_CURSORS {
            return Err(StoreError::Capacity);
        }
        let mut store = Self::default();
        {
            let mut inner = store.0.lock().map_err(|_| StoreError::Unavailable)?;
            for (index, (binding, row)) in reservations.into_iter().enumerate() {
                if row.codec_version != persist::CODEC_VERSION {
                    continue;
                }
                let state_bytes = row
                    .owned_bytes
                    .checked_sub(size_of::<Replay>() + 2 * size_of::<usize>())
                    .filter(|n| *n >= size_of::<State>())
                    .ok_or(StoreError::Unavailable)?;
                let slot = Slot {
                    binding: binding.into(),
                    version: STATE_VERSION,
                    active: row.active.map(token),
                    expires: now + remaining(row, now_ms)?,
                    revision: 0,
                    leased: false,
                    state: None,
                    state_bytes,
                    replay: None,
                    durable: Some(row),
                };
                inner.used = inner
                    .used
                    .checked_add(slot.charge())
                    .filter(|used| *used <= MAX_CURSOR_BYTES)
                    .ok_or(StoreError::Capacity)?;
                inner.slots[index] = Some(slot);
            }
        }
        store.1 = Some(Arc::new(Persistence {
            backend,
            scope: Box::new(scope),
            generation_matches: Box::new(generation_matches),
            available: Box::new(available),
            wall_high: AtomicI64::new(now_ms),
        }));
        Ok(store)
    }

    pub(in crate::mail::conversation) async fn checkout_async(
        &self,
        binding: Binding,
        cursor: Cursor,
        now: Instant,
    ) -> Result<Checkout, StoreError> {
        let Some(persistence) = self.1.clone() else {
            return self.checkout(binding, cursor, now);
        };
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            store.checkout_durable(binding, cursor, now, &persistence)
        })
        .await
        .map_err(|_| StoreError::Unavailable)?
    }

    fn checkout_durable(
        &self,
        binding: Binding,
        cursor: Cursor,
        now: Instant,
        persistence: &Persistence,
    ) -> Result<Checkout, StoreError> {
        let Acquired {
            mut lease,
            row,
            state_bytes,
            advancing,
        } = self.acquire(binding, cursor, now)?;
        let Some(row) = row else {
            if !lease.generation_matches() {
                lease.invalidate()?;
                return Err(StoreError::Unavailable);
            }
            return lease.into_checkout(advancing);
        };
        let now_ms = persistence.now_ms()?;
        let bytes = *cursor.0.as_bytes();
        let result = if lease.state.is_some() {
            persistence
                .backend
                .validate(binding.into(), bytes, now_ms)
                .and_then(|actual| verify_row(actual, row))
        } else {
            let restored =
                persistence
                    .backend
                    .load(binding.into(), bytes, now_ms, |actual, reader| {
                        verify_row(actual, row)?;
                        let (state, replay) = persist::decode(
                            reader,
                            actual.encoded_len,
                            state_bytes,
                            binding.namespace,
                        )
                        .map_err(|_| CursorError::Corrupt)?;
                        if replay.as_ref().map(|item| *item.input.0.as_bytes()) != row.replay
                            || replay.as_ref().is_some_and(|item| {
                                item.next.map(|next| *next.0.as_bytes()) != row.active
                            })
                            || !persistence.configured(&state)
                        {
                            return Err(CursorError::Unavailable);
                        }
                        Ok((state, replay))
                    });
            restored.and_then(|(state, replay)| {
                let mut inner = self.0.lock().map_err(|_| CursorError::Storage)?;
                let slot = inner.slots[lease.index]
                    .as_mut()
                    .ok_or(CursorError::Unavailable)?;
                slot.replay = replay.map(Arc::new);
                if !advancing {
                    lease.replay = Some(slot.replay.clone().ok_or(CursorError::Corrupt)?);
                }
                lease.checkpoint = state.checkpoint();
                lease.state = Some(Box::new(state));
                Ok(())
            })
        };
        if let Err(error) = result {
            if matches!(
                error,
                CursorError::Unavailable | CursorError::Corrupt | CursorError::Conflict
            ) {
                lease.invalidate()?;
            }
            return Err(cursor_error(error));
        }
        if !lease.generation_matches() {
            lease.invalidate()?;
            return Err(StoreError::Unavailable);
        }
        if !persistence.available(lease.state()) {
            return Err(StoreError::Unavailable);
        }
        lease.into_checkout(advancing)
    }
}

fn verify_row(actual: CursorRow, expected: CursorRow) -> Result<(), CursorError> {
    if actual != expected {
        return Err(CursorError::Conflict);
    }
    Ok(())
}

impl Lease {
    pub(in crate::mail::conversation) fn durable_mode(&self) -> bool {
        self.state().durable()
    }

    pub(in crate::mail::conversation) fn generation_matches(&self) -> bool {
        self.store.1.as_ref().is_none_or(|persistence| {
            let state = self.state();
            state.folders().iter().all(|folder| match folder.boundary {
                Boundary::Frozen { uid_validity, .. } => {
                    (persistence.generation_matches)(state.namespace(), &folder.name, uid_validity)
                }
                Boundary::Unavailable(_) => true,
            })
        })
    }

    pub(in crate::mail::conversation) fn persistence_available(&self) -> bool {
        !self.state().durable()
            || self
                .store
                .1
                .as_ref()
                .is_some_and(|persistence| persistence.available(self.state()))
    }

    pub(in crate::mail::conversation) async fn commit_async(
        self,
        prepared: Prepared,
        now: Instant,
        cancellation: CancellationToken,
    ) -> Result<Commit, StoreError> {
        if cancellation.is_cancelled() || !self.generation_matches() {
            return Err(StoreError::Unavailable);
        }
        let Some(persistence) = self.store.1.clone() else {
            return self.commit(prepared, now);
        };
        if (self.unpublished && prepared.next.is_none()) || !self.state().durable() {
            return self.commit(prepared, now);
        }
        if !persistence.available(self.state()) {
            return Err(StoreError::Unavailable);
        }
        // Dropping the RPC future also cancels a detached blocking job before
        // its storage commit point. The job keeps the exclusive lease.
        let cancellation = cancellation.child_token();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        tokio::task::spawn_blocking(move || {
            self.commit_durable(prepared, now, cancellation, &persistence)
        })
        .await
        .map_err(|_| StoreError::Unavailable)?
    }

    fn commit_durable(
        self,
        prepared: Prepared,
        now: Instant,
        cancellation: CancellationToken,
        persistence: &Persistence,
    ) -> Result<Commit, StoreError> {
        if self.replay.is_some()
            || prepared.input != self.input
            || prepared.revision != self.revision
            || self.mutation != prepared.mutation
            || self.state().coverage_after(&prepared.delivery)? != prepared.coverage
        {
            return Err(StoreError::Unavailable);
        }
        let (binding, old, expires, owned_bytes) = {
            let inner = self.store.0.lock().map_err(|_| StoreError::Unavailable)?;
            let slot = inner.slots[self.index]
                .as_ref()
                .filter(|slot| slot.revision == self.revision && slot.leased)
                .ok_or(StoreError::Unavailable)?;
            (slot.binding, slot.durable, slot.expires, slot.charge())
        };
        let now_ms = persistence.now_ms()?;
        if old.is_some_and(|row| row.last_progress_ms > now_ms) || cancellation.is_cancelled() {
            return Err(StoreError::Unavailable);
        }
        let progressed =
            self.state().checkpoint() != self.checkpoint || !prepared.delivery.items().is_empty();
        let (last_progress_ms, expires_ms) = if progressed {
            (
                now_ms,
                now_ms.checked_add(TTL_MS).ok_or(StoreError::Unavailable)?,
            )
        } else if let Some(row) = old {
            (row.last_progress_ms, row.expires_ms)
        } else {
            let remaining: i64 = expires
                .saturating_duration_since(now)
                .as_millis()
                .try_into()
                .map_err(|_| StoreError::Unavailable)?;
            let expires_ms = now_ms
                .checked_add(remaining)
                .ok_or(StoreError::Unavailable)?;
            (
                expires_ms
                    .checked_sub(TTL_MS)
                    .ok_or(StoreError::Unavailable)?,
                expires_ms,
            )
        };
        let mut frozen = [CursorScope {
            mailbox: "",
            uid_validity: 0,
        }; MAX_FOLDERS];
        let mut count = 0;
        for folder in self.state().folders() {
            if let Boundary::Frozen { uid_validity, .. } = folder.boundary {
                frozen[count] = CursorScope {
                    mailbox: &folder.name,
                    uid_validity,
                };
                count += 1;
            }
        }
        let active = prepared.next.map(|next| *next.0.as_bytes());
        let replay = (!self.unpublished).then(|| *self.input.0.as_bytes());
        let encoded_len = persist::encode(
            &mut std::io::sink(),
            self.state(),
            &prepared,
            replay.is_some(),
        )
        .map_err(|_| StoreError::Capacity)?;
        let expected = old.map(|row| CursorExpected {
            token: *self.input.0.as_bytes(),
            revision: row.revision,
        });
        let revision = old
            .map_or(Some(1), |row| row.revision.checked_add(1))
            .ok_or(StoreError::Unavailable)?;
        persistence
            .backend
            .save(
                CursorWrite {
                    binding: binding.into(),
                    expected,
                    active,
                    replay,
                    now_ms,
                    last_progress_ms,
                    expires_ms,
                    codec_version: persist::CODEC_VERSION,
                    owned_bytes,
                    encoded_len,
                    frozen: &frozen[..count],
                    cancelled: &|| {
                        cancellation.is_cancelled()
                            || !self.generation_matches()
                            || !persistence.available(self.state())
                    },
                },
                |writer| {
                    let written =
                        persist::encode(writer, self.state(), &prepared, replay.is_some())
                            .map_err(|_| CursorError::Storage)?;
                    if written != encoded_len {
                        return Err(CursorError::Corrupt);
                    }
                    Ok(())
                },
            )
            .map_err(cursor_error)?;
        let durable = CursorRow {
            active,
            replay,
            revision,
            codec_version: persist::CODEC_VERSION,
            owned_bytes,
            encoded_len,
            expires_ms,
            last_progress_ms,
        };
        {
            let mut inner = self.store.0.lock().map_err(|_| StoreError::Unavailable)?;
            let slot = inner.slots[self.index]
                .as_mut()
                .filter(|slot| slot.revision == self.revision && slot.leased)
                .ok_or(StoreError::Unavailable)?;
            slot.durable = Some(durable);
        }
        self.commit(prepared, now)
    }

    pub(in crate::mail::conversation) async fn invalidate_async(self) -> Result<(), StoreError> {
        let Some(persistence) = self.store.1.clone() else {
            return self.invalidate();
        };
        tokio::task::spawn_blocking(move || {
            let (binding, row) = {
                let inner = self.store.0.lock().map_err(|_| StoreError::Unavailable)?;
                let slot = inner.slots[self.index]
                    .as_ref()
                    .filter(|slot| slot.revision == self.revision && slot.leased)
                    .ok_or(StoreError::Unavailable)?;
                (slot.binding, slot.durable)
            };
            let result = row.map(|row| {
                persistence.backend.invalidate(
                    binding.into(),
                    *self.input.0.as_bytes(),
                    row.revision,
                )
            });
            self.invalidate()?;
            match result {
                Some(Err(CursorError::Unavailable | CursorError::Conflict))
                | None
                | Some(Ok(())) => Ok(()),
                Some(Err(error)) => Err(cursor_error(error)),
            }
        })
        .await
        .map_err(|_| StoreError::Unavailable)?
    }
}
