//! Bounded QRESYNC negotiation and completed UID-based evidence.
use super::{
    super::{InputLimits, mailbox_name},
    delta::{self, ChangedFlags, FetchFlags, ModSeq},
    scan::Selected,
};
use crate::{
    accounts::AccountKey,
    error::{MailError, Result},
};
use async_imap::{
    Session,
    imap_proto::{
        Capability, Response, ResponseCode, Status,
        bounded::{MAX_MODSEQ, MAX_SEARCH_UIDS, MAX_VANISHED_RANGES},
    },
    types::{CommandReadLimits, Mailbox},
};
use std::{
    fmt::{Debug, Write},
    ops::RangeInclusive,
};
use tokio::io::{AsyncRead, AsyncWrite};

const MAX_COMMAND_BYTES: usize = 8192;
const MAX_COMMAND_FRAMING: usize = 32 + 1 + 2; // Tag, space and CRLF.

#[cfg(test)]
mod tests;

pub(super) struct Enabled<S: AsyncRead + AsyncWrite + Unpin + Debug>(Session<S>);

pub(super) struct Opened<S: AsyncRead + AsyncWrite + Unpin + Debug> {
    pub(super) selected: Selected,
    pub(super) modseq: ModSeq,
    pub(super) evidence: Evidence,
    session: Session<S>,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(super) struct Evidence {
    pub(super) flags: Vec<ChangedFlags>,
    pub(super) earlier: Vec<RangeInclusive<u32>>,
    pub(super) live: Vec<RangeInclusive<u32>>,
    pub(super) highest_modseq: Option<u64>,
    pub(super) lowest_modseq: Option<u64>,
    pub(super) modseq_unavailable: bool,
}

impl Evidence {
    fn observe(&mut self, code: Option<&ResponseCode<'_>>) {
        match code {
            Some(ResponseCode::NoModSeq) => self.modseq_unavailable = true,
            Some(ResponseCode::HighestModSeq(value)) => {
                self.highest_modseq = Some(*value);
                self.lowest_modseq = Some(self.lowest_modseq.map_or(*value, |old| old.min(*value)));
            }
            _ => {}
        }
    }
}

pub(super) async fn enable<S>(mut session: Session<S>) -> Result<Enabled<S>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Debug,
{
    let mut enabled = None;
    delta::read_evidence_with_limits(
        &mut session, "ENABLE QRESYNC", CommandReadLimits::qresync(), |response| {
            match response {
                Response::Enabled(atoms) => {
                    let qresync = atoms.iter().any(|atom| matches!(atom, Capability::Atom(value) if value.eq_ignore_ascii_case("QRESYNC")));
                    if enabled.replace(qresync).is_some() { return Err(MailError::backend()); }
                }
                Response::Vanished { .. } | Response::Fetch(..) | Response::Expunge(_) => return Err(MailError::backend()),
                _ => {}
            }
            Ok(())
        },
    ).await?;
    if enabled != Some(true) {
        return Err(MailError::backend());
    }
    Ok(Enabled(session))
}

struct Known<'a> {
    uid_validity: u32,
    base: u64,
    uids: &'a [u32],
}

impl<S> Enabled<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Debug,
{
    pub(super) async fn examine_known(
        self,
        account: &AccountKey,
        mailbox: &str,
        uid_validity: u32,
        base: u64,
        known: &[u32],
    ) -> Result<Opened<S>> {
        if uid_validity == 0
            || !(1..=MAX_MODSEQ).contains(&base)
            || known.is_empty()
            || !valid_uids(known)
        {
            return Err(MailError::backend());
        }
        self.examine(
            account,
            mailbox,
            Some(Known {
                uid_validity,
                base,
                uids: known,
            }),
        )
        .await
    }

    pub(super) async fn examine_plain(
        self,
        account: &AccountKey,
        mailbox: &str,
    ) -> Result<Opened<S>> {
        self.examine(account, mailbox, None).await
    }

