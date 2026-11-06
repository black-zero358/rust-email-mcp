//! Read-only CONDSTORE command evidence; presence belongs to rev1 reconciliation.
use super::{
    super::{InputLimits, mailbox_name, map_read_imap_error, quote},
    scan::Selected,
};
use crate::{
    accounts::AccountKey,
    error::{MailError, Result},
};
use async_imap::{
    Session,
    imap_proto::{
        AttributeValue, Capability, Response, ResponseCode, Status,
        bounded::{MAX_MODSEQ, MAX_SEARCH_UIDS},
    },
    types::{CommandReadLimits, Flag, Mailbox},
};
use std::{borrow::Cow, fmt::Debug, fmt::Write};
use tokio::io::{AsyncRead, AsyncWrite};

#[cfg(test)]
mod tests;

const FLAG_ATOMS_PER_COMMAND: usize = 4096;
const FLAG_BYTES_PER_UID: usize = 3072;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ModSeq {
    Highest(u64),
    Unavailable,
}

impl ModSeq {
    pub(super) fn observe_minimum(previous: &mut Option<Self>, code: Option<&ResponseCode<'_>>) {
        match code {
            Some(ResponseCode::NoModSeq) => *previous = Some(Self::Unavailable),
            Some(ResponseCode::HighestModSeq(value)) => match previous {
                Some(Self::Unavailable) => {}
                Some(Self::Highest(lowest)) => *lowest = (*lowest).min(*value),
                None => *previous = Some(Self::Highest(*value)),
            },
            _ => {}
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct ChangedFlags {
    pub(super) uid: u32,
    pub(super) modseq: u64,
    pub(super) flags: Vec<String>,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(super) struct Changed {
    pub(super) flags: Vec<ChangedFlags>,
    /// Last explicit HIGHESTMODSEQ in this command's completed response.
    pub(super) highest_modseq: Option<u64>,
    /// Lowest explicit value, including values preceding the final outcome.
    pub(super) lowest_modseq: Option<u64>,
    pub(super) modseq_unavailable: bool,
}

pub(super) async fn supports_condstore<S>(session: &mut Session<S>) -> Result<bool>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Debug,
{
    Ok(capabilities(session).await?.condstore)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Capabilities {
    pub(super) condstore: bool,
    pub(super) qresync: bool,
    pub(super) enable: bool,
}

pub(super) async fn capabilities<S>(session: &mut Session<S>) -> Result<Capabilities>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Debug,
{
    let mut supported = None;
    read_evidence(session, "CAPABILITY", |response| {
        let Response::Capabilities(capabilities) = response else {
            return Ok(());
        };
        let mut imap = false;
        let mut supported_extensions = Capabilities::default();
        for capability in capabilities {
            match capability {
                Capability::Imap4rev1 => imap = true,
                Capability::Atom(atom) => {
                    imap |= atom.eq_ignore_ascii_case("IMAP4rev2");
                    supported_extensions.condstore |= atom.eq_ignore_ascii_case("CONDSTORE");
                    supported_extensions.qresync |= atom.eq_ignore_ascii_case("QRESYNC");
                    supported_extensions.enable |= atom.eq_ignore_ascii_case("ENABLE");
                }
                _ => {}
            }
        }
        supported_extensions.condstore |= supported_extensions.qresync;
        if !imap || supported.replace(supported_extensions).is_some() {
            return Err(MailError::backend());
        }
        Ok(())
    })
    .await?;
    validated(session, supported.ok_or_else(MailError::backend))
}

pub(super) async fn examine<S>(
    session: &mut Session<S>,
    account: &AccountKey,
    mailbox: &str,
) -> Result<(Selected, ModSeq)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Debug,
{
    let wire = quote(&mailbox_name(mailbox, InputLimits::Read)?)?;
    let mut state = Mailbox::default();
    let mut modseq = None;
    read_evidence(
        session,
        &format!("EXAMINE {wire} (CONDSTORE)"),
        |response| {
            match response {
                Response::Done { outcome, .. } if outcome.code != Some(ResponseCode::ReadOnly) => {
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
                    Some(ResponseCode::HighestModSeq(value))
                        if modseq.replace(ModSeq::Highest(value)).is_none() => {}
                    Some(ResponseCode::NoModSeq)
                        if modseq.replace(ModSeq::Unavailable).is_none() => {}
                    Some(
                        ResponseCode::UidValidity(_)
                        | ResponseCode::UidNext(_)
                        | ResponseCode::HighestModSeq(_)
                        | ResponseCode::NoModSeq
                        | ResponseCode::ReadWrite,
                    ) => return Err(MailError::backend()),
                    _ => {}
                },
                _ => {}
            }
            Ok(())
        },
    )
    .await?;
    validated(
        session,
        Selected::from_mailbox(account, mailbox, &state)
            .and_then(|selected| Ok((selected, modseq.ok_or_else(MailError::backend)?))),
    )
}

pub(super) async fn collect<S>(
    session: &mut Session<S>,
    selected: &Selected,
    expected: &[u32],
    base: u64,
) -> Result<Changed>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Debug,
{
    if !(1..=MAX_MODSEQ).contains(&base)
        || expected.len() > MAX_SEARCH_UIDS
        || expected.first().is_some_and(|uid| *uid == 0)
        || expected.last().is_some_and(|uid| *uid >= selected.uid_next)
        || expected.windows(2).any(|uids| uids[0] >= uids[1])
    {
        return validated(session, Err(MailError::backend()));
    }
    let Some(first) = expected.first() else {
        return Ok(Changed::default());
    };
    let mut set = first.to_string();
    for uid in &expected[1..] {
        write!(set, ",{uid}").map_err(|_| MailError::backend())?;
    }
    let mut items: Vec<Option<ChangedFlags>> = (0..expected.len()).map(|_| None).collect();
    let mut highest_modseq = None;
    let mut lowest_modseq = None;
    let mut modseq_unavailable = false;
    let mut flag_atoms = 0;
    read_evidence(
        session,
        &format!("UID FETCH {set} (UID FLAGS MODSEQ) (CHANGEDSINCE {base})"),
        |response| {
            match response {
                Response::Data {
                    status: Status::Ok,
                    outcome,
                }
                | Response::Done { outcome, .. } => match outcome.code {
                    Some(ResponseCode::HighestModSeq(value)) => {
                        highest_modseq = Some(value);
                        lowest_modseq =
                            Some(lowest_modseq.map_or(value, |previous: u64| previous.min(value)));
                    }
                    Some(ResponseCode::NoModSeq) => modseq_unavailable = true,
                    _ => {}
                },
                Response::Fetch(sequence, attributes) => {
                    let change = FetchFlags::read(*sequence, attributes)?;
                    // Untagged updates outside the requested UID set carry no evidence for it.
                    let Ok(index) = expected.binary_search(&change.uid) else {
                        return Ok(());
                    };
                    change.merge(&mut items[index], base, &mut flag_atoms)?;
                }
                _ => {}
            }
            Ok(())
        },
    )
    .await?;
    Ok(Changed {
        flags: items.into_iter().flatten().collect(),
        highest_modseq,
        lowest_modseq,
        modseq_unavailable,
    })
}

pub(super) struct FetchFlags<'a, 'wire> {
    pub(super) uid: u32,
    modseq: u64,
    flags: Option<&'a [Cow<'wire, str>]>,
}

impl<'a, 'wire> FetchFlags<'a, 'wire> {
    pub(super) fn read(sequence: u32, attributes: &'a [AttributeValue<'wire>]) -> Result<Self> {
        let change = Self::read_optional(sequence, attributes)?;
        if change.flags.is_none() {
            return Err(MailError::backend());
        }
        Ok(change)
    }

    pub(super) fn read_optional(
        sequence: u32,
        attributes: &'a [AttributeValue<'wire>],
    ) -> Result<Self> {
        if sequence == 0 {
            return Err(MailError::backend());
        }
        let mut uid = None;
        let mut modseq = None;
        let mut flags = None;
        for attribute in attributes {
            match attribute {
                AttributeValue::Uid(value) if *value != 0 && uid.replace(*value).is_none() => {}
                AttributeValue::ModSeq(value) if modseq.replace(*value).is_none() => {}
                AttributeValue::Flags(value) if flags.replace(value).is_none() => {}
                _ => return Err(MailError::backend()),
            }
        }
        Ok(Self {
            uid: uid.ok_or_else(MailError::backend)?,
            modseq: modseq.ok_or_else(MailError::backend)?,
            flags: flags.map(Vec::as_slice),
        })
    }

    pub(super) fn merge(
        self,
        item: &mut Option<ChangedFlags>,
        base: u64,
        atoms: &mut usize,
    ) -> Result<()> {
        let Self { uid, modseq, flags } = self;
        let flags = flags.ok_or_else(MailError::backend)?;
        if modseq <= base || modseq > MAX_MODSEQ {
            return Err(MailError::backend());
        }
        *atoms += flags.len();
        if *atoms > FLAG_ATOMS_PER_COMMAND {
            return Err(MailError::backend());
        }
        let mut bytes = 0;
        let mut values = Vec::with_capacity(flags.len());
        for flag in flags {
            let value = format!("{:?}", Flag::from(flag.as_ref()));
            bytes += value.len();
            if value.len() > 256 || bytes > FLAG_BYTES_PER_UID {
                return Err(MailError::backend());
            }
            values.push(value);
        }
        values.sort_unstable();
        values.dedup();
        if let Some(previous) = item {
            if previous.modseq == modseq && previous.flags != values {
                return Err(MailError::backend());
            }
            if previous.modseq >= modseq {
                return Ok(());
            }
        }
        *item = Some(ChangedFlags {
            uid,
            modseq,
            flags: values,
        });
        Ok(())
    }
}

async fn read_evidence<S>(
    session: &mut Session<S>,
    command: &str,
    visit: impl FnMut(&Response<'_>) -> Result<()>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Debug,
{
    read_evidence_with_limits(session, command, CommandReadLimits::condstore(), visit).await
}

pub(super) async fn read_evidence_with_limits<S>(
    session: &mut Session<S>,
    command: &str,
    limits: CommandReadLimits,
    mut visit: impl FnMut(&Response<'_>) -> Result<()>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Debug,
{
    let result = async {
        let tag = session
            .run_command_with_limits(command, limits)
            .await
            .map_err(map_read_imap_error)?;
        loop {
            let response = session
                .read_response()
                .await
                .map_err(|error| map_read_imap_error(error.into()))?
                .ok_or_else(MailError::backend)?;
            let response = response.parsed();
            match response {
                Response::Done {
                    tag: received,
                    status: Status::Ok,
                    ..
                } if received == &tag => return visit(response),
                Response::Done { .. }
                | Response::Continue(_)
                | Response::Data {
                    status: Status::Bye,
                    ..
                } => {
                    return Err(MailError::backend());
                }
                _ => visit(response)?,
            }
        }
    }
    .await;
    validated(session, result)
}

pub(super) fn validated<S, T>(session: &mut Session<S>, result: Result<T>) -> Result<T>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Debug,
{
    if result.is_err() {
        session.abort_bounded_command();
    }
    result
}
