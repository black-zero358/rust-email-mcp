use std::collections::HashSet;

use async_channel as channel;
use futures_util::stream::Stream;
use futures_util::{StreamExt as _, TryStreamExt as _, io};
use imap_proto::rfc5464::Metadata;
use imap_proto::{self, MailboxDatum, RequestId, Response};

use crate::error::{Error, Result};
use crate::types::ResponseData;
use crate::types::*;

pub(crate) fn parse_names<T: Stream<Item = io::Result<ResponseData>> + Unpin + Send>(
    stream: &mut T,
    unsolicited: channel::Sender<UnsolicitedResponse>,
    command_tag: RequestId,
) -> impl Stream<Item = Result<Name>> + '_ + Send + Unpin {
    use futures_util::{FutureExt, StreamExt};

    StreamExt::filter_map(checked_responses(stream, command_tag), move |resp| {
        let unsolicited = unsolicited.clone();
        async move {
            match resp {
                Ok(resp) => match resp.parsed() {
                    Response::MailboxData(MailboxDatum::List { .. }) => {
                        let name = Name::from_mailbox_data(resp);
                        Some(Ok(name))
                    }
                    _ => {
                        handle_unilateral(resp, unsolicited);
                        None
                    }
                },
                Err(err) => Some(Err(err.into())),
            }
        }
        .boxed()
    })
}

// Every streaming parser must see command rejection and unexpected EOF. A
// normally exhausted data stream is not proof that the command succeeded.
pub(crate) fn checked_responses<T: Stream<Item = io::Result<ResponseData>> + Unpin + Send>(
    stream: &mut T,
    command_tag: RequestId,
) -> impl Stream<Item = Result<ResponseData>> + Unpin + Send {
    futures_util::stream::unfold(
        (stream, command_tag, false),
        |(stream, tag, done)| async move {
            if done {
                return None;
            }
            let (item, done) = match stream.next().await {
                Some(Ok(resp)) => match resp.parsed() {
                    Response::Done {
                        tag: received,
                        status,
                        outcome,
                        ..
                    } if received == &tag => {
                        let err = match status {
                            imap_proto::Status::Ok => return None,
                            imap_proto::Status::No => Error::No(format!("outcome: {outcome:?}")),
                            imap_proto::Status::Bad => Error::Bad(format!("outcome: {outcome:?}")),
                            _ => Error::Io(io::Error::other("Invalid tagged completion status")),
                        };
                        (Err(err), true)
                    }
                    Response::Done { .. } => (
                        Err(Error::Io(io::Error::other("Unexpected command tag"))),
                        true,
                    ),
                    _ => (Ok(resp), false),
                },
                Some(Err(err)) => (Err(Error::Io(err)), true),
                None => (
                    Err(Error::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Missing tagged command completion",
                    ))),
                    true,
                ),
            };
            Some((item, (stream, tag, done)))
        },
    )
    .boxed()
}

pub(crate) fn parse_fetches<T: Stream<Item = io::Result<ResponseData>> + Unpin + Send>(
    stream: &mut T,
    unsolicited: channel::Sender<UnsolicitedResponse>,
    command_tag: RequestId,
) -> impl Stream<Item = Result<Fetch>> + '_ + Send + Unpin {
    use futures_util::{FutureExt, StreamExt};

    StreamExt::filter_map(checked_responses(stream, command_tag), move |resp| {
        let unsolicited = unsolicited.clone();

        async move {
            match resp {
                Ok(resp) => match resp.parsed() {
                    Response::Fetch(..) => Some(Ok(Fetch::new(resp))),
                    _ => {
                        handle_unilateral(resp, unsolicited);
                        None
                    }
                },
                Err(err) => Some(Err(err.into())),
            }
        }
        .boxed()
    })
}

