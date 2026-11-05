//! Allocation-bounded subset used for CONDSTORE capability, selection and flags.
use super::*;
use crate::{Capability, ResponseCode};

const MAX_ATOMS: usize = 128;
const MAX_ATOM_BYTES: usize = 256;
const MAX_STATUS_TEXT: usize = 4096;
const MAX_PADDING: usize = 64;

pub(crate) fn parse<'a>(i: &'a [u8], limits: &BodyStructureLimits) -> ParseResult<'a> {
    let mut limits = *limits;
    limits.aggregate_bytes = limits.aggregate_bytes.min(MAX_SEARCH_RESPONSE_BYTES);
    let budget = Budget::new(&limits);
    let parsed = (|| {
        let (rest, response) = if i.first() == Some(&b'*') {
            let (rest, _) = tag("* ")(i)?;
            alt((
                map(
                    |i| status(i, &budget),
                    |(status, outcome)| Response::Data { status, outcome },
                ),
                |i| capabilities(i, &budget),
                map(
                    preceded(tag_no_case("FLAGS "), |i| flags(i, &budget)),
                    |flags| Response::MailboxData(MailboxDatum::Flags(flags)),
                ),
                |i| numeric(i, &budget),
            ))
            .parse(rest)?
        } else {
            let (rest, name) = take_while_m_n(1, 32, rfc3501::is_tag_char)(i)?;
            let (rest, _) = tag(" ")(rest)?;
            let (rest, (status, outcome)) = status(rest, &budget)?;
            let name = std::str::from_utf8(name).map_err(|_| invalid(name))?;
            (
                rest,
                Response::Done {
                    tag: RequestId(name.into()),
                    status,
                    outcome,
                },
            )
        };
        let (rest, _) = padding(rest, &budget)?;
        let (rest, _) = tag("\r\n")(rest)?;
        Ok((rest, response))
    })();
    parsed.map_err(|error| {
        if budget.failure.get().is_some() {
            ParseError::SearchLimit(SearchLimitError {
                kind: SearchLimitKind::ParserBytes,
                limit: limits.aggregate_bytes,
                actual: limits.aggregate_bytes + 1,
            })
        } else {
            ParseError::Syntax(error)
        }
    })
}

fn invalid(i: &[u8]) -> nom::Err<Error<&[u8]>> {
    nom::Err::Failure(Error::new(i, ErrorKind::Verify))
}

fn positive(i: &[u8], max: u64) -> Result<u64, nom::Err<Error<&[u8]>>> {
    if i.is_empty() || i.len() > 19 || !i.iter().all(u8::is_ascii_digit) {
        return Err(invalid(i));
    }
    std::str::from_utf8(i)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| (1..=max).contains(value))
        .ok_or_else(|| invalid(i))
}

fn positive_uid(i: &[u8]) -> Result<u32, nom::Err<Error<&[u8]>>> {
    if i.len() > 10 || i.first() == Some(&b'0') {
        return Err(invalid(i));
    }
    Ok(positive(i, u64::from(u32::MAX))? as u32)
}

fn status<'a>(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], (Status, Outcome<'a>)> {
    let (rest, status) = Status::parse(i)?;
    let Some(text) = rest.strip_prefix(b" ") else {
        return Ok((rest, (status, Outcome::default())));
    };
    let (rest, text) = take_while_m_n(0, MAX_STATUS_TEXT, core::is_text_char)(text)?;
    if rest.first().is_some_and(|b| core::is_text_char(*b)) {
        return budget.fail(rest, LimitKind::StringBytes);
    }
    let code = response_code(text)?;
    Ok((
        rest,
        (
            status,
            Outcome {
                code,
                information: None,
            },
        ),
    ))
}

fn response_code(text: &[u8]) -> Result<Option<ResponseCode<'_>>, nom::Err<Error<&[u8]>>> {
    let Some(code) = text.strip_prefix(b"[") else {
        return Ok(None);
    };
    let close = code.iter().position(|b| *b == b']');
    let mut fields = code[..close.unwrap_or(code.len())].splitn(2, |b| *b == b' ');
    let key = fields.next().unwrap_or_default();
    let value = fields.next();
    let code = if key.eq_ignore_ascii_case(b"HIGHESTMODSEQ") {
        ResponseCode::HighestModSeq(positive(value.unwrap_or_default(), MAX_MODSEQ)?)
    } else if key.eq_ignore_ascii_case(b"UIDVALIDITY") {
        ResponseCode::UidValidity(positive_uid(value.unwrap_or_default())?)
    } else if key.eq_ignore_ascii_case(b"UIDNEXT") {
        ResponseCode::UidNext(positive_uid(value.unwrap_or_default())?)
    } else {
        let code = if key.eq_ignore_ascii_case(b"NOMODSEQ") {
            ResponseCode::NoModSeq
        } else if key.eq_ignore_ascii_case(b"READ-ONLY") {
            ResponseCode::ReadOnly
        } else if key.eq_ignore_ascii_case(b"READ-WRITE") {
            ResponseCode::ReadWrite
        } else {
            return Ok(None);
        };
        if value.is_some() {
            return Err(invalid(text));
        }
        code
    };
    if !close.is_some_and(|end| {
        let tail = &text[end + 2..];
        tail.is_empty() || tail.starts_with(b" ")
    }) {
        return Err(invalid(text));
    }
    Ok(Some(code))
}

