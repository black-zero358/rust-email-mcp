//! Principal- and generation-bound in-memory cursor leases.
use super::{
    graph::{Delivery, MAX_FOLDERS, StateError},
    state::{Coverage, ErrorSnapshot, FolderCoverage, STATE_VERSION, State},
};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    mem::size_of,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

const MAX_CURSORS: usize = 64;
const MAX_CURSOR_BYTES: usize = 8 * 1024 * 1024;
const CURSOR_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) struct Binding {
    namespace: [u8; 32],
    principal: [u8; 32],
}

impl Binding {
    pub(super) fn new(namespace: [u8; 32], principal: &str) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"conversation-principal-v1\0");
        hash.update(principal.as_bytes());
        Self {
            namespace,
            principal: hash.finalize().into(),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) struct Cursor(Uuid);

impl Cursor {
    fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for Cursor {
    type Err = StoreError;

    fn from_str(input: &str) -> Result<Self, StoreError> {
        if input.len() != 36 {
            return Err(StoreError::Unavailable);
        }
        Uuid::parse_str(input)
            .map(Self)
            .map_err(|_| StoreError::Unavailable)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum StoreError {
    Unavailable,
    Capacity,
    State(StateError),
}

impl From<StateError> for StoreError {
    fn from(error: StateError) -> Self {
        match error {
            StateError::Capacity => Self::Capacity,
            other => Self::State(other),
        }
    }
}

/// The prior input token can replay these exact metadata events, including
/// status at commit time. Body text is never retained by the cursor store.
struct Replay {
    input: Cursor,
    next: Option<Cursor>,
    delivery: Delivery,
    coverage: Coverage,
    folders: [Option<FolderCoverage>; MAX_FOLDERS],
    errors: ErrorSnapshot,
    capacity_reached: bool,
    wire_omitted: bool,
}

pub(super) struct ReplayView<'a> {
    pub next: Option<Cursor>,
    pub delivery: &'a Delivery,
    pub coverage: Coverage,
    pub errors: &'a ErrorSnapshot,
    pub capacity_reached: bool,
    pub wire_omitted: bool,
    folders: &'a [Option<FolderCoverage>; MAX_FOLDERS],
}

impl ReplayView<'_> {
    pub(super) fn folder_coverage(&self, folder: u8) -> Option<FolderCoverage> {
        self.folders.get(folder as usize).copied().flatten()
    }
}

pub(super) struct Commit {
    pub next: Option<Cursor>,
    pub coverage: Coverage,
}

/// The exact prospective next cursor and metadata used by the wire fitter.
pub(super) struct Prepared {
    next: Option<Cursor>,
    coverage: Coverage,
    delivery: Delivery,
    input: Cursor,
    revision: u64,
    mutation: u64,
    folders: [Option<FolderCoverage>; MAX_FOLDERS],
    errors: ErrorSnapshot,
    capacity_reached: bool,
    wire_omitted: bool,
}

impl Prepared {
    pub(super) fn next(&self) -> Option<Cursor> {
        self.next
    }
    pub(super) fn coverage(&self) -> Coverage {
        self.coverage
    }
    pub(super) fn folder_coverage(&self, folder: u8) -> Option<FolderCoverage> {
        self.folders.get(folder as usize).copied().flatten()
    }
    pub(super) fn errors(&self) -> &ErrorSnapshot {
        &self.errors
    }
    pub(super) fn capacity_reached(&self) -> bool {
        self.capacity_reached
    }
    pub(super) fn wire_omitted(&self) -> bool {
        self.wire_omitted
    }
}

struct Slot {
    binding: Binding,
    version: u16,
    active: Option<Cursor>,
    expires: Instant,
    revision: u64,
    state: Option<Box<State>>,
    state_bytes: usize,
    replay: Option<Arc<Replay>>,
}

impl Slot {
    fn charge(&self) -> usize {
        // Reserve one replay allocation upfront, including its Arc counters.
        self.state_bytes + size_of::<Replay>() + 2 * size_of::<usize>()
    }
}

struct Inner {
    slots: Box<[Option<Slot>; MAX_CURSORS]>,
    used: usize,
}

impl Inner {
    fn expire(&mut self, now: Instant) {
        for slot in &mut *self.slots {
            if slot
                .as_ref()
                .is_some_and(|slot| slot.state.is_some() && now >= slot.expires)
                && let Some(slot) = slot.take()
            {
                self.used -= slot.charge();
            }
        }
    }
}

#[derive(Clone)]
pub(in crate::mail) struct CursorStore(Arc<Mutex<Inner>>);

impl Default for CursorStore {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Inner {
            slots: Box::new(std::array::from_fn(|_| None)),
            used: size_of::<[Option<Slot>; MAX_CURSORS]>()
                + size_of::<Mutex<Inner>>()
                + 2 * size_of::<usize>(),
        })))
    }
}

