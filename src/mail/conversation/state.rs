//! Frozen mailbox scope and resumable, exact-field graph traversal.
use super::{
    graph::*,
    identity::{MAX_IDS_PER_MESSAGE, MessageId, ThreadFacts},
};
use std::mem::size_of;

pub(super) const MAX_CANDIDATES: usize = 200;
pub(super) const MAX_BODY_BYTES: usize = 256 * 1024;
pub(super) const MAX_PAGE_SECONDS: u64 = 60;
pub(super) const STATE_VERSION: u16 = 1;
const MAX_ERROR_DETAILS: usize = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Options {
    pub candidate_limit: usize,
    pub result_limit: usize,
    pub max_body_length: Option<usize>,
    pub trimmed: bool,
    pub readable_html: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            candidate_limit: MAX_CANDIDATES,
            result_limit: MAX_RESULTS,
            max_body_length: None,
            trimmed: false,
            readable_html: false,
        }
    }
}

#[derive(Clone, Copy)]
struct Position {
    next_high: u32,
    failure: Option<Failure>,
    blocked: bool,
}

struct Target {
    id: MessageId,
    positions: Box<[[Position; 3]]>,
}

#[derive(Clone, Copy)]
struct Pending {
    candidate: Candidate,
    failure: Option<Failure>,
    blocked: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Coverage {
    pub completed_windows: u64,
    pub remaining_windows: u64,
    pub pending_candidates: usize,
    pub unresolved_errors: usize,
    pub known_locations: usize,
    pub graph_complete: bool,
    pub delivery_pending: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FolderCoverage {
    pub completed_windows: u64,
    pub remaining_windows: u64,
    pub pending_candidates: usize,
    pub unresolved_errors: usize,
    pub complete: bool,
    pub failure: Option<Failure>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CheckpointError {
    Folder {
        folder: u8,
        failure: Failure,
    },
    Search {
        unit: SearchUnit,
        failure: Failure,
    },
    Candidate {
        candidate: Candidate,
        failure: Failure,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ErrorSnapshot {
    entries: [Option<CheckpointError>; MAX_ERROR_DETAILS],
    count: usize,
}

impl ErrorSnapshot {
    pub(super) fn entries(&self) -> impl Iterator<Item = CheckpointError> + '_ {
        self.entries.iter().copied().flatten()
    }
    #[cfg(test)]
    pub(super) fn count(&self) -> usize {
        self.count
    }
    pub(super) fn omitted(&self) -> usize {
        self.count.saturating_sub(MAX_ERROR_DETAILS)
    }
}

pub(super) struct Reference<'a> {
    pub mailbox: &'a str,
    pub uid_validity: u32,
    pub uid: u32,
}

pub(super) struct State {
    namespace: [u8; 32],
    folders: Box<[Folder]>,
    seed: RefKey,
    options: Options,
    targets: Vec<Target>,
    pending: Vec<Pending>,
    graph: Graph,
    folder_failures: [Option<Failure>; MAX_FOLDERS],
    blocked_folders: [bool; MAX_FOLDERS],
    next_slot: usize,
    folder_probe: u8,
    completed_windows: [u64; MAX_FOLDERS],
    checkpoint: u64,
}

impl State {
    pub(super) fn new(
        namespace: [u8; 32],
        folders: Vec<Folder>,
        seed: RefKey,
        options: Options,
        facts: ThreadFacts,
        limit: usize,
    ) -> Result<Self, StateError> {
        if folders.is_empty()
            || folders.len() > MAX_FOLDERS
            || seed.uid == 0
            || !(1..=MAX_CANDIDATES).contains(&options.candidate_limit)
            || !(1..=MAX_RESULTS).contains(&options.result_limit)
            || options
                .max_body_length
                .is_some_and(|limit| limit > 512 * 1024)
        {
            return Err(StateError::InvalidScope);
        }
        for (index, folder) in folders.iter().enumerate() {
            if folder.name.is_empty()
                || folder.name.len() > 2048
                || folder.name.chars().count() > 512
                || folder.name.chars().any(char::is_control)
                || matches!(
                    folder.boundary,
                    Boundary::Frozen {
                        uid_validity: 0,
                        ..
                    }
                )
                || folders[..index].iter().any(|prior| {
                    prior.name == folder.name
                        || prior.name.eq_ignore_ascii_case("INBOX")
                            && folder.name.eq_ignore_ascii_case("INBOX")
                })
            {
                return Err(StateError::InvalidScope);
            }
        }
        if !matches!(folders.get(seed.folder as usize).map(|folder| folder.boundary), Some(Boundary::Frozen { high_uid, .. }) if seed.uid <= high_uid)
        {
            return Err(StateError::InvalidScope);
        }
        let scope_bytes = folders.len() * size_of::<Folder>()
            + folders
                .iter()
                .map(|folder| folder.name.len())
                .sum::<usize>();
        if size_of::<Self>() + scope_bytes > limit {
            return Err(StateError::Capacity);
        }
        let mut state = Self {
            namespace,
            folders: folders.into_boxed_slice(),
            seed,
            options,
            targets: Vec::new(),
            pending: Vec::new(),
            graph: Graph::default(),
            folder_failures: [None; MAX_FOLDERS],
            blocked_folders: [false; MAX_FOLDERS],
            next_slot: 0,
            folder_probe: 0,
            completed_windows: [0; MAX_FOLDERS],
            checkpoint: 0,
        };
        state.add_location(seed, facts, limit)?;
        Ok(state)
    }

    pub(super) fn namespace(&self) -> &[u8; 32] {
        &self.namespace
    }
    pub(super) fn version(&self) -> u16 {
        STATE_VERSION
    }
    pub(super) fn checkpoint(&self) -> u64 {
        self.checkpoint
    }
    pub(super) fn folders(&self) -> &[Folder] {
        &self.folders
    }
    pub(super) fn seed(&self) -> RefKey {
        self.seed
    }
    pub(super) fn options(&self) -> Options {
        self.options
    }
    pub(super) fn message_id(&self, target: usize) -> Option<&MessageId> {
        self.targets.get(target).map(|target| &target.id)
    }

    pub(super) fn reference(&self, key: RefKey) -> Option<Reference<'_>> {
        let folder = self.folders.get(key.folder as usize)?;
        let Boundary::Frozen {
            uid_validity,
            high_uid,
        } = folder.boundary
        else {
            return None;
        };
        (key.uid > 0 && key.uid <= high_uid).then_some(Reference {
            mailbox: &folder.name,
            uid_validity,
            uid: key.uid,
        })
    }

    pub(super) fn begin_page(&mut self) {
        self.blocked_folders.fill(false);
        for pending in &mut self.pending {
            pending.blocked = false;
        }
        for target in &mut self.targets {
            for position in target.positions.iter_mut().flatten() {
                position.blocked = false;
            }
        }
    }

    pub(super) fn folder_probe_start(&self) -> u8 {
        self.folder_probe
    }

    /// Rotate preflight independently of search progress so a slow first
    /// folder cannot consume the deadline of every resumed page.
    pub(super) fn rotate_folder_probe(&mut self) -> u8 {
        let start = self.folder_probe;
        self.folder_probe = (start + 1) % self.folders.len() as u8;
        start
    }

    pub(super) fn folder_failed(&mut self, folder: u8, failure: Failure) -> Result<(), StateError> {
        let index = folder as usize;
        self.folders.get(index).ok_or(StateError::InvalidWork)?;
        self.folder_failures[index] = Some(failure);
        self.blocked_folders[index] = true;
        Ok(())
    }

    /// The caller independently verifies the observed generation before clearing
    /// a transient folder failure. A missing initial boundary stays immutable.
    pub(super) fn folder_checked(
        &mut self,
        folder: u8,
        uid_validity: u32,
    ) -> Result<(), StateError> {
        let index = folder as usize;
        if !matches!(self.folders.get(index).map(|folder| folder.boundary), Some(Boundary::Frozen { uid_validity: frozen, .. }) if uid_validity == frozen)
        {
            return Err(StateError::InvalidScope);
        }
        self.folder_failures[index] = None;
        self.blocked_folders[index] = false;
        Ok(())
    }

    pub(super) fn next_work(&self) -> Option<Work> {
        if let Some(pending) = self.pending.iter().find(|pending| {
            !pending.blocked && !self.blocked_folders[pending.candidate.reference.folder as usize]
        }) {
            return Some(Work::Verify(pending.candidate));
        }
        let slots = self.targets.len() * self.folders.len() * SearchField::ALL.len();
        (0..slots).find_map(|offset| {
            let slot = (self.next_slot + offset) % slots;
            self.search_slot(slot).map(Work::Search)
        })
    }

    fn search_slot(&self, slot: usize) -> Option<SearchUnit> {
        let stride = self.folders.len() * SearchField::ALL.len();
        let target = slot / stride;
        let folder = (slot % stride) / SearchField::ALL.len();
        let field = slot % SearchField::ALL.len();
        let position = self.targets.get(target)?.positions[folder][field];
        if position.next_high == 0 || position.blocked || self.blocked_folders[folder] {
            return None;
        }
        Some(SearchUnit {
            target,
            folder: folder as u8,
            field: SearchField::ALL[field],
            start: position
                .next_high
                .saturating_sub(WINDOW_UIDS as u32 - 1)
                .max(1),
            end: position.next_high,
        })
    }

    fn search_index(&self, unit: SearchUnit) -> usize {
        (unit.target * self.folders.len() + unit.folder as usize) * SearchField::ALL.len()
            + unit.field as usize
    }

    /// Apply only a successful, fully consumed SEARCH. Reserving every hit and
    /// advancing the precise numeric interval form one pure state transition.
    pub(super) fn apply_search(
        &mut self,
        unit: SearchUnit,
        hits: &[u32],
        limit: usize,
    ) -> Result<(), StateError> {
        if self.next_work() != Some(Work::Search(unit)) {
            return Err(StateError::InvalidWork);
        }
        if hits.len() > WINDOW_UIDS
            || hits.iter().enumerate().any(|(index, uid)| {
                !(unit.start..=unit.end).contains(uid) || hits[..index].contains(uid)
            })
        {
            return Err(StateError::InvalidHits);
        }
        let mut sorted = [0; WINDOW_UIDS];
        sorted[..hits.len()].copy_from_slice(hits);
        sorted[..hits.len()].sort_unstable_by(|a, b| b.cmp(a));
        let refs = sorted[..hits.len()].iter().map(|uid| RefKey {
            folder: unit.folder,
            uid: *uid,
        });
        let count = refs
            .clone()
            .filter(|reference| !self.graph.contains(*reference))
            .count();
        self.check_growth(growth(&self.pending, count), limit)?;
        reserve(&mut self.pending, count)?;
        for reference in refs.filter(|reference| !self.graph.contains(*reference)) {
            let blocked = self.pending.iter().any(|pending| {
                pending.candidate.reference == reference
                    && pending.blocked
                    && pending.failure.is_some()
            });
            self.pending.push(Pending {
                candidate: Candidate {
                    target: unit.target,
                    field: unit.field,
                    reference,
                },
                failure: None,
                blocked,
            });
        }
        self.targets[unit.target].positions[unit.folder as usize][unit.field as usize] = Position {
            next_high: unit.start - 1,
            failure: None,
            blocked: false,
        };
        self.next_slot = self.search_index(unit) + 1;
        self.completed_windows[unit.folder as usize] += 1;
        self.checkpoint += 1;
        Ok(())
    }

    pub(super) fn search_failed(
        &mut self,
        unit: SearchUnit,
        failure: Failure,
    ) -> Result<(), StateError> {
        if self.next_work() != Some(Work::Search(unit)) {
            return Err(StateError::InvalidWork);
        }
        let position =
            &mut self.targets[unit.target].positions[unit.folder as usize][unit.field as usize];
        position.failure = Some(failure);
        position.blocked = true;
        self.next_slot = self.search_index(unit) + 1;
        Ok(())
    }

    pub(super) fn apply_facts(
        &mut self,
        candidate: Candidate,
        facts: ThreadFacts,
        limit: usize,
    ) -> Result<bool, StateError> {
        if self.next_work() != Some(Work::Verify(candidate)) {
            return Err(StateError::InvalidWork);
        }
        let target = &self.targets[candidate.target].id;
        let exact = match candidate.field {
            SearchField::MessageId => facts.id.as_ref() == Some(target),
            SearchField::References => facts.references.contains(target),
            SearchField::InReplyTo => facts.in_reply_to.contains(target),
        };
        if exact {
            self.add_location(candidate.reference, facts, limit)?;
            // Other successful SEARCHes may have queued this same reference;
            // all of its exact edges have now been visited.
            self.pending
                .retain(|pending| pending.candidate.reference != candidate.reference);
        } else {
            // Substring false positives do not enter the global graph seen set.
            self.pending
                .retain(|pending| pending.candidate != candidate);
        }
        self.checkpoint += 1;
        Ok(exact)
    }

    pub(super) fn candidate_failed(
        &mut self,
        candidate: Candidate,
        failure: Failure,
    ) -> Result<(), StateError> {
        if self.next_work() != Some(Work::Verify(candidate)) {
            return Err(StateError::InvalidWork);
        }
        let pending = self
            .pending
            .iter_mut()
            .find(|pending| pending.candidate == candidate)
            .ok_or(StateError::InvalidWork)?;
        pending.failure = Some(failure);
        // Different HEADER predicates may return this physical message again.
        // Keep their checkpoints, but do not retry its failed fetch this page.
        for pending in &mut self.pending {
            if pending.candidate.reference == candidate.reference {
                pending.blocked = true;
            }
        }
        Ok(())
    }

    fn add_location(
        &mut self,
        reference: RefKey,
        facts: ThreadFacts,
        limit: usize,
    ) -> Result<(), StateError> {
        let folder = &self.folders[reference.folder as usize];
        let variant = StableKey::for_reference(&self.namespace, folder, reference.uid);
        let entity = facts.id.as_ref().map_or(variant, |id| {
            StableKey::for_message_id(&self.namespace, id.as_str())
        });
        let evidence = facts.evidence;
        let add = self.graph.prepare(entity, evidence);
        let fact_count =
            facts.references.len() + facts.in_reply_to.len() + usize::from(facts.id.is_some());
        if fact_count > MAX_IDS_PER_MESSAGE {
            return Err(StateError::Capacity);
        }
        // Facts themselves are bounded by the per-message parser. Move their
        // IDs into a bounded scratch list before retaining any in the cursor.
        let mut new_ids = Vec::new();
        reserve(&mut new_ids, fact_count)?;
        let mut own_index = None;
        for (own, id) in facts.id.into_iter().map(|id| (true, id)).chain(
            facts
                .references
                .into_iter()
                .chain(facts.in_reply_to)
                .map(|id| (false, id)),
        ) {
            let index = if let Some(index) = self.targets.iter().position(|target| target.id == id)
            {
                index
            } else if let Some(index) = new_ids.iter().position(|existing| *existing == id) {
                self.targets.len() + index
            } else {
                let index = self.targets.len() + new_ids.len();
                new_ids.push(id);
                index
            };
            if own {
                own_index = Some(index);
            }
        }
        let added = self.graph.growth(&add)
            + growth(&self.targets, new_ids.len())
            + new_ids.iter().map(|id| id.as_str().len()).sum::<usize>()
            + new_ids.len() * self.folders.len() * size_of::<[Position; 3]>();
        self.check_growth(added, limit)?;
        self.graph.reserve(&add)?;
        reserve(&mut self.targets, new_ids.len())?;
        for id in new_ids {
            let positions = self
                .folders
                .iter()
                .map(|folder| {
                    let next_high = match folder.boundary {
                        Boundary::Frozen { high_uid, .. } => high_uid,
                        Boundary::Unavailable(_) => 0,
                    };
                    [Position {
                        next_high,
                        failure: None,
                        blocked: false,
                    }; 3]
                })
                .collect::<Box<[_]>>();
            self.targets.push(Target { id, positions });
        }
        self.graph
            .insert(add, entity, variant, own_index, reference, evidence);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn plan_delta(&self) -> Delivery {
        self.plan_delta_for_folders([true; MAX_FOLDERS])
    }

    pub(super) fn plan_delta_for_folders(&self, folders: [bool; MAX_FOLDERS]) -> Delivery {
        self.graph.plan_delivery(self.options.result_limit, folders)
    }
    pub(super) fn event(&self, item: EmitKey) -> Option<Event> {
        self.graph.event(item)
    }
    pub(super) fn mark_emitted(&mut self, delivery: &Delivery) -> Result<(), StateError> {
        self.graph.mark_emitted(delivery)
    }

    pub(super) fn coverage_after(&self, delivery: &Delivery) -> Result<Coverage, StateError> {
        let mut coverage = self.coverage();
        coverage.delivery_pending = self.graph.has_delivery_after(delivery)?;
        Ok(coverage)
    }

    pub(super) fn folder_errors(&self) -> impl Iterator<Item = (u8, Failure)> + '_ {
        self.folders
            .iter()
            .enumerate()
            .filter_map(|(index, folder)| {
                let error = match folder.boundary {
                    Boundary::Frozen { .. } => self.folder_failures[index],
                    Boundary::Unavailable(error) => Some(error),
                };
                error.map(|error| (index as u8, error))
            })
    }

    pub(super) fn search_errors(&self) -> impl Iterator<Item = (SearchUnit, Failure)> + '_ {
        self.targets.iter().enumerate().flat_map(|(target, item)| {
            item.positions
                .iter()
                .enumerate()
                .flat_map(move |(folder, positions)| {
                    positions
                        .iter()
                        .enumerate()
                        .filter_map(move |(field, position)| {
                            position.failure.map(|failure| {
                                (
                                    SearchUnit {
                                        target,
                                        folder: folder as u8,
                                        field: SearchField::ALL[field],
                                        start: position
                                            .next_high
                                            .saturating_sub(WINDOW_UIDS as u32 - 1)
                                            .max(1),
                                        end: position.next_high,
                                    },
                                    failure,
                                )
                            })
                        })
                })
        })
    }

    pub(super) fn candidate_errors(&self) -> impl Iterator<Item = (Candidate, Failure)> + '_ {
        self.pending
            .iter()
            .filter_map(|pending| pending.failure.map(|failure| (pending.candidate, failure)))
    }

