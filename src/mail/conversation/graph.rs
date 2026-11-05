//! Conversation graph records and metadata-only delivery deltas.
use super::identity::{Comparison, Evidence};
use sha2::{Digest, Sha256};
use std::mem::size_of;

pub(super) const MAX_FOLDERS: usize = 20;
pub(super) const WINDOW_UIDS: usize = 200;
pub(super) const MAX_RESULTS: usize = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StateError {
    Capacity,
    InvalidScope,
    InvalidWork,
    InvalidHits,
    InvalidDelivery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Failure {
    Backend,
    Protocol,
    Timeout,
    Limit,
    Missing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Boundary {
    Frozen { uid_validity: u32, high_uid: u32 },
    Unavailable(Failure),
}

pub(super) struct Folder {
    pub name: Box<str>,
    pub boundary: Boundary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SearchField {
    MessageId,
    References,
    InReplyTo,
}

impl SearchField {
    pub(super) const ALL: [Self; 3] = [Self::MessageId, Self::References, Self::InReplyTo];

    pub(super) fn wire_name(self) -> &'static str {
        match self {
            Self::MessageId => "Message-ID",
            Self::References => "References",
            Self::InReplyTo => "In-Reply-To",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct RefKey {
    pub folder: u8,
    pub uid: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SearchUnit {
    pub target: usize,
    pub folder: u8,
    pub field: SearchField,
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Candidate {
    pub target: usize,
    pub field: SearchField,
    pub reference: RefKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Work {
    Search(SearchUnit),
    Verify(Candidate),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum IdentityStatus {
    #[default]
    IdentityUnverified,
    Verified,
    Collision,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct StableKey(pub [u8; 32]);

impl StableKey {
    pub(super) fn for_reference(namespace: &[u8; 32], folder: &Folder, uid: u32) -> Self {
        let Boundary::Frozen { uid_validity, .. } = folder.boundary else {
            unreachable!("only references in a frozen mailbox have identity");
        };
        let mut hash = Sha256::new();
        hash.update(b"conversation-reference-v1\0");
        hash.update(namespace);
        let name = if folder.name.eq_ignore_ascii_case("INBOX") {
            "INBOX"
        } else {
            &folder.name
        };
        hash.update((name.len() as u32).to_be_bytes());
        hash.update(name.as_bytes());
        hash.update(uid_validity.to_be_bytes());
        hash.update(uid.to_be_bytes());
        Self(hash.finalize().into())
    }

    pub(super) fn for_message_id(namespace: &[u8; 32], id: &str) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"conversation-message-id-v1\0");
        hash.update(namespace);
        hash.update(id.as_bytes());
        Self(hash.finalize().into())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct EmitKey {
    pub location: usize,
    pub entity: usize,
    pub variant: usize,
    pub new_entity: bool,
    pub new_variant: bool,
    pub status: IdentityStatus,
    representative: RefKey,
}

/// Fixed-size metadata-only delivery journal. A caller may fit a prefix to the
/// wire budget and commit only that prefix. Replays retain the original status.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct Delivery {
    items: [EmitKey; MAX_RESULTS],
    count: usize,
    folders: [bool; MAX_FOLDERS],
}

impl Delivery {
    pub(super) fn items(&self) -> &[EmitKey] {
        &self.items[..self.count]
    }

    pub(super) fn truncate(&mut self, count: usize) {
        self.count = self.count.min(count);
    }

    fn push(&mut self, key: EmitKey) {
        self.items[self.count] = key;
        self.count += 1;
    }
}

pub(super) struct Event {
    pub entity: StableKey,
    pub variant: StableKey,
    pub message_id: Option<usize>,
    pub location: RefKey,
    pub representative: RefKey,
    pub new_entity: bool,
    pub new_variant: bool,
    pub status: IdentityStatus,
}

struct Entity {
    key: StableKey,
    message_id: Option<usize>,
    status: IdentityStatus,
    emitted: bool,
}

struct Variant {
    key: StableKey,
    entity: usize,
    representative: RefKey,
    evidence: Evidence,
    emitted: bool,
}

struct Location {
    reference: RefKey,
    variant: usize,
    emitted: bool,
}

#[derive(Default)]
pub(super) struct Graph {
    entities: Vec<Entity>,
    variants: Vec<Variant>,
    locations: Vec<Location>,
}

pub(super) struct Addition {
    entity: usize,
    variant: usize,
    new_entity: bool,
    new_variant: bool,
    status: IdentityStatus,
}

impl Graph {
    pub(super) fn contains(&self, reference: RefKey) -> bool {
        self.locations
            .iter()
            .any(|entry| entry.reference == reference)
    }

    pub(super) fn count(&self) -> usize {
        self.locations.len()
    }

    pub(super) fn prepare(&self, key: StableKey, evidence: Evidence) -> Addition {
        let initial_status = if evidence.content.is_some() {
            IdentityStatus::Verified
        } else {
            IdentityStatus::IdentityUnverified
        };
        let Some(entity) = self.entities.iter().position(|entity| entity.key == key) else {
            return Addition {
                entity: self.entities.len(),
                variant: self.variants.len(),
                new_entity: true,
                new_variant: true,
                status: initial_status,
            };
        };
        let mut status = self.entities[entity].status;
        let mut matching = None;
        for (index, variant) in self
            .variants
            .iter()
            .enumerate()
            .filter(|(_, variant)| variant.entity == entity)
        {
            match evidence.compare(variant.evidence) {
                Comparison::Match => matching = Some(index),
                Comparison::Conflict => status = IdentityStatus::Collision,
                Comparison::Unverified if status != IdentityStatus::Collision => {
                    status = IdentityStatus::IdentityUnverified
                }
                Comparison::Unverified => {}
            }
        }
        Addition {
            entity,
            variant: matching.unwrap_or(self.variants.len()),
            new_entity: false,
            new_variant: matching.is_none(),
            status,
        }
    }

    pub(super) fn growth(&self, add: &Addition) -> usize {
        growth(&self.entities, usize::from(add.new_entity))
            + growth(&self.variants, usize::from(add.new_variant))
            + growth(&self.locations, 1)
    }

    pub(super) fn reserve(&mut self, add: &Addition) -> Result<(), StateError> {
        reserve(&mut self.entities, usize::from(add.new_entity))?;
        reserve(&mut self.variants, usize::from(add.new_variant))?;
        reserve(&mut self.locations, 1)
    }

    pub(super) fn insert(
        &mut self,
        add: Addition,
        entity: StableKey,
        variant: StableKey,
        message_id: Option<usize>,
        reference: RefKey,
        evidence: Evidence,
    ) {
        if add.new_entity {
            self.entities.push(Entity {
                key: entity,
                message_id,
                status: add.status,
                emitted: false,
            });
        } else {
            self.entities[add.entity].status = add.status;
        }
        if add.new_variant {
            self.variants.push(Variant {
                key: variant,
                entity: add.entity,
                representative: reference,
                evidence,
                emitted: false,
            });
        }
        self.locations.push(Location {
            reference,
            variant: add.variant,
            emitted: false,
        });
    }

    pub(super) fn allocated_bytes(&self) -> usize {
        capacity_bytes(&self.entities)
            + capacity_bytes(&self.variants)
            + capacity_bytes(&self.locations)
    }

    pub(super) fn plan_delivery(&self, limit: usize, folders: [bool; MAX_FOLDERS]) -> Delivery {
        let mut delivery = Delivery {
            folders,
            ..Delivery::default()
        };
        for (index, location) in self
            .locations
            .iter()
            .enumerate()
            .filter(|(_, location)| {
                !location.emitted && folders[location.reference.folder as usize]
            })
            .take(limit.min(MAX_RESULTS))
        {
            let variant = &self.variants[location.variant];
            let entity = &self.entities[variant.entity];
            let new_entity = !entity.emitted
                && !delivery
                    .items()
                    .iter()
                    .any(|item| item.entity == variant.entity);
            let earlier = delivery
                .items()
                .iter()
                .find(|item| item.variant == location.variant);
            let new_variant = !variant.emitted && earlier.is_none();
            let representative = if folders[variant.representative.folder as usize] {
                variant.representative
            } else {
                // Only positively verified copies share a variant. A checked
                // location can represent it when its original source is blocked.
                earlier.map_or(location.reference, |item| item.representative)
            };
            delivery.push(EmitKey {
                location: index,
                entity: variant.entity,
                variant: location.variant,
                new_entity,
                new_variant,
                status: entity.status,
                representative,
            });
        }
        delivery
    }

    pub(super) fn event(&self, key: EmitKey) -> Option<Event> {
        let entity = self.entities.get(key.entity)?;
        let variant = self.variants.get(key.variant)?;
        let location = self.locations.get(key.location)?;
        if location.variant != key.variant || variant.entity != key.entity {
            return None;
        }
        Some(Event {
            entity: entity.key,
            variant: variant.key,
            message_id: entity.message_id,
            location: location.reference,
            representative: key.representative,
            new_entity: key.new_entity,
            new_variant: key.new_variant,
            status: key.status,
        })
    }

    pub(super) fn mark_emitted(&mut self, delivery: &Delivery) -> Result<(), StateError> {
        self.has_delivery_after(delivery)?;
        for item in delivery.items() {
            self.locations[item.location].emitted = true;
            self.variants[item.variant].emitted = true;
            self.entities[item.entity].emitted = true;
        }
        Ok(())
    }

    pub(super) fn has_delivery(&self) -> bool {
        self.locations.iter().any(|location| !location.emitted)
    }

    pub(super) fn has_delivery_after(&self, delivery: &Delivery) -> Result<bool, StateError> {
        if self.plan_delivery(delivery.count, delivery.folders).items() != delivery.items() {
            return Err(StateError::InvalidDelivery);
        }
        Ok(self.locations.iter().enumerate().any(|(index, location)| {
            !location.emitted && !delivery.items().iter().any(|item| item.location == index)
        }))
    }
}

pub(super) fn capacity_bytes<T>(values: &Vec<T>) -> usize {
    values.capacity() * size_of::<T>()
}

pub(super) fn growth<T>(values: &Vec<T>, count: usize) -> usize {
    values
        .len()
        .saturating_add(count)
        .saturating_sub(values.capacity())
        * size_of::<T>()
}

pub(super) fn reserve<T>(values: &mut Vec<T>, count: usize) -> Result<(), StateError> {
    values
        .try_reserve_exact(count)
        .map_err(|_| StateError::Capacity)
}

#[path = "persist/graph.rs"]
mod persist;
