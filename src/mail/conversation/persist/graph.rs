use super::*;
use crate::mail::conversation::{
    identity::ContentFingerprint,
    state::{State, persist::valid_reference},
    store::persist::{Decoder, Encoder, Invalid, Result as CodecResult},
};

impl IdentityStatus {
    fn encode(self, e: &mut Encoder<'_>) -> CodecResult<()> {
        e.u8(match self {
            Self::IdentityUnverified => 0,
            Self::Verified => 1,
            Self::Collision => 2,
        })
    }

    fn decode(d: &mut Decoder<'_>) -> CodecResult<Self> {
        match d.u8()? {
            0 => Ok(Self::IdentityUnverified),
            1 => Ok(Self::Verified),
            2 => Ok(Self::Collision),
            _ => Err(Invalid),
        }
    }
}

impl Graph {
    pub(in crate::mail::conversation) fn validate_unique(
        &self,
        d: &mut Decoder<'_>,
        mut pending: impl Iterator<Item = RefKey>,
    ) -> CodecResult<()> {
        d.with_unique_indices(&self.entities, |a, b| a.key.0.cmp(&b.key.0), |_| true)?;
        d.with_unique_indices(
            &self.locations,
            |a, b| a.reference.cmp(&b.reference),
            |indices| {
                pending.all(|reference| {
                    indices
                        .binary_search_by_key(&reference, |index| {
                            self.locations[*index as usize].reference
                        })
                        .is_err()
                })
            },
        )
    }

    pub(in crate::mail::conversation) fn encode(
        &self,
        e: &mut Encoder<'_>,
        delivered: &Delivery,
    ) -> CodecResult<()> {
        let items = delivered.items();
        e.index(self.entities.len())?;
        for (index, entity) in self.entities.iter().enumerate() {
            e.bytes(&entity.key.0)?;
            e.option(entity.message_id, Encoder::index)?;
            entity.status.encode(e)?;
            e.boolean(entity.emitted || items.iter().any(|item| item.entity == index))?;
        }
        e.index(self.variants.len())?;
        for (index, variant) in self.variants.iter().enumerate() {
            e.index(variant.entity)?;
            e.reference(variant.representative)?;
            e.bytes(&variant.evidence.headers)?;
            e.option(variant.evidence.content, |e, evidence| {
                e.u16(evidence.method)?;
                e.bytes(&evidence.digest)
            })?;
            e.boolean(variant.emitted || items.iter().any(|item| item.variant == index))?;
        }
        e.index(self.locations.len())?;
        for (index, location) in self.locations.iter().enumerate() {
            e.reference(location.reference)?;
            e.index(location.variant)?;
            e.boolean(location.emitted || items.iter().any(|item| item.location == index))?;
        }
        Ok(())
    }