fn capabilities<'a>(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], Response<'a>> {
    let (mut rest, _) = tag_no_case("CAPABILITY")(i)?;
    let mut items = Vec::new();
    while let Some(atom) = rest.strip_prefix(b" ") {
        if atom.first() == Some(&b'\r') {
            break;
        }
        if items.len() == MAX_ATOMS {
            return budget.fail(atom, LimitKind::Nodes);
        }
        let (next, raw) = take_while_m_n(1, MAX_ATOM_BYTES, core::is_atom_char)(atom)?;
        if next.first().is_some_and(|b| core::is_atom_char(*b)) {
            return budget.fail(atom, LimitKind::StringBytes);
        }
        let value = std::str::from_utf8(raw).map_err(|_| invalid(raw))?;
        let capability = if value.eq_ignore_ascii_case("IMAP4rev1") {
            Capability::Imap4rev1
        } else {
            Capability::Atom(Cow::Borrowed(value))
        };
        budget.push(atom, &mut items, capability)?;
        rest = next;
    }
    Ok((rest, Response::Capabilities(items)))
}

fn flags<'a>(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], Vec<Cow<'a, str>>> {
    bounded_list(i, budget, 0, MAX_ATOMS, |i| flag_atom(i, budget))
}

fn flag_atom<'a>(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], Cow<'a, str>> {
    let (token, max_bytes, is_char) = if let Some(extension) = i.strip_prefix(b"\\") {
        if let Some(rest) = extension.strip_prefix(b"*") {
            return Ok((rest, Cow::Borrowed("\\*")));
        }
        (
            extension,
            MAX_ATOM_BYTES - 1,
            core::is_atom_char as fn(u8) -> bool,
        )
    } else {
        (i, MAX_ATOM_BYTES, core::is_astring_char as fn(u8) -> bool)
    };
    let (rest, _) = take_while_m_n(1, max_bytes, is_char)(token)?;
    if rest.first().is_some_and(|byte| is_char(*byte)) {
        return budget.fail(rest, LimitKind::StringBytes);
    }
    let raw = &i[..i.len() - rest.len()];
    let flag = std::str::from_utf8(raw).map_err(|_| invalid(raw))?;
    Ok((rest, Cow::Borrowed(flag)))
}

fn padding<'a>(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], &'a [u8]> {
    let whitespace = |byte| matches!(byte, b' ' | b'\t');
    let (rest, value) = take_while_m_n(0, MAX_PADDING, whitespace)(i)?;
    if rest.first().is_some_and(|byte| whitespace(*byte)) {
        return budget.fail(rest, LimitKind::StringBytes);
    }
    Ok((rest, value))
}

fn bounded_list<'a, T>(
    i: &'a [u8],
    budget: &Budget<'_>,
    min: usize,
    max: usize,
    mut item: impl FnMut(&'a [u8]) -> IResult<&'a [u8], T>,
) -> IResult<&'a [u8], Vec<T>> {
    let (rest, _) = tag("(")(i)?;
    let (mut rest, _) = padding(rest, budget)?;
    let mut values = Vec::new();
    loop {
        if let Some(tail) = rest.strip_prefix(b")") {
            if values.len() < min {
                return Err(invalid(rest));
            }
            return Ok((tail, values));
        }
        if values.len() == max {
            if rest.is_empty() {
                return Err(nom::Err::Incomplete(nom::Needed::new(1)));
            }
            return budget.fail(rest, LimitKind::Nodes);
        }
        let (tail, value) = item(rest)?;
        budget.push(rest, &mut values, value)?;
        let (tail, space) = padding(tail, budget)?;
        if space.is_empty() && tail.first().is_some_and(|byte| *byte != b')') {
            return Err(invalid(tail));
        }
        rest = tail;
    }
}

fn numeric<'a>(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], Response<'a>> {
    let (rest, sequence) = search_u32(i)?;
    alt((
        map(tag_no_case(" EXISTS"), move |_| {
            Response::MailboxData(MailboxDatum::Exists(sequence))
        }),
        map(tag_no_case(" RECENT"), move |_| {
            Response::MailboxData(MailboxDatum::Recent(sequence))
        }),
        map(tag_no_case(" EXPUNGE"), move |_| {
            Response::Expunge(sequence)
        }),
        map(
            preceded(tag_no_case(" FETCH "), |i| {
                bounded_list(i, budget, 1, 3, |i| attribute(i, budget))
            }),
            move |attrs| Response::Fetch(sequence, attrs),
        ),
    ))
    .parse(rest)
}

fn attribute<'a>(i: &'a [u8], budget: &Budget<'_>) -> IResult<&'a [u8], AttributeValue<'a>> {
    alt((
        |i: &'a [u8]| {
            let (rest, _) = tag_no_case("UID ")(i)?;
            let (rest, digits) = take_while_m_n(1, 10, |b: u8| b.is_ascii_digit())(rest)?;
            if rest.first().is_some_and(u8::is_ascii_digit) {
                return Err(invalid(rest));
            }
            Ok((rest, AttributeValue::Uid(positive_uid(digits)?)))
        },
        map(
            preceded(tag_no_case("FLAGS "), |i| flags(i, budget)),
            AttributeValue::Flags,
        ),
        |i| {
            let (rest, _) = tag_no_case("MODSEQ (")(i)?;
            let (rest, digits) = take_while_m_n(1, 19, |b: u8| b.is_ascii_digit())(rest)?;
            let (rest, _) = tag(")")(rest)?;
            Ok((rest, AttributeValue::ModSeq(positive(digits, MAX_MODSEQ)?)))
        },
    ))
    .parse(i)
}