pub(crate) async fn parse_status<T: Stream<Item = io::Result<ResponseData>> + Unpin + Send>(
    stream: &mut T,
    expected_mailbox: &str,
    unsolicited: channel::Sender<UnsolicitedResponse>,
    command_tag: RequestId,
) -> Result<Mailbox> {
    let mut mbox = Mailbox::default();

    let mut responses = checked_responses(stream, command_tag);
    while let Some(resp) = responses.try_next().await? {
        match resp.parsed() {
            Response::MailboxData(MailboxDatum::Status { mailbox, status })
                if mailbox == expected_mailbox =>
            {
                for attribute in status {
                    match attribute {
                        StatusAttribute::HighestModSeq(highest_modseq) => {
                            mbox.highest_modseq = Some(*highest_modseq)
                        }
                        StatusAttribute::Messages(exists) => mbox.exists = *exists,
                        StatusAttribute::Recent(recent) => mbox.recent = *recent,
                        StatusAttribute::UidNext(uid_next) => mbox.uid_next = Some(*uid_next),
                        StatusAttribute::UidValidity(uid_validity) => {
                            mbox.uid_validity = Some(*uid_validity)
                        }
                        StatusAttribute::Unseen(unseen) => mbox.unseen = Some(*unseen),
                        _ => {}
                    }
                }
            }
            _ => {
                handle_unilateral(resp, unsolicited.clone());
            }
        }
    }

    Ok(mbox)
}

pub(crate) fn parse_expunge<T: Stream<Item = io::Result<ResponseData>> + Unpin + Send>(
    stream: &mut T,
    unsolicited: channel::Sender<UnsolicitedResponse>,
    command_tag: RequestId,
) -> impl Stream<Item = Result<u32>> + '_ + Send {
    use futures_util::StreamExt;

    StreamExt::filter_map(checked_responses(stream, command_tag), move |resp| {
        let unsolicited = unsolicited.clone();

        async move {
            match resp {
                Ok(resp) => match resp.parsed() {
                    Response::Expunge(id) => Some(Ok(*id)),
                    _ => {
                        handle_unilateral(resp, unsolicited);
                        None
                    }
                },
                Err(err) => Some(Err(err.into())),
            }
        }
    })
}

pub(crate) async fn parse_capabilities<
    T: Stream<Item = io::Result<ResponseData>> + Unpin + Send,
>(
    stream: &mut T,
    unsolicited: channel::Sender<UnsolicitedResponse>,
    command_tag: RequestId,
) -> Result<Capabilities> {
    let mut caps: HashSet<Capability> = HashSet::new();

    let mut responses = checked_responses(stream, command_tag);
    while let Some(resp) = responses.try_next().await? {
        match resp.parsed() {
            Response::Capabilities(cs) => {
                for c in cs {
                    caps.insert(Capability::from(c)); // TODO: avoid clone
                }
            }
            _ => {
                handle_unilateral(resp, unsolicited.clone());
            }
        }
    }

    Ok(Capabilities(caps))
}

pub(crate) async fn parse_noop<T: Stream<Item = io::Result<ResponseData>> + Unpin + Send>(
    stream: &mut T,
    unsolicited: channel::Sender<UnsolicitedResponse>,
    command_tag: RequestId,
) -> Result<()> {
    let mut responses = checked_responses(stream, command_tag);
    while let Some(resp) = responses.try_next().await? {
        handle_unilateral(resp, unsolicited.clone());
    }

    Ok(())
}

pub(crate) async fn parse_mailbox<T: Stream<Item = io::Result<ResponseData>> + Unpin + Send>(
    stream: &mut T,
    unsolicited: channel::Sender<UnsolicitedResponse>,
    command_tag: RequestId,
) -> Result<Mailbox> {
    let mut mailbox = Mailbox::default();

    let mut responses = checked_responses(stream, command_tag);
    while let Some(resp) = responses.try_next().await? {
        match resp.parsed() {
            Response::Data { status, outcome } => {
                use imap_proto::Status;

                match status {
                    Status::Ok => {
                        use imap_proto::ResponseCode;
                        match &outcome.code {
                            Some(ResponseCode::UidValidity(uid)) => {
                                mailbox.uid_validity = Some(*uid);
                            }
                            Some(ResponseCode::UidNext(unext)) => {
                                mailbox.uid_next = Some(*unext);
                            }
                            Some(ResponseCode::HighestModSeq(highest_modseq)) => {
                                mailbox.highest_modseq = Some(*highest_modseq);
                            }
                            Some(ResponseCode::Unseen(n)) => {
                                mailbox.unseen = Some(*n);
                            }
                            Some(ResponseCode::PermanentFlags(flags)) => {
                                mailbox
                                    .permanent_flags
                                    .extend(flags.iter().map(|s| (*s).to_string()).map(Flag::from));
                            }
                            _ => {}
                        }
                    }
                    Status::Bad => {
                        return Err(Error::Bad(format!("outcome: {outcome:?}")));
                    }
                    Status::No => {
                        return Err(Error::No(format!("outcome: {outcome:?}")));
                    }
                    _ => {
                        return Err(Error::Io(io::Error::other(format!(
                            "status: {status:?}, outcome: {outcome:?}"
                        ))));
                    }
                }
            }
            Response::MailboxData(m) => match m {
                MailboxDatum::Status { .. } => handle_unilateral(resp, unsolicited.clone()),
                MailboxDatum::Exists(e) => {
                    mailbox.exists = *e;
                }
                MailboxDatum::Recent(r) => {
                    mailbox.recent = *r;
                }
                MailboxDatum::Flags(flags) => {
                    mailbox
                        .flags
                        .extend(flags.iter().map(|s| (*s).to_string()).map(Flag::from));
                }
                MailboxDatum::List { .. } => {}
                MailboxDatum::MetadataSolicited { .. } => {}
                MailboxDatum::MetadataUnsolicited { .. } => {}
                MailboxDatum::Search { .. } => {}
                MailboxDatum::Sort { .. } => {}
                _ => {}
            },
            _ => {
                handle_unilateral(resp, unsolicited.clone());
            }
        }
    }

    Ok(mailbox)
}