    pub(in crate::mail::conversation) fn decode<'a>(
        d: &mut Decoder<'_>,
        namespace: &[u8; 32],
        folders: &[Folder],
        seed: RefKey,
        message_id: impl Fn(usize) -> Option<&'a str>,
    ) -> CodecResult<Self> {
        let entity_count = d.index()?;
        let mut entities = d.vector(entity_count, 35)?;
        for _ in 0..entity_count {
            let key = StableKey(d.array()?);
            let message = d.option(Decoder::index)?;
            if let Some(target) = message {
                let id = message_id(target).ok_or(Invalid)?;
                if key != StableKey::for_message_id(namespace, id) {
                    return Err(Invalid);
                }
            }
            entities.push(Entity {
                key,
                message_id: message,
                status: IdentityStatus::decode(d)?,
                emitted: d.boolean()?,
            });
        }
        let variant_count = d.index()?;
        let mut variants = d.vector(variant_count, 43)?;
        let mut next_entity = 0;
        for _ in 0..variant_count {
            let entity = d.index()?;
            let representative = d.reference()?;
            if entity > next_entity || !valid_reference(folders, representative) {
                return Err(Invalid);
            }
            let owner = entities.get(entity).ok_or(Invalid)?;
            let key = StableKey::for_reference(
                namespace,
                &folders[representative.folder as usize],
                representative.uid,
            );
            if owner.message_id.is_none() && (entity != next_entity || key != owner.key) {
                return Err(Invalid);
            }
            if entity == next_entity {
                next_entity += 1;
            }
            let evidence = Evidence {
                headers: d.array()?,
                content: d.option(|d| {
                    let method = d.u16()?;
                    if method != 1 {
                        return Err(Invalid);
                    }
                    Ok(ContentFingerprint {
                        method,
                        digest: d.array()?,
                    })
                })?,
            };
            let emitted = d.boolean()?;
            if (emitted && !owner.emitted)
                || (owner.message_id.is_none() && evidence.content.is_some())
            {
                return Err(Invalid);
            }
            variants.push(Variant {
                key,
                entity,
                representative,
                evidence,
                emitted,
            });
        }
        if next_entity != entities.len() {
            return Err(Invalid);
        }
        let location_count = d.index()?;
        let mut locations = d.vector(location_count, 10)?;
        let mut next_variant = 0;
        for index in 0..location_count {
            let reference = d.reference()?;
            let variant = d.index()?;
            let emitted = d.boolean()?;
            let owner = variants.get(variant).ok_or(Invalid)?;
            if !valid_reference(folders, reference)
                || variant > next_variant
                || (index == 0 && reference != seed)
                || (emitted && !owner.emitted)
            {
                return Err(Invalid);
            }
            if variant == next_variant {
                if owner.representative != reference {
                    return Err(Invalid);
                }
                next_variant += 1;
            } else if owner.evidence.content.is_none() {
                return Err(Invalid);
            }
            locations.push(Location {
                reference,
                variant,
                emitted,
            });
        }
        if locations.is_empty() || next_variant != variants.len() {
            return Err(Invalid);
        }

        // Check that every emitted flag is justified by a delivered location.
        // Clear and reconstruct the already validated flags without a second graph.
        let mut remaining_variants = variants.iter().filter(|v| v.emitted).count();
        for location in locations.iter().filter(|l| l.emitted) {
            if std::mem::take(&mut variants[location.variant].emitted) {
                remaining_variants -= 1;
            }
        }
        let mut remaining_entities = entities.iter().filter(|e| e.emitted).count();
        for location in locations.iter().filter(|l| l.emitted) {
            let variant = &mut variants[location.variant];
            variant.emitted = true;
            if std::mem::take(&mut entities[variant.entity].emitted) {
                remaining_entities -= 1;
            }
        }
        for variant in variants.iter().filter(|v| v.emitted) {
            entities[variant.entity].emitted = true;
        }
        if remaining_entities != 0 || remaining_variants != 0 {
            return Err(Invalid);
        }
        Ok(Self {
            entities,
            variants,
            locations,
        })
    }

    pub(in crate::mail::conversation) fn validate_replay(
        &self,
        delivery: &Delivery,
    ) -> CodecResult<()> {
        for (index, item) in delivery.items().iter().enumerate() {
            let event = self.event(*item).ok_or(Invalid)?;
            if !self.locations[item.location].emitted
                || !delivery.folders[event.location.folder as usize]
                || !delivery.folders[event.representative.folder as usize]
                || item.status != self.entities[item.entity].status
                || (index != 0 && item.location <= delivery.items()[index - 1].location)
            {
                return Err(Invalid);
            }
            let variant = &self.variants[item.variant];
            let expected_representative =
                if delivery.folders[variant.representative.folder as usize] {
                    variant.representative
                } else {
                    delivery.items()[..index]
                        .iter()
                        .find(|previous| previous.variant == item.variant)
                        .map_or(event.location, |previous| previous.representative)
                };
            if item.representative != expected_representative {
                return Err(Invalid);
            }
            // Any emitted location from this entity/variant outside this
            // replay batch means it had been delivered on an earlier page.
            let prior = self
                .locations
                .iter()
                .enumerate()
                .filter(|(location_index, location)| {
                    location.emitted
                        && !delivery.items()[index..]
                            .iter()
                            .any(|current| current.location == *location_index)
                });
            let new_variant = !prior
                .clone()
                .any(|(_, location)| location.variant == item.variant);
            let new_entity = !prior
                .clone()
                .any(|(_, location)| self.variants[location.variant].entity == item.entity);
            if item.new_entity != new_entity || item.new_variant != new_variant {
                return Err(Invalid);
            }
        }
        Ok(())
    }
}

impl Delivery {
    pub(in crate::mail::conversation) fn encode(&self, e: &mut Encoder<'_>) -> CodecResult<()> {
        e.u8(self.count as u8)?;
        for folder in self.folders {
            e.boolean(folder)?;
        }
        for item in self.items() {
            e.index(item.location)?;
            e.index(item.entity)?;
            e.index(item.variant)?;
            e.boolean(item.new_entity)?;
            e.boolean(item.new_variant)?;
            item.status.encode(e)?;
            e.reference(item.representative)?;
        }
        Ok(())
    }

    pub(in crate::mail::conversation) fn decode(
        d: &mut Decoder<'_>,
        state: &State,
    ) -> CodecResult<Self> {
        let count = d.u8()? as usize;
        if count > MAX_RESULTS {
            return Err(Invalid);
        }
        let mut result = Self::default();
        for folder in &mut result.folders {
            *folder = d.boolean()?;
        }
        for _ in 0..count {
            let item = EmitKey {
                location: d.index()?,
                entity: d.index()?,
                variant: d.index()?,
                new_entity: d.boolean()?,
                new_variant: d.boolean()?,
                status: IdentityStatus::decode(d)?,
                representative: d.reference()?,
            };
            state.reference(item.representative).ok_or(Invalid)?;
            result.push(item);
        }
        state.validate_replay(&result)?;
        Ok(result)
    }
}