    async fn examine(
        self,
        account: &AccountKey,
        mailbox: &str,
        known: Option<Known<'_>>,
    ) -> Result<Opened<S>> {
        let Self(mut session) = self;
        let wire = mailbox_name(mailbox, InputLimits::Read)?;
        let wire = format!("\"{}\"", wire.replace('\\', "\\\\").replace('"', "\\\""));
        let command = if let Some(known) = &known {
            let set = uid_set(known.uids)?;
            format!(
                "EXAMINE {wire} (QRESYNC ({} {} {set}))",
                known.uid_validity, known.base
            )
        } else {
            format!("EXAMINE {wire} (CONDSTORE)")
        };
        validate_command(&command)?;
        let mut collector = Collector::new(
            known.as_ref().map_or(&[], |known| known.uids),
            known.as_ref().map_or(0, |known| known.base),
        );
        let mut state = Mailbox::default();
        let mut opening = None;
        delta::read_evidence_with_limits(
            &mut session,
            &command,
            CommandReadLimits::qresync(),
            |response| {
                match response {
                    Response::Done { outcome, .. }
                        if outcome.code != Some(ResponseCode::ReadOnly) =>
                    {
                        return Err(MailError::backend());
                    }
                    Response::Data {
                        status: Status::Ok,
                        outcome,
                    } => match outcome.code {
                        Some(ResponseCode::UidValidity(value))
                            if state.uid_validity.replace(value).is_none() => {}
                        Some(ResponseCode::UidNext(value))
                            if state.uid_next.replace(value).is_none() => {}
                        Some(
                            ResponseCode::UidValidity(_)
                            | ResponseCode::UidNext(_)
                            | ResponseCode::ReadWrite,
                        ) => return Err(MailError::backend()),
                        Some(ResponseCode::HighestModSeq(value)) => {
                            opening.get_or_insert(ModSeq::Highest(value));
                        }
                        Some(ResponseCode::NoModSeq) => {
                            opening.get_or_insert(ModSeq::Unavailable);
                        }
                        _ => {}
                    },
                    _ => {}
                }
                collector.observe(response)
            },
        )
        .await?;
        let selected = Selected::from_mailbox(account, mailbox, &state)?;
        let modseq = opening.ok_or_else(MailError::backend)?;
        if let Some(known) = &known {
            if known.uid_validity != selected.uid_validity {
                // The server selected a different generation. Its selection
                // may be observed, but no old-generation delta is usable.
                collector.discard_delta();
                collector.evidence.live.clear();
            } else if known
                .uids
                .last()
                .is_some_and(|uid| *uid >= selected.uid_next)
            {
                return Err(MailError::backend());
            }
        }
        if modseq == ModSeq::Unavailable || collector.evidence.modseq_unavailable {
            collector.discard_delta();
        } else if collector.saw_expunge {
            return Err(MailError::backend());
        }
        Ok(Opened {
            selected,
            modseq,
            evidence: collector.finish(),
            session,
        })
    }
}

impl<S> Opened<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Debug,
{
    pub(super) async fn collect(
        &mut self,
        known: &[u32],
        base: u64,
        mut fence_live_deletion: impl FnMut() -> bool,
    ) -> Result<Evidence> {
        if !matches!(self.modseq, ModSeq::Highest(_))
            || self.evidence.modseq_unavailable
            || !(1..=MAX_MODSEQ).contains(&base)
            || !valid_uids(known)
            || known
                .last()
                .is_some_and(|uid| *uid >= self.selected.uid_next)
        {
            return delta::validated(&mut self.session, Err(MailError::backend()));
        }
        if known.is_empty() {
            return Ok(Evidence::default());
        }
        let set = uid_set(known)?;
        let mut collector = Collector::new(known, base);
        let command = format!("UID FETCH {set} (UID FLAGS MODSEQ) (CHANGEDSINCE {base} VANISHED)");
        validate_command(&command)?;
        delta::read_evidence_with_limits(
            &mut self.session,
            &command,
            CommandReadLimits::qresync(),
            |response| {
                collector.observe(response)?;
                // The mailbox is already selected. Realtime deletions are
                // independent of this FETCH's completion and cancellation.
                if matches!(response, Response::Vanished { earlier: false, .. })
                    && !fence_live_deletion()
                {
                    return Err(MailError::backend());
                }
                Ok(())
            },
        )
        .await?;
        if collector.evidence.modseq_unavailable {
            collector.discard_delta();
        } else if collector.saw_expunge {
            return delta::validated(&mut self.session, Err(MailError::backend()));
        }
        Ok(collector.finish())
    }
}

fn valid_uids(known: &[u32]) -> bool {
    known.len() <= MAX_SEARCH_UIDS
        && known.first() != Some(&0)
        && known.windows(2).all(|pair| pair[0] < pair[1])
}