impl CursorStore {
    /// Construct a new state within the remaining shared capacity. The closure
    /// is pure CPU work and may not perform I/O or hold the lock across awaits.
    pub(super) fn open_lease(
        &self,
        binding: Binding,
        now: Instant,
        construct: impl FnOnce(usize) -> Result<State, StateError>,
    ) -> Result<Lease, StoreError> {
        let mut inner = self.0.lock().map_err(|_| StoreError::Unavailable)?;
        inner.expire(now);
        let index = inner
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(StoreError::Capacity)?;
        let replay_charge = size_of::<Replay>() + 2 * size_of::<usize>();
        let available = MAX_CURSOR_BYTES
            .checked_sub(inner.used + replay_charge)
            .ok_or(StoreError::Capacity)?;
        let state = construct(available)?;
        if state.namespace() != &binding.namespace || state.version() != STATE_VERSION {
            return Err(StoreError::Unavailable);
        }
        let state_bytes = state.owned_bytes();
        if state_bytes > available {
            return Err(StoreError::Capacity);
        }
        let slot = Slot {
            binding,
            version: STATE_VERSION,
            active: None,
            expires: now + CURSOR_TTL,
            revision: 0,
            state: None,
            state_bytes,
            replay: None,
        };
        inner.used += slot.charge();
        inner.slots[index] = Some(slot);
        Ok(Lease {
            store: self.clone(),
            index,
            revision: 0,
            input: Cursor::new(),
            checkpoint: state.checkpoint(),
            state: Some(Box::new(state)),
            replay: None,
            unpublished: true,
            mutation: 0,
        })
    }

    pub(super) fn checkout(
        &self,
        binding: Binding,
        cursor: Cursor,
        now: Instant,
    ) -> Result<Checkout, StoreError> {
        let mut inner = self.0.lock().map_err(|_| StoreError::Unavailable)?;
        inner.expire(now);
        let (index, slot) = inner
            .slots
            .iter_mut()
            .enumerate()
            .find_map(|(index, slot)| {
                slot.as_mut()
                    .filter(|slot| {
                        slot.active == Some(cursor)
                            || slot
                                .replay
                                .as_ref()
                                .is_some_and(|replay| replay.input == cursor)
                    })
                    .map(|slot| (index, slot))
            })
            .ok_or(StoreError::Unavailable)?;
        if slot.binding != binding || slot.version != STATE_VERSION || now >= slot.expires {
            return Err(StoreError::Unavailable);
        }
        let state = slot.state.take().ok_or(StoreError::Unavailable)?;
        slot.revision = slot.revision.wrapping_add(1);
        let checkpoint = state.checkpoint();
        let mut lease = Lease {
            store: self.clone(),
            index,
            revision: slot.revision,
            input: cursor,
            checkpoint,
            state: Some(state),
            replay: None,
            unpublished: false,
            mutation: 0,
        };
        if slot.active == Some(cursor) {
            Ok(Checkout::Advance(lease))
        } else {
            lease.replay = slot.replay.clone();
            Ok(Checkout::Replay(lease))
        }
    }
}

pub(super) enum Checkout {
    Advance(Lease),
    Replay(Lease),
}

pub(super) struct Lease {
    store: CursorStore,
    index: usize,
    revision: u64,
    input: Cursor,
    checkpoint: u64,
    state: Option<Box<State>>,
    replay: Option<Arc<Replay>>,
    unpublished: bool,
    mutation: u64,
}

impl Lease {
    pub(super) fn state(&self) -> &State {
        self.state.as_deref().expect("live cursor lease owns state")
    }

