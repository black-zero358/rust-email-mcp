use super::*;
use crate::mail::conversation::graph::WINDOW_UIDS;
use crate::mail::conversation::store::persist::{Decoder, Encoder, Invalid, Result as CodecResult};

impl State {
    pub(in crate::mail::conversation) fn encode(
        &self,
        e: &mut Encoder<'_>,
        delivered: &Delivery,
    ) -> CodecResult<()> {
        e.bytes(&self.namespace)?;
        e.u8(self.folders.len() as u8)?;
        for (index, folder) in self.folders.iter().enumerate() {
            e.string(&folder.name)?;
            match folder.boundary {
                Boundary::Frozen {
                    uid_validity,
                    high_uid,
                } => {
                    e.u8(0)?;
                    e.u32(uid_validity)?;
                    e.u32(high_uid)?;
                    e.failure(self.folder_failures[index])?;
                }
                Boundary::Unavailable(failure) => e.failure(Some(failure))?,
            }
        }
        e.reference(self.seed)?;
        e.index(self.options.candidate_limit)?;
        e.index(self.options.result_limit)?;
        e.option(self.options.max_body_length, Encoder::index)?;
        e.boolean(self.options.trimmed)?;
        e.boolean(self.options.readable_html)?;
        e.index(self.next_slot)?;
        e.u8(self.folder_probe)?;
        e.u64(self.checkpoint)?;
        e.index(self.targets.len())?;
        for target in &self.targets {
            e.string(target.id.as_str())?;
            for positions in target.positions.iter() {
                for position in positions {
                    e.u32(position.next_high)?;
                    e.failure(position.failure)?;
                }
            }
        }
        e.index(self.pending.len())?;
        for pending in &self.pending {
            encode_candidate(e, pending.candidate)?;
            e.failure(pending.failure)?;
        }
        self.graph.encode(e, delivered)
    }