pub(crate) async fn parse_ids<T: Stream<Item = io::Result<ResponseData>> + Unpin + Send>(
    stream: &mut T,
    unsolicited: channel::Sender<UnsolicitedResponse>,
    command_tag: RequestId,
) -> Result<HashSet<u32>> {
    let mut ids: HashSet<u32> = HashSet::new();

    let mut responses = checked_responses(stream, command_tag);
    while let Some(resp) = responses.try_next().await? {
        match resp.parsed() {
            Response::MailboxData(MailboxDatum::Search(cs)) => {
                for c in cs {
                    ids.insert(*c);
                }
            }
            _ => {
                handle_unilateral(resp, unsolicited.clone());
            }
        }
    }

    Ok(ids)
}

/// Parses [GETMETADATA](https://www.rfc-editor.org/info/rfc5464) response.
pub(crate) async fn parse_metadata<T: Stream<Item = io::Result<ResponseData>> + Unpin + Send>(
    stream: &mut T,
    mailbox_name: &str,
    unsolicited: channel::Sender<UnsolicitedResponse>,
    command_tag: RequestId,
) -> Result<Vec<Metadata>> {
    let mut res_values = Vec::new();
    let mut responses = checked_responses(stream, command_tag);
    while let Some(resp) = responses.try_next().await? {
        match resp.parsed() {
            // METADATA Response with Values
            // <https://datatracker.ietf.org/doc/html/rfc5464.html#section-4.4.1>
            Response::MailboxData(MailboxDatum::MetadataSolicited { mailbox, values })
                if mailbox == mailbox_name =>
            {
                res_values.extend_from_slice(values.as_slice());
            }

            // We are not interested in
            // [Unsolicited METADATA Response without Values](https://datatracker.ietf.org/doc/html/rfc5464.html#section-4.4.2),
            // they go to unsolicited channel with other unsolicited responses.
            _ => {
                handle_unilateral(resp, unsolicited.clone());
            }
        }
    }
    Ok(res_values)
}

/// Sends unilateral server response
/// (see Section 7 of RFC 3501)
/// into the channel.
///
/// If the channel is full or closed,
/// i.e. the responses are not being consumed,
/// ignores new responses.
pub(crate) fn handle_unilateral(
    res: ResponseData,
    unsolicited: channel::Sender<UnsolicitedResponse>,
) {
    match res.parsed() {
        Response::MailboxData(MailboxDatum::Status { mailbox, status }) => {
            unsolicited
                .try_send(UnsolicitedResponse::Status {
                    mailbox: (mailbox.as_ref()).into(),
                    attributes: status.to_vec(),
                })
                .ok();
        }
        Response::MailboxData(MailboxDatum::Recent(n)) => {
            unsolicited.try_send(UnsolicitedResponse::Recent(*n)).ok();
        }
        Response::MailboxData(MailboxDatum::Exists(n)) => {
            unsolicited.try_send(UnsolicitedResponse::Exists(*n)).ok();
        }
        Response::Expunge(n) => {
            unsolicited.try_send(UnsolicitedResponse::Expunge(*n)).ok();
        }
        _ => {
            unsolicited.try_send(UnsolicitedResponse::Other(res)).ok();
        }
    }
}