fn validate_command(command: &str) -> Result<()> {
    if command.len() > MAX_COMMAND_BYTES - MAX_COMMAND_FRAMING {
        return Err(MailError::backend());
    }
    Ok(())
}

fn uid_set(known: &[u32]) -> Result<String> {
    let mut set = String::new();
    let mut ranges = known
        .chunk_by(|a, b| a.checked_add(1) == Some(*b))
        .peekable();
    while let Some(range) = ranges.next() {
        let first = range[0];
        let last = range[range.len() - 1];
        if first == last {
            write!(set, "{first}")
        } else {
            write!(set, "{first}:{last}")
        }
        .map_err(|_| MailError::backend())?;
        if ranges.peek().is_some() {
            set.push(',');
        }
    }
    Ok(set)
}

struct Collector<'a> {
    requested: &'a [u32],
    base: u64,
    flags: Vec<Option<ChangedFlags>>,
    evidence: Evidence,
    flag_atoms: usize,
    range_count: usize,
    saw_fetch: bool,
    saw_expunge: bool,
}

impl<'a> Collector<'a> {
    fn new(requested: &'a [u32], base: u64) -> Self {
        Self {
            requested,
            base,
            flags: (0..requested.len()).map(|_| None).collect(),
            evidence: Evidence::default(),
            flag_atoms: 0,
            range_count: 0,
            saw_fetch: false,
            saw_expunge: false,
        }
    }

    fn observe(&mut self, response: &Response<'_>) -> Result<()> {
        match response {
            Response::Data {
                status: Status::Ok,
                outcome,
            }
            | Response::Done { outcome, .. } => self.evidence.observe(outcome.code.as_ref()),
            Response::Enabled(_) => return Err(MailError::backend()),
            Response::Expunge(_) => self.saw_expunge = true,
            Response::Vanished { earlier, uids } => self.vanished(*earlier, uids)?,
            Response::Fetch(sequence, attributes) => {
                self.saw_fetch = true;
                let change = FetchFlags::read_optional(*sequence, attributes)?;
                if self
                    .evidence
                    .earlier
                    .iter()
                    .chain(&self.evidence.live)
                    .any(|range| range.contains(&change.uid))
                {
                    return Err(MailError::backend());
                }
                if let Ok(index) = self.requested.binary_search(&change.uid) {
                    change.merge(&mut self.flags[index], self.base, &mut self.flag_atoms)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn vanished(&mut self, earlier: bool, ranges: &[RangeInclusive<u32>]) -> Result<()> {
        self.range_count += ranges.len();
        if self.range_count > MAX_VANISHED_RANGES
            || (earlier && (self.saw_fetch || self.requested.is_empty()))
        {
            return Err(MailError::backend());
        }
        let (updates, other) = if earlier {
            (&mut self.evidence.earlier, &self.evidence.live)
        } else {
            (&mut self.evidence.live, &self.evidence.earlier)
        };
        for range in ranges {
            if other
                .iter()
                .any(|opposite| range.start() <= opposite.end() && opposite.start() <= range.end())
            {
                return Err(MailError::backend());
            }
            let low = self.requested.partition_point(|uid| uid < range.start());
            let high = self.requested.partition_point(|uid| uid <= range.end());
            if earlier {
                let length = u64::from(*range.end()) - u64::from(*range.start()) + 1;
                if (high - low) as u64 != length {
                    return Err(MailError::backend());
                }
            } else {
                // A realtime removal after a FETCH is the final UID state.
                self.flags[low..high].fill_with(|| None);
            }
        }
        updates.extend_from_slice(ranges);
        normalize(updates);
        Ok(())
    }

    fn discard_delta(&mut self) {
        self.flags.clear();
        self.evidence.earlier.clear();
    }

    fn finish(mut self) -> Evidence {
        self.evidence.flags = self.flags.into_iter().flatten().collect();
        self.evidence
    }
}

fn normalize(ranges: &mut Vec<RangeInclusive<u32>>) {
    ranges.sort_unstable_by_key(|range| (*range.start(), *range.end()));
    if ranges.is_empty() {
        return;
    }
    let mut last = 0;
    for index in 1..ranges.len() {
        if *ranges[index].start() <= ranges[last].end().saturating_add(1) {
            ranges[last] = *ranges[last].start()..=(*ranges[last].end()).max(*ranges[index].end());
        } else {
            last += 1;
            ranges.swap(last, index);
        }
    }
    ranges.truncate(last + 1);
}