    pub(in crate::mail::conversation) fn decode(
        d: &mut Decoder<'_>,
        namespace: [u8; 32],
    ) -> CodecResult<Self> {
        d.charge(size_of::<Self>())?;
        if d.array::<32>()? != namespace {
            return Err(Invalid);
        }
        let count = d.u8()? as usize;
        if !(1..=MAX_FOLDERS).contains(&count) {
            return Err(Invalid);
        }
        let mut folders: Vec<Folder> = d.vector(count, 4)?;
        let mut folder_failures = [None; MAX_FOLDERS];
        let mut scratch = [0; 2048];
        for failure_slot in &mut folder_failures[..count] {
            let len = d.string(&mut scratch)?;
            let name = std::str::from_utf8(&scratch[..len]).map_err(|_| Invalid)?;
            if name.chars().count() > 512
                || name.chars().any(char::is_control)
                || folders.iter().any(|folder| {
                    folder.name.as_ref() == name
                        || folder.name.eq_ignore_ascii_case("INBOX")
                            && name.eq_ignore_ascii_case("INBOX")
                })
            {
                return Err(Invalid);
            }
            let boundary = if let Some(failure) = d.failure()? {
                Boundary::Unavailable(failure)
            } else {
                let uid_validity = d.u32()?;
                let high_uid = d.u32()?;
                if uid_validity == 0 {
                    return Err(Invalid);
                }
                *failure_slot = d.failure()?;
                Boundary::Frozen {
                    uid_validity,
                    high_uid,
                }
            };
            folders.push(Folder {
                name: name.into(),
                boundary,
            });
        }
        let seed = d.reference()?;
        if !valid_reference(&folders, seed) {
            return Err(Invalid);
        }
        let options = Options {
            candidate_limit: d.index()?,
            result_limit: d.index()?,
            max_body_length: d.option(Decoder::index)?,
            trimmed: d.boolean()?,
            readable_html: d.boolean()?,
        };
        if !(1..=MAX_CANDIDATES).contains(&options.candidate_limit)
            || !(1..=MAX_RESULTS).contains(&options.result_limit)
            || options
                .max_body_length
                .is_some_and(|limit| limit > 512 * 1024)
        {
            return Err(Invalid);
        }
        let next_slot = d.index()?;
        let folder_probe = d.u8()?;
        let checkpoint = d.u64()?;
        let targets_len = d.index()?;
        if folder_probe as usize >= count
            || next_slot > targets_len.checked_mul(count * 3).ok_or(Invalid)?
        {
            return Err(Invalid);
        }
        let mut targets = d.vector(targets_len, 3 + count * 15)?;
        let mut completed_windows = [0u64; MAX_FOLDERS];
        let mut id_scratch = [0; 998];
        for _ in 0..targets_len {
            let len = d.string(&mut id_scratch)?;
            let raw = &id_scratch[..len];
            let id = MessageId::parse(raw).map_err(|_| Invalid)?.ok_or(Invalid)?;
            if id.as_str().as_bytes() != raw {
                return Err(Invalid);
            }
            let mut positions = d.vector(count, 15)?;
            for (index, folder) in folders.iter().enumerate() {
                let mut group = [Position {
                    next_high: 0,
                    failure: None,
                    blocked: false,
                }; 3];
                for position in &mut group {
                    position.next_high = d.u32()?;
                    position.failure = d.failure()?;
                    let high = match folder.boundary {
                        Boundary::Frozen { high_uid, .. } => high_uid,
                        Boundary::Unavailable(_) => 0,
                    };
                    let advance = high.checked_sub(position.next_high).ok_or(Invalid)?;
                    if (position.next_high == 0 && position.failure.is_some())
                        || (position.next_high != 0 && !advance.is_multiple_of(WINDOW_UIDS as u32))
                    {
                        return Err(Invalid);
                    }
                    completed_windows[index] += advance.div_ceil(WINDOW_UIDS as u32) as u64;
                }
                positions.push(group);
            }
            targets.push(Target {
                id,
                positions: positions.into_boxed_slice(),
            });
        }
        let pending_len = d.index()?;
        let mut pending = d.vector(pending_len, 11)?;
        for _ in 0..pending_len {
            let candidate = decode_candidate(d)?;
            if !valid_reference(&folders, candidate.reference) {
                return Err(Invalid);
            }
            let Some(target) = targets.get(candidate.target) else {
                return Err(Invalid);
            };
            if target.positions[candidate.reference.folder as usize][candidate.field as usize]
                .next_high
                >= candidate.reference.uid
            {
                return Err(Invalid);
            }
            pending.push(Pending {
                candidate,
                failure: d.failure()?,
                blocked: false,
            });
        }
        let graph = Graph::decode(d, &namespace, &folders, seed, |target| {
            targets.get(target).map(|target| target.id.as_str())
        })?;
        d.with_unique_indices(&targets, |a, b| a.id.as_str().cmp(b.id.as_str()), |_| true)?;
        d.with_unique_indices(
            &pending,
            |a, b| candidate_key(a.candidate).cmp(&candidate_key(b.candidate)),
            |_| true,
        )?;
        graph.validate_unique(d, pending.iter().map(|item| item.candidate.reference))?;
        if checkpoint < completed_windows.iter().sum::<u64>() {
            return Err(Invalid);
        }
        Ok(Self {
            namespace,
            durable: true,
            folders: folders.into_boxed_slice(),
            seed,
            options,
            targets,
            pending,
            graph,
            folder_failures,
            blocked_folders: [false; MAX_FOLDERS],
            next_slot,
            folder_probe,
            completed_windows,
            checkpoint,
        })
    }

    pub(in crate::mail::conversation) fn validate_replay(
        &self,
        delivery: &Delivery,
    ) -> CodecResult<()> {
        if delivery.items().len() > self.options.result_limit {
            return Err(Invalid);
        }
        self.graph.validate_replay(delivery)
    }
}

pub(in crate::mail::conversation) fn valid_reference(
    folders: &[Folder],
    reference: RefKey,
) -> bool {
    reference.uid > 0
        && matches!(folders.get(reference.folder as usize).map(|folder| folder.boundary),
            Some(Boundary::Frozen { high_uid, .. }) if reference.uid <= high_uid)
}

fn encode_candidate(e: &mut Encoder<'_>, candidate: Candidate) -> CodecResult<()> {
    e.index(candidate.target)?;
    e.field(candidate.field)?;
    e.reference(candidate.reference)
}

