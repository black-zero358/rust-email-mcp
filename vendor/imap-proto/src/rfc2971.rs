//!
//!
//! <https://tools.ietf.org/html/rfc2971>
//!
//! The IMAP4 ID extension
//!

use std::{borrow::Cow, collections::HashMap};

use nom::{
    IResult, Parser,
    branch::alt,
    bytes::complete::tag_no_case,
    character::complete::{char, space0, space1},
    combinator::map,
    multi::many0,
    sequence::{preceded, separated_pair},
};

use crate::{
    Response,
    core::{nil, nstring_utf8, string_utf8},
};

// A single id parameter (field and value).
// Format: string SPACE nstring
// [RFC2971 - Formal Syntax](https://tools.ietf.org/html/rfc2971#section-4)
#[allow(clippy::type_complexity)]
fn id_param(i: &[u8]) -> IResult<&[u8], (Cow<'_, str>, Option<Cow<'_, str>>)> {
    separated_pair(string_utf8, space1, nstring_utf8).parse(i)
}

// The non-nil case of id parameter list.
// Format: "(" #(string SPACE nstring) ")"
// [RFC2971 - Formal Syntax](https://tools.ietf.org/html/rfc2971#section-4)
fn id_param_list_not_nil(i: &[u8]) -> IResult<&[u8], HashMap<Cow<'_, str>, Cow<'_, str>>> {
    map(
        (
            char('('),
            id_param,
            many0((space1, id_param)),
            preceded(space0, char(')')),
        ),
        |(_, first_param, rest_params, _)| {
            let mut params = vec![first_param];
            for (_, p) in rest_params {
                params.push(p)
            }

            params
                .into_iter()
                .filter(|(_k, v)| v.is_some())
                .map(|(k, v)| (k, v.unwrap()))
                .collect()
        },
    )
    .parse(i)
}

// The id parameter list of all cases
// id_params_list ::= "(" #(string SPACE nstring) ")" / nil
// [RFC2971 - Formal Syntax](https://tools.ietf.org/html/rfc2971#section-4)
#[allow(clippy::type_complexity)]
fn id_param_list(i: &[u8]) -> IResult<&[u8], Option<HashMap<Cow<'_, str>, Cow<'_, str>>>> {
    alt((map(id_param_list_not_nil, Some), map(nil, |_| None))).parse(i)
}

// id_response ::= "ID" SPACE id_params_list
// [RFC2971 - Formal Syntax](https://tools.ietf.org/html/rfc2971#section-4)
pub(crate) fn resp_id(i: &[u8]) -> IResult<&[u8], Response<'_>> {
    let (rest, map) = map(
        (tag_no_case("ID"), space1, id_param_list),
        |(_id, _sp, p)| p,
    )
    .parse(i)?;

    Ok((rest, Response::Id(map)))
}
