//! Adds support for the GETQUOTA and GETQUOTAROOT commands specificed in [RFC2087](https://tools.ietf.org/html/rfc2087).

use async_channel as channel;
use futures_util::{Stream, TryStreamExt as _, io};
use imap_proto::{self, RequestId, Response};

use crate::types::*;
use crate::{
    error::Result,
    parse::{checked_responses, handle_unilateral},
};
use crate::{
    error::{Error, ParseError},
    types::{Quota, QuotaRoot, ResponseData},
};

pub(crate) async fn parse_get_quota<T: Stream<Item = io::Result<ResponseData>> + Unpin + Send>(
    stream: &mut T,
    unsolicited: channel::Sender<UnsolicitedResponse>,
    command_tag: RequestId,
) -> Result<Quota> {
    let mut quota = None;
    let mut responses = checked_responses(stream, command_tag);
    while let Some(resp) = responses.try_next().await? {
        match resp.parsed() {
            Response::Quota(q) => quota = Some(q.clone().into()),
            _ => {
                handle_unilateral(resp, unsolicited.clone());
            }
        }
    }

    match quota {
        Some(q) => Ok(q),
        None => Err(Error::Parse(ParseError::ExpectedResponseNotFound(
            "Quota, no quota response found".to_string(),
        ))),
    }
}

pub(crate) async fn parse_get_quota_root<
    T: Stream<Item = io::Result<ResponseData>> + Unpin + Send,
>(
    stream: &mut T,
    unsolicited: channel::Sender<UnsolicitedResponse>,
    command_tag: RequestId,
) -> Result<(Vec<QuotaRoot>, Vec<Quota>)> {
    let mut roots: Vec<QuotaRoot> = Vec::new();
    let mut quotas: Vec<Quota> = Vec::new();

    let mut responses = checked_responses(stream, command_tag);
    while let Some(resp) = responses.try_next().await? {
        match resp.parsed() {
            Response::QuotaRoot(qr) => {
                roots.push(qr.clone().into());
            }
            Response::Quota(q) => {
                quotas.push(q.clone().into());
            }
            _ => {
                handle_unilateral(resp, unsolicited.clone());
            }
        }
    }

    Ok((roots, quotas))
}
