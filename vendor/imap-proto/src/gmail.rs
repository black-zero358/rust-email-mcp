use std::borrow::Cow;

use nom::branch::alt;
use nom::bytes::streaming::tag_no_case;
use nom::combinator::map;
use nom::sequence::preceded;
use nom::{IResult, Parser};

use crate::rfc3501::{AttributeValue, MailboxDatum};

use super::core::{number_64, parenthesized_list, quoted_utf8};
use super::rfc3501::flag;

pub(crate) fn gmail_label_list(i: &[u8]) -> IResult<&[u8], Vec<Cow<'_, str>>> {
    preceded(
        tag_no_case("X-GM-LABELS "),
        parenthesized_list(alt((map(flag, Cow::Borrowed), quoted_utf8))),
    )
    .parse(i)
}

pub(crate) fn msg_att_gmail_labels(i: &[u8]) -> IResult<&[u8], AttributeValue<'_>> {
    map(gmail_label_list, AttributeValue::GmailLabels).parse(i)
}

pub(crate) fn mailbox_data_gmail_labels(i: &[u8]) -> IResult<&[u8], MailboxDatum<'_>> {
    map(gmail_label_list, MailboxDatum::GmailLabels).parse(i)
}

pub(crate) fn gmail_msgid(i: &[u8]) -> IResult<&[u8], u64> {
    preceded(tag_no_case("X-GM-MSGID "), number_64).parse(i)
}

pub(crate) fn msg_att_gmail_msgid(i: &[u8]) -> IResult<&[u8], AttributeValue<'_>> {
    map(gmail_msgid, AttributeValue::GmailMsgId).parse(i)
}

pub(crate) fn mailbox_data_gmail_msgid(i: &[u8]) -> IResult<&[u8], MailboxDatum<'_>> {
    map(gmail_msgid, MailboxDatum::GmailMsgId).parse(i)
}

pub(crate) fn gmail_thrid(i: &[u8]) -> IResult<&[u8], u64> {
    preceded(tag_no_case("X-GM-THRID "), number_64).parse(i)
}

pub(crate) fn msg_att_gmail_thrid(i: &[u8]) -> IResult<&[u8], AttributeValue<'_>> {
    map(gmail_thrid, AttributeValue::GmailThrId).parse(i)
}

pub(crate) fn mailbox_data_gmail_thrid(i: &[u8]) -> IResult<&[u8], MailboxDatum<'_>> {
    map(gmail_thrid, MailboxDatum::GmailThrId).parse(i)
}