    pub(super) fn replay(&self) -> Option<ReplayView<'_>> {
        self.replay.as_ref().map(|replay| ReplayView {
            next: replay.next,
            delivery: &replay.delivery,
            coverage: replay.coverage,
            folders: &replay.folders,
            errors: &replay.errors,
            capacity_reached: replay.capacity_reached,
            wire_omitted: replay.wire_omitted,
        })
    }

    /// The lock reserves the global growth budget for a single pure mutation.
    /// No network operation can run in this synchronous callback.
    pub(super) fn update<T>(
        &mut self,
        mutation: impl FnOnce(&mut State, usize) -> Result<T, StateError>,
    ) -> Result<T, StoreError> {
        if self.replay.is_some() {
            return Err(StoreError::Unavailable);
        }
        let mut inner = self.store.0.lock().map_err(|_| StoreError::Unavailable)?;
        let charged = inner.used;
        let slot = inner.slots[self.index]
            .as_mut()
            .filter(|slot| slot.revision == self.revision && slot.state.is_none())
            .ok_or(StoreError::Unavailable)?;
        let previous_bytes = slot.state_bytes;
        let available = MAX_CURSOR_BYTES - charged + previous_bytes;
        let state = self.state.as_deref_mut().ok_or(StoreError::Unavailable)?;
        let result = mutation(state, available);
        let bytes = state.owned_bytes();
        assert!(
            bytes <= available,
            "state mutations reserve owned capacity before growth"
        );
        slot.state_bytes = bytes;
        inner.used = charged - previous_bytes + bytes;
        self.mutation = self.mutation.wrapping_add(1);
        result.map_err(StoreError::from)
    }

    pub(super) fn prepare(&self, delivery: Delivery) -> Result<Prepared, StoreError> {
        if self.replay.is_some() {
            return Err(StoreError::Unavailable);
        }
        let state = self.state();
        let coverage = state.coverage_after(&delivery)?;
        let finished = coverage.graph_complete && !coverage.delivery_pending;
        let next = (!finished).then(Cursor::new);
        Ok(Prepared {
            next,
            coverage,
            delivery,
            input: self.input,
            revision: self.revision,
            mutation: self.mutation,
            folders: std::array::from_fn(|index| state.folder_coverage(index as u8)),
            errors: state.error_snapshot(),
            capacity_reached: false,
            wire_omitted: false,
        })
    }

    pub(super) fn prepare_with_capacity(
        &self,
        delivery: Delivery,
        capacity_reached: bool,
    ) -> Result<Prepared, StoreError> {
        let mut prepared = self.prepare(delivery)?;
        prepared.capacity_reached = capacity_reached;
        Ok(prepared)
    }

    pub(super) fn prepare_with_outcome(
        &self,
        delivery: Delivery,
        capacity_reached: bool,
        wire_omitted: bool,
    ) -> Result<Prepared, StoreError> {
        let mut prepared = self.prepare_with_capacity(delivery, capacity_reached)?;
        prepared.wire_omitted = wire_omitted;
        Ok(prepared)
    }

    /// Call after fitting the exact prepared metadata into the final wire
    /// response. The matching input token replays this delivery without advancing.
    pub(super) fn commit(mut self, prepared: Prepared, now: Instant) -> Result<Commit, StoreError> {
        if self.replay.is_some()
            || prepared.input != self.input
            || prepared.revision != self.revision
        {
            return Err(StoreError::Unavailable);
        }
        let mut inner = self.store.0.lock().map_err(|_| StoreError::Unavailable)?;
        let slot = inner.slots[self.index]
            .as_mut()
            .filter(|slot| slot.revision == self.revision && slot.state.is_none())
            .ok_or(StoreError::Unavailable)?;
        let state = self.state.as_deref_mut().ok_or(StoreError::Unavailable)?;
        let Prepared {
            next,
            coverage,
            delivery,
            mutation,
            folders,
            errors,
            capacity_reached,
            wire_omitted,
            ..
        } = prepared;
        if self.mutation != mutation || state.coverage_after(&delivery)? != coverage {
            return Err(StoreError::State(StateError::InvalidDelivery));
        }
        state.mark_emitted(&delivery)?;
        let finished = coverage.graph_complete && !coverage.delivery_pending;
        let progressed = state.checkpoint() != self.checkpoint || !delivery.items().is_empty();
        if !self.unpublished {
            // The exclusive lease means no replay reader can still hold the
            // old Arc. Drop it before using the reserved replay allocation.
            slot.replay = None;
            slot.replay = Some(Arc::new(Replay {
                input: self.input,
                next,
                delivery,
                coverage,
                folders,
                errors,
                capacity_reached,
                wire_omitted,
            }));
        }
        if progressed {
            slot.expires = now + CURSOR_TTL;
        }
        slot.active = next;
        slot.state = self.state.take();
        if self.unpublished && finished {
            let charge = slot.charge();
            inner.slots[self.index] = None;
            inner.used -= charge;
        }
        Ok(Commit { next, coverage })
    }

    /// Observing a different frozen UIDVALIDITY invalidates both the active
    /// token and its replay token. Drop owned data before returning quota.
    pub(super) fn invalidate(mut self) -> Result<(), StoreError> {
        let mut inner = self.store.0.lock().map_err(|_| StoreError::Unavailable)?;
        let slot = inner.slots[self.index]
            .as_ref()
            .filter(|slot| slot.revision == self.revision && slot.state.is_none())
            .ok_or(StoreError::Unavailable)?;
        let charge = slot.charge();
        self.state = None;
        self.replay = None;
        inner.slots[self.index] = None;
        inner.used -= charge;
        Ok(())
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // Release any replay reference before another lease can replace it.
        self.replay = None;
        let Some(state) = self.state.take() else {
            return;
        };
        let mut inner = self
            .store
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(slot) = &mut inner.slots[self.index]
            && slot.revision == self.revision
            && slot.state.is_none()
        {
            if self.unpublished {
                let charge = slot.charge();
                drop(state);
                inner.slots[self.index] = None;
                inner.used -= charge;
            } else {
                slot.state = Some(state);
            }
        }
    }
}

#[cfg(test)]
#[path = "tests/store.rs"]
mod tests;