    pub(super) fn error_snapshot(&self) -> ErrorSnapshot {
        let mut errors = self
            .folder_errors()
            .map(|(folder, failure)| CheckpointError::Folder { folder, failure })
            .chain(
                self.search_errors()
                    .map(|(unit, failure)| CheckpointError::Search { unit, failure }),
            )
            .chain(
                self.candidate_errors()
                    .map(|(candidate, failure)| CheckpointError::Candidate { candidate, failure }),
            );
        let entries = std::array::from_fn(|_| errors.next());
        let count = entries.iter().flatten().count() + errors.count();
        ErrorSnapshot { entries, count }
    }

    pub(super) fn coverage(&self) -> Coverage {
        let remaining_windows = self
            .targets
            .iter()
            .flat_map(|target| target.positions.iter().flatten())
            .map(|position| u64::from(position.next_high).div_ceil(WINDOW_UIDS as u64))
            .sum();
        let unresolved_errors = self.folder_errors().count()
            + self.search_errors().count()
            + self.candidate_errors().count();
        Coverage {
            completed_windows: self.completed_windows.iter().sum(),
            remaining_windows,
            pending_candidates: self.pending.len(),
            unresolved_errors,
            known_locations: self.graph.count(),
            graph_complete: remaining_windows == 0
                && self.pending.is_empty()
                && unresolved_errors == 0,
            delivery_pending: self.graph.has_delivery(),
        }
    }

