use std::borrow::Cow;
use std::collections::HashMap;

use nom::{IResult, Parser, branch::alt};

pub mod bounded;
pub mod core;
use crate::core::to_owned_cow;

pub mod rfc3501;
pub use rfc3501::body::{MessageSection, SectionPath};
pub use rfc3501::body_structure::{
    BodyContentCommon, BodyContentSinglePart, BodyExt1Part, BodyExtMPart, BodyExtension,
    BodyFields, BodyParams, BodyStructure, ContentDisposition, ContentEncoding, ContentType,
};
pub use rfc3501::{
    Address, AttributeValue, Capability, Envelope, MailboxDatum, MailboxListData, NameAttribute,
    Outcome, RequestId, ResponseCode, Status, StatusAttribute,
};

pub mod bodystructure;
pub mod builders;
pub mod gmail;
pub mod rfc2087;
use rfc2087::{Quota, QuotaRoot};
pub mod rfc2971;
pub mod rfc4314;
use rfc4314::{Acl, ListRights, MyRights};
pub mod rfc4315;
pub mod rfc4551;
pub mod rfc5161;
pub mod rfc5256;
pub mod rfc5464;
pub mod rfc7162;

#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Response<'a> {
    Capabilities(Vec<Capability<'a>>),
    Enabled(Vec<Capability<'a>>),
    Continue(Outcome<'a>),
    Done {
        tag: RequestId,
        status: Status,
        outcome: Outcome<'a>,
    },
    Data {
        status: Status,
        outcome: Outcome<'a>,
    },
    Expunge(u32),
    Vanished {
        earlier: bool,
        uids: Vec<std::ops::RangeInclusive<u32>>,
    },
    Fetch(u32, Vec<AttributeValue<'a>>),
    MailboxData(MailboxDatum<'a>),
    Quota(Quota<'a>),
    QuotaRoot(QuotaRoot<'a>),
    Id(Option<HashMap<Cow<'a, str>, Cow<'a, str>>>),
    Acl(Acl<'a>),
    ListRights(ListRights<'a>),
    MyRights(MyRights<'a>),
}

impl<'a> Response<'a> {
    pub fn parse(msg: &'a [u8]) -> ParseResult<'a> {
        alt((
            rfc3501::continue_req,
            rfc3501::response_data,
            rfc3501::response_tagged,
        ))
        .parse(msg)
    }

    /// Parse one response with allocation budgets for BODYSTRUCTURE and ENVELOPE.
    /// Other response grammars retain the legacy parser behavior.
    pub fn parse_bounded(
        msg: &'a [u8],
        limits: &bounded::BodyStructureLimits,
    ) -> bounded::ParseResult<'a> {
        bounded::parse_response(msg, limits)
    }

    /// Return accounting on success, incomplete input, syntax error or limit.
    pub fn parse_bounded_with_usage(
        msg: &'a [u8],
        limits: &bounded::BodyStructureLimits,
    ) -> (bounded::ParseResult<'a>, bounded::BudgetUsage) {
        bounded::parse_response_with_usage(msg, limits)
    }

    /// Also reject BODY section literals above the current command's requested size.
    /// A declared literal is checked before requesting its payload from the buffer.
    pub fn parse_bounded_with_fetch_limit(
        msg: &'a [u8],
        limits: &bounded::BodyStructureLimits,
        max_literal_bytes: usize,
    ) -> bounded::ParseResult<'a> {
        bounded::parse_response_with_fetch_limit(msg, limits, Some(max_literal_bytes)).0
    }

    /// Parse SEARCH with a bounded UID list and allocation limits on the
    /// supported mailbox notifications. Status response text is consumed
    /// without building its optional code or information.
    pub fn parse_bounded_with_search_limit(
        msg: &'a [u8],
        limits: &bounded::BodyStructureLimits,
        max_uids: usize,
    ) -> bounded::ParseResult<'a> {
        bounded::parse_response_with_search_limit(msg, limits, max_uids)
    }

    /// Parse bounded mailbox notifications and UID FETCH metadata without
    /// entering the SEARCH or BODYSTRUCTURE grammars.
    pub fn parse_bounded_mailbox_metadata(
        msg: &'a [u8],
        limits: &bounded::BodyStructureLimits,
        max_header_bytes: usize,
    ) -> bounded::ParseResult<'a> {
        bounded::parse_mailbox_metadata(msg, limits, max_header_bytes)
    }

    /// Parse responses to a bounded CONDSTORE capability, selection or flag query.
    pub fn parse_bounded_condstore(
        msg: &'a [u8],
        limits: &bounded::BodyStructureLimits,
    ) -> bounded::ParseResult<'a> {
        bounded::parse_condstore(msg, limits)
    }

    /// Parse bounded synchronization responses, including ENABLED and VANISHED.
    pub fn parse_bounded_qresync(
        msg: &'a [u8],
        limits: &bounded::BodyStructureLimits,
    ) -> bounded::ParseResult<'a> {
        bounded::parse_qresync(msg, limits)
    }

    pub fn into_owned(self) -> Response<'static> {
        match self {
            Response::Capabilities(capabilities) => Response::Capabilities(
                capabilities
                    .into_iter()
                    .map(Capability::into_owned)
                    .collect(),
            ),
            Response::Enabled(capabilities) => Response::Enabled(
                capabilities
                    .into_iter()
                    .map(Capability::into_owned)
                    .collect(),
            ),
            Response::Continue(outcome) => Response::Continue(outcome.into_owned()),
            Response::Done {
                tag,
                status,
                outcome,
            } => Response::Done {
                tag,
                status,
                outcome: outcome.into_owned(),
            },
            Response::Data { status, outcome } => Response::Data {
                status,
                outcome: outcome.into_owned(),
            },
            Response::Expunge(seq) => Response::Expunge(seq),
            Response::Vanished { earlier, uids } => Response::Vanished { earlier, uids },
            Response::Fetch(seq, attrs) => Response::Fetch(
                seq,
                attrs.into_iter().map(AttributeValue::into_owned).collect(),
            ),
            Response::MailboxData(datum) => Response::MailboxData(datum.into_owned()),
            Response::Quota(quota) => Response::Quota(quota.into_owned()),
            Response::QuotaRoot(quota_root) => Response::QuotaRoot(quota_root.into_owned()),
            Response::Id(map) => Response::Id(map.map(|m| {
                m.into_iter()
                    .map(|(k, v)| (to_owned_cow(k), to_owned_cow(v)))
                    .collect()
            })),
            Response::Acl(acl_list) => Response::Acl(acl_list.into_owned()),
            Response::ListRights(rights) => Response::ListRights(rights.into_owned()),
            Response::MyRights(rights) => Response::MyRights(rights.into_owned()),
        }
    }
}

pub type ParseResult<'a> = IResult<&'a [u8], Response<'a>>;