fn decode_candidate(d: &mut Decoder<'_>) -> CodecResult<Candidate> {
    Ok(Candidate {
        target: d.index()?,
        field: d.field()?,
        reference: d.reference()?,
    })
}

fn candidate_key(candidate: Candidate) -> (usize, u8, RefKey) {
    (candidate.target, candidate.field as u8, candidate.reference)
}

impl Coverage {
    pub(in crate::mail::conversation) fn encode(self, e: &mut Encoder<'_>) -> CodecResult<()> {
        e.u64(self.completed_windows)?;
        e.u64(self.remaining_windows)?;
        e.index(self.pending_candidates)?;
        e.index(self.unresolved_errors)?;
        e.index(self.known_locations)?;
        e.boolean(self.graph_complete)?;
        e.boolean(self.delivery_pending)
    }

    pub(in crate::mail::conversation) fn decode(d: &mut Decoder<'_>) -> CodecResult<Self> {
        Ok(Self {
            completed_windows: d.u64()?,
            remaining_windows: d.u64()?,
            pending_candidates: d.index()?,
            unresolved_errors: d.index()?,
            known_locations: d.index()?,
            graph_complete: d.boolean()?,
            delivery_pending: d.boolean()?,
        })
    }
}

impl FolderCoverage {
    pub(in crate::mail::conversation) fn encode(self, e: &mut Encoder<'_>) -> CodecResult<()> {
        e.u64(self.completed_windows)?;
        e.u64(self.remaining_windows)?;
        e.index(self.pending_candidates)?;
        e.index(self.unresolved_errors)?;
        e.boolean(self.complete)?;
        e.failure(self.failure)
    }

    pub(in crate::mail::conversation) fn decode(d: &mut Decoder<'_>) -> CodecResult<Self> {
        Ok(Self {
            completed_windows: d.u64()?,
            remaining_windows: d.u64()?,
            pending_candidates: d.index()?,
            unresolved_errors: d.index()?,
            complete: d.boolean()?,
            failure: d.failure()?,
        })
    }
}

impl ErrorSnapshot {
    pub(in crate::mail::conversation) fn encode(&self, e: &mut Encoder<'_>) -> CodecResult<()> {
        e.index(self.count)?;
        for error in self.entries() {
            match error {
                CheckpointError::Folder { folder, failure } => {
                    e.u8(0)?;
                    e.u8(folder)?;
                    e.failure(Some(failure))?;
                }
                CheckpointError::Search { unit, failure } => {
                    e.u8(1)?;
                    e.index(unit.target)?;
                    e.field(unit.field)?;
                    e.u8(unit.folder)?;
                    e.u32(unit.start)?;
                    e.u32(unit.end)?;
                    e.failure(Some(failure))?;
                }
                CheckpointError::Candidate { candidate, failure } => {
                    e.u8(2)?;
                    encode_candidate(e, candidate)?;
                    e.failure(Some(failure))?;
                }
            }
        }
        Ok(())
    }

    pub(in crate::mail::conversation) fn decode(
        d: &mut Decoder<'_>,
        state: &State,
    ) -> CodecResult<Self> {
        let snapshot = state.error_snapshot();
        if d.index()? != snapshot.count {
            return Err(Invalid);
        }
        for expected in snapshot.entries() {
            let actual = match d.u8()? {
                0 => CheckpointError::Folder {
                    folder: d.u8()?,
                    failure: d.failure()?.ok_or(Invalid)?,
                },
                1 => CheckpointError::Search {
                    unit: SearchUnit {
                        target: d.index()?,
                        field: d.field()?,
                        folder: d.u8()?,
                        start: d.u32()?,
                        end: d.u32()?,
                    },
                    failure: d.failure()?.ok_or(Invalid)?,
                },
                2 => CheckpointError::Candidate {
                    candidate: decode_candidate(d)?,
                    failure: d.failure()?.ok_or(Invalid)?,
                },
                _ => return Err(Invalid),
            };
            if actual != expected {
                return Err(Invalid);
            }
        }
        Ok(snapshot)
    }
}