    pub(super) fn folder_coverage(&self, folder: u8) -> Option<FolderCoverage> {
        let index = folder as usize;
        let snapshot = self.folders.get(index)?;
        let positions = || {
            self.targets
                .iter()
                .flat_map(|target| &target.positions[index])
        };
        let remaining_windows = positions()
            .map(|position| u64::from(position.next_high).div_ceil(WINDOW_UIDS as u64))
            .sum();
        let pending = || {
            self.pending
                .iter()
                .filter(|pending| pending.candidate.reference.folder == folder)
        };
        let pending_candidates = pending().count();
        let failure = match snapshot.boundary {
            Boundary::Unavailable(failure) => Some(failure),
            Boundary::Frozen { .. } => self.folder_failures[index],
        };
        let unresolved_errors = positions()
            .filter(|position| position.failure.is_some())
            .count()
            + pending()
                .filter(|pending| pending.failure.is_some())
                .count()
            + usize::from(failure.is_some());
        Some(FolderCoverage {
            completed_windows: self.completed_windows[index],
            remaining_windows,
            pending_candidates,
            unresolved_errors,
            complete: remaining_windows == 0 && pending_candidates == 0 && unresolved_errors == 0,
            failure,
        })
    }

    pub(super) fn owned_bytes(&self) -> usize {
        size_of::<Self>()
            + self.folders.len() * size_of::<Folder>()
            + self
                .folders
                .iter()
                .map(|folder| folder.name.len())
                .sum::<usize>()
            + capacity_bytes(&self.targets)
            + capacity_bytes(&self.pending)
            + self.graph.allocated_bytes()
            + self
                .targets
                .iter()
                .map(|target| {
                    target.id.as_str().len() + target.positions.len() * size_of::<[Position; 3]>()
                })
                .sum::<usize>()
    }

    fn check_growth(&self, additional: usize, limit: usize) -> Result<(), StateError> {
        self.owned_bytes()
            .checked_add(additional)
            .filter(|bytes| *bytes <= limit)
            .map(|_| ())
            .ok_or(StateError::Capacity)
    }
}

#[cfg(test)]
#[path = "tests/state.rs"]
mod tests;
