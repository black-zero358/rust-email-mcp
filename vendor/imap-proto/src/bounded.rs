//! Explicit allocation budgets for BODYSTRUCTURE and nested ENVELOPE values.
use crate::{
    core,
    rfc3501::{self, Envelope},
    Address, AttributeValue, Response,
};
use nom::{
    bytes::streaming::tag,
    error::{Error, ErrorKind},
    IResult, Parser,
};
use std::{borrow::Cow, cell::Cell, mem::size_of};

#[derive(Clone, Copy, Debug)]
pub struct BodyStructureLimits {
    pub depth: usize,
    pub nodes: usize,
    pub string_bytes: usize,
    pub node_parameters: usize,
    pub total_parameters: usize,
    pub aggregate_bytes: usize,
}
impl Default for BodyStructureLimits {
    fn default() -> Self {
        Self {
            depth: 32,
            nodes: 1024,
            string_bytes: 128 * 1024,
            node_parameters: 256,
            total_parameters: 8192,
            aggregate_bytes: 1024 * 1024,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitKind {
    Depth,
    Nodes,
    StringBytes,
    NodeParameters,
    TotalParameters,
    AggregateBytes,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitUnit {
    Levels,
    Nodes,
    Bytes,
    Pairs,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitError {
    pub kind: LimitKind,
    pub limit: usize,
    pub unit: LimitUnit,
}
impl LimitError {
    pub fn new(kind: LimitKind, limits: &BodyStructureLimits) -> Self {
        let (limit, unit) = match kind {
            LimitKind::Depth => (limits.depth.min(32), LimitUnit::Levels),
            LimitKind::Nodes => (limits.nodes, LimitUnit::Nodes),
            LimitKind::StringBytes => (limits.string_bytes, LimitUnit::Bytes),
            LimitKind::NodeParameters => (limits.node_parameters, LimitUnit::Pairs),
            LimitKind::TotalParameters => (limits.total_parameters, LimitUnit::Pairs),
            LimitKind::AggregateBytes => (limits.aggregate_bytes, LimitUnit::Bytes),
        };
        Self { kind, limit, unit }
    }
}
impl std::fmt::Display for LimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "IMAP BODYSTRUCTURE {:?} budget exceeded (limit {} {:?})",
            self.kind, self.limit, self.unit
        )
    }
}
impl std::error::Error for LimitError {}
#[derive(Debug, PartialEq)]
pub enum ParseError<'a> {
    Limit(LimitError),
    FetchLiteralLimit(FetchLiteralLimitError),
    Syntax(nom::Err<Error<&'a [u8]>>),
}

/// A command-scoped BODY section literal exceeded the requested byte count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FetchLiteralLimitError {
    pub limit: usize,
    pub actual: usize,
}
impl std::fmt::Display for FetchLiteralLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "IMAP FETCH literal byte limit exceeded (limit {})",
            self.limit
        )
    }
}
impl std::error::Error for FetchLiteralLimitError {}
pub type ParseResult<'a> = Result<(&'a [u8], Response<'a>), ParseError<'a>>;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BudgetUsage {
    pub nodes: usize,
    pub parameters: usize,
    pub string_bytes: usize,
    pub container_bytes: usize,
    pub total_bytes: usize,
    pub peak_depth: usize,
    pub largest_string_bytes: usize,
    pub largest_reservation_bytes: usize,
    pub largest_capacity_bytes: usize,
    pub owned_string_capacity_bytes: usize,
    pub largest_string_capacity_bytes: usize,
}
#[derive(Clone, Copy)]
struct Usage {
    depth: usize,
    nodes: usize,
    bytes: usize,
    parameters: usize,
    node_parameters: [usize; 33],
    report: BudgetUsage,
}
impl Default for Usage {
    fn default() -> Self {
        Self {
            depth: 0,
            nodes: 0,
            bytes: 0,
            parameters: 0,
            node_parameters: [0; 33],
            report: BudgetUsage::default(),
        }
    }
}
pub(crate) struct Budget<'l> {
    limits: Option<&'l BodyStructureLimits>,
    usage: Cell<Usage>,
    failure: Cell<Option<LimitError>>,
    fetch_literal_bytes: Option<usize>,
    fetch_failure: Cell<Option<FetchLiteralLimitError>>,
}
pub(crate) struct Guard<'b, 'l> {
    budget: &'b Budget<'l>,
    depth: usize,
}
impl Drop for Guard<'_, '_> {
    fn drop(&mut self) {
        let mut usage = self.budget.usage.get();
        usage.depth = self.depth;
        self.budget.usage.set(usage);
    }
}
impl<'l> Budget<'l> {
    pub(crate) fn unlimited() -> Self {
        Self {
            limits: None,
            usage: Cell::new(Usage::default()),
            failure: Cell::new(None),
            fetch_literal_bytes: None,
            fetch_failure: Cell::new(None),
        }
    }
    fn new(limits: &'l BodyStructureLimits) -> Self {
        Self {
            limits: Some(limits),
            ..Self::unlimited()
        }
    }
    fn fail<'a, T>(&self, i: &'a [u8], kind: LimitKind) -> IResult<&'a [u8], T> {
        self.failure.set(Some(LimitError::new(
            kind,
            self.limits.unwrap_or(&BodyStructureLimits::default()),
        )));
        Err(nom::Err::Failure(Error::new(i, ErrorKind::TooLarge)))
    }
    pub(crate) fn enter<'a>(
        &self,
        i: &'a [u8],
    ) -> Result<Guard<'_, 'l>, nom::Err<Error<&'a [u8]>>> {
        let mut usage = self.usage.get();
        let depth = usage.depth;
        if let Some(limits) = self.limits {
            if depth >= limits.depth || depth >= 32 {
                return self.fail(i, LimitKind::Depth).map(|(_, v)| v);
            }
            if usage.nodes >= limits.nodes {
                return self.fail(i, LimitKind::Nodes).map(|(_, v)| v);
            }
            usage.nodes += 1;
            usage.depth += 1;
            usage.report.peak_depth = usage.report.peak_depth.max(usage.depth);
            usage.node_parameters[usage.depth] = 0;
            self.usage.set(usage);
        }
        Ok(Guard {
            budget: self,
            depth,
        })
    }
    pub(crate) fn attempt<'a, T>(
        &self,
        i: &'a [u8],
        mut parser: impl FnMut(&'a [u8]) -> IResult<&'a [u8], T>,
    ) -> IResult<&'a [u8], T> {
        let checkpoint = self.usage.get();
        let result = parser(i);
        if matches!(result, Err(nom::Err::Error(_))) {
            self.usage.set(checkpoint);
        }
        result
    }
    fn charge<'a>(
        &self,
        i: &'a [u8],
        bytes: usize,
        string: bool,
    ) -> Result<(), nom::Err<Error<&'a [u8]>>> {
        if let Some(limits) = self.limits {
            let mut usage = self.usage.get();
            let Some(next) = usage
                .bytes
                .checked_add(bytes)
                .filter(|n| *n <= limits.aggregate_bytes)
            else {
                return self.fail(i, LimitKind::AggregateBytes).map(|(_, v)| v);
            };
            usage.bytes = next;
            if string {
                usage.report.string_bytes += bytes;
            } else {
                usage.report.container_bytes += bytes;
            }
            self.usage.set(usage);
        }
        Ok(())
    }
    pub(crate) fn parameter<'a>(&self, i: &'a [u8]) -> Result<(), nom::Err<Error<&'a [u8]>>> {
        if let Some(limits) = self.limits {
            let mut usage = self.usage.get();
            if usage.node_parameters[usage.depth] >= limits.node_parameters {
                return self.fail(i, LimitKind::NodeParameters).map(|(_, v)| v);
            }
            if usage.parameters >= limits.total_parameters {
                return self.fail(i, LimitKind::TotalParameters).map(|(_, v)| v);
            }
            usage.node_parameters[usage.depth] += 1;
            usage.parameters += 1;
            self.usage.set(usage);
        }
        Ok(())
    }
    fn string_check<'a>(&self, i: &'a [u8], bytes: usize) -> Result<(), nom::Err<Error<&'a [u8]>>> {
        if let Some(limits) = self.limits {
            if bytes > limits.string_bytes {
                return self.fail(i, LimitKind::StringBytes).map(|(_, v)| v);
            }
            if self
                .usage
                .get()
                .bytes
                .checked_add(bytes)
                .map_or(true, |n| n > limits.aggregate_bytes)
            {
                return self.fail(i, LimitKind::AggregateBytes).map(|(_, v)| v);
            }
        }
        Ok(())
    }
    fn string<'a>(&self, i: &'a [u8]) -> IResult<&'a [u8], &'a [u8]> {
        if self.limits.is_some() {
            if i.first() == Some(&b'{') {
                let (rest, _) = tag("{")(i)?;
                let (rest, count) = core::number(rest)?;
                let (rest, _) = tag("}\r\n")(rest)?;
                self.string_check(i, count as usize)?; // Before take(count) can request more input.
                let _ = rest;
            } else if i.first() == Some(&b'"') {
                let mut escaped = false;
                for (position, &byte) in i[1..].iter().enumerate() {
                    if byte == b'"' && !escaped {
                        break;
                    }
                    self.string_check(i, position + 1)?; // Also bounds unfinished quoted input.
                    escaped = byte == b'\\' && !escaped;
                }
            }
        }
        let (rest, value) = core::string(i)?;
        self.string_check(i, value.len())?;
        self.charge(i, value.len(), true)?;
        let mut usage = self.usage.get();
        usage.report.largest_string_bytes = usage.report.largest_string_bytes.max(value.len());
        self.usage.set(usage);
        Ok((rest, value))
    }
    pub(crate) fn nstring<'a>(&self, i: &'a [u8]) -> IResult<&'a [u8], Option<&'a [u8]>> {
        match core::nil(i) {
            Ok((rest, _)) => Ok((rest, None)),
            Err(nom::Err::Error(_)) => self.string(i).map(|(r, v)| (r, Some(v))),
            Err(error) => Err(error),
        }
    }
    pub(crate) fn string_utf8<'a>(&self, i: &'a [u8]) -> IResult<&'a [u8], Cow<'a, str>> {
        let (rest, bytes) = self.string(i)?;
        if let Ok(valid) = std::str::from_utf8(bytes) {
            return Ok((rest, Cow::Borrowed(valid)));
        }
        let mut remaining = bytes;
        let mut length = Some(0usize);
        loop {
            match std::str::from_utf8(remaining) {
                Ok(valid) => {
                    length = length.and_then(|n| n.checked_add(valid.len()));
                    break;
                }
                Err(error) => {
                    length = length
                        .and_then(|n| n.checked_add(error.valid_up_to()))
                        .and_then(|n| n.checked_add(3));
                    match error.error_len() {
                        Some(n) => remaining = &remaining[error.valid_up_to() + n..],
                        None => break,
                    }
                }
            }
        }
        let Some(length) = length else {
            return self.fail(i, LimitKind::StringBytes);
        };
        if let Some(limits) = self.limits {
            if length > limits.string_bytes {
                return self.fail(i, LimitKind::StringBytes);
            }
        }
        self.charge(i, length.saturating_sub(bytes.len()), true)?; // Before lossy UTF-8 allocates.
        let mut usage = self.usage.get();
        usage.report.largest_string_bytes = usage.report.largest_string_bytes.max(length);
        self.usage.set(usage);
        let mut value = String::new();
        // The complete lossy length has been charged before requesting storage.
        if value.try_reserve_exact(length).is_err() {
            return self.fail(i, LimitKind::AggregateBytes);
        }
        if let Some(limits) = self.limits {
            if value.capacity() > limits.string_bytes {
                return self.fail(i, LimitKind::StringBytes);
            }
        }
        self.charge(i, value.capacity().saturating_sub(length), true)?;
        let mut usage = self.usage.get();
        usage.report.owned_string_capacity_bytes += value.capacity();
        usage.report.largest_string_capacity_bytes = usage
            .report
            .largest_string_capacity_bytes
            .max(value.capacity());
        self.usage.set(usage);
        let mut remaining = bytes;
        loop {
            match std::str::from_utf8(remaining) {
                Ok(valid) => {
                    value.push_str(valid);
                    break;
                }
                Err(error) => {
                    let valid = std::str::from_utf8(&remaining[..error.valid_up_to()])
                        .map_err(|_| nom::Err::Failure(Error::new(i, ErrorKind::MapRes)))?;
                    value.push_str(valid);
                    value.push('\u{FFFD}');
                    match error.error_len() {
                        Some(n) => remaining = &remaining[error.valid_up_to() + n..],
                        None => break,
                    }
                }
            }
        }
        Ok((rest, Cow::Owned(value)))
    }
    pub(crate) fn nstring_utf8<'a>(&self, i: &'a [u8]) -> IResult<&'a [u8], Option<Cow<'a, str>>> {
        match core::nil(i) {
            Ok((rest, _)) => Ok((rest, None)),
            Err(nom::Err::Error(_)) => self.string_utf8(i).map(|(r, v)| (r, Some(v))),
            Err(error) => Err(error),
        }
    }
    pub(crate) fn single<'a, T>(
        &self,
        i: &'a [u8],
        value: T,
    ) -> Result<Vec<T>, nom::Err<Error<&'a [u8]>>> {
        let mut items = Vec::new();
        self.push(i, &mut items, value)?;
        Ok(items)
    }
    pub(crate) fn boxed<'a, T>(&self, i: &'a [u8]) -> Result<(), nom::Err<Error<&'a [u8]>>> {
        self.charge(i, size_of::<T>(), false)
    }
    pub(crate) fn push<'a, T>(
        &self,
        i: &'a [u8],
        items: &mut Vec<T>,
        item: T,
    ) -> Result<(), nom::Err<Error<&'a [u8]>>> {
        if items.len() == items.capacity() {
            let capacity = items.capacity();
            let Some(next) = capacity.checked_mul(2).map(|n| n.max(1)) else {
                return self.fail(i, LimitKind::AggregateBytes).map(|(_, v)| v);
            };
            let Some(requested_bytes) = next.checked_mul(size_of::<T>()) else {
                return self.fail(i, LimitKind::AggregateBytes).map(|(_, v)| v);
            };
            let Some(bytes) = (next - capacity).checked_mul(size_of::<T>()) else {
                return self.fail(i, LimitKind::AggregateBytes).map(|(_, v)| v);
            };
            self.charge(i, bytes, false)?; // All newly reserved slots, including unused capacity.
            if items.try_reserve_exact(next - items.len()).is_err() {
                return self.fail(i, LimitKind::AggregateBytes).map(|(_, v)| v);
            }
            // Global's requested layout is charged before reserve. Reconcile
            // any allocator-reported slack before retaining the new capacity.
            let Some(slack) = items
                .capacity()
                .saturating_sub(next)
                .checked_mul(size_of::<T>())
            else {
                return self.fail(i, LimitKind::AggregateBytes).map(|(_, v)| v);
            };
            self.charge(i, slack, false)?;
            let Some(actual_bytes) = items.capacity().checked_mul(size_of::<T>()) else {
                return self.fail(i, LimitKind::AggregateBytes).map(|(_, v)| v);
            };
            let mut usage = self.usage.get();
            usage.report.largest_reservation_bytes =
                usage.report.largest_reservation_bytes.max(requested_bytes);
            usage.report.largest_capacity_bytes =
                usage.report.largest_capacity_bytes.max(actual_bytes);
            self.usage.set(usage);
        }
        items.push(item);
        Ok(())
    }
    pub(crate) fn list<'a, T>(
        &self,
        i: &'a [u8],
        mut parser: impl Parser<&'a [u8], Output = T, Error = Error<&'a [u8]>>,
    ) -> IResult<&'a [u8], Vec<T>> {
        let (mut rest, _) = tag("(")(i)?;
        let mut items = Vec::new();
        loop {
            let (next, item) = self.attempt(rest, |i| parser.parse(i))?;
            self.push(rest, &mut items, item)?;
            rest = next;
            if rest.first() != Some(&b' ') {
                break;
            }
            if rest.iter().copied().find(|b| *b != b' ' && *b != b'\t') == Some(b')') {
                break;
            }
            rest = &rest[1..];
        }
        while matches!(rest.first(), Some(b' ' | b'\t')) {
            rest = &rest[1..];
        }
        let (rest, _) = tag(")")(rest)?;
        Ok((rest, items))
    }
    pub(crate) fn adjacent_list<'a, T>(
        &self,
        i: &'a [u8],
        mut parser: impl FnMut(&'a [u8]) -> IResult<&'a [u8], T>,
    ) -> IResult<&'a [u8], Vec<T>> {
        let (mut rest, item) = self.attempt(i, |i| parser(i))?;
        let mut items = Vec::new();
        self.push(i, &mut items, item)?;
        loop {
            match self.attempt(rest, |i| parser(i)) {
                Ok((next, item)) => {
                    if next.len() == rest.len() {
                        return Err(nom::Err::Error(Error::new(rest, ErrorKind::Many1)));
                    }
                    self.push(rest, &mut items, item)?;
                    rest = next;
                }
                Err(nom::Err::Error(_)) => return Ok((rest, items)),
                Err(error) => return Err(error),
            }
        }
    }
    pub(crate) fn addresses<'a>(&self, i: &'a [u8]) -> IResult<&'a [u8], Option<Vec<Address<'a>>>> {
        match core::nil(i) {
            Ok((rest, _)) => return Ok((rest, None)),
            Err(nom::Err::Error(_)) => {}
            Err(error) => return Err(error),
        }
        let (mut rest, _) = tag("(")(i)?;
        let mut items = Vec::new();
        loop {
            let (next, item) = Address::parse_with_budget(rest, self)?;
            self.push(rest, &mut items, item)?;
            rest = next;
            if rest.first() == Some(&b' ') {
                rest = &rest[1..];
            }
            if rest.first() == Some(&b')') {
                break;
            }
        }
        let (rest, _) = tag(")")(rest)?;
        Ok((rest, Some(items)))
    }

    pub(crate) fn section_number<'a>(
        &self,
        input: &'a [u8],
        parts: &mut Vec<u32>,
        number: u32,
    ) -> Result<(), nom::Err<Error<&'a [u8]>>> {
        if let Some(limits) = self.limits {
            if parts.len() >= limits.depth.min(32) {
                return self.fail(input, LimitKind::Depth).map(|(_, value)| value);
            }
        }
        self.push(input, parts, number)
    }
    pub(crate) fn section_field<'a>(&self, input: &'a [u8]) -> IResult<&'a [u8], &'a [u8]> {
        if matches!(input.first(), Some(b'"' | b'{')) {
            return self.string(input);
        }
        for (position, &byte) in input.iter().enumerate() {
            if !core::is_astring_char(byte) {
                break;
            }
            self.string_check(input, position + 1)?;
        }
        let (rest, field) = core::astring(input)?;
        self.charge(input, field.len(), true)?;
        Ok((rest, field))
    }
    pub(crate) fn section_fields<'a>(&self, input: &'a [u8]) -> IResult<&'a [u8], Vec<&'a [u8]>> {
        if input == b"( " {
            return Err(nom::Err::Incomplete(nom::Needed::new(1)));
        }
        if let Some(rest) = input.strip_prefix(b"()") {
            return Ok((rest, Vec::new()));
        }
        if let Some(rest) = input.strip_prefix(b"( )") {
            return Ok((rest, Vec::new()));
        }
        self.list(input, |input| self.section_field(input))
    }

    fn fetch_limit<'a, T>(
        &self,
        input: &'a [u8],
        limit: usize,
        actual: usize,
    ) -> IResult<&'a [u8], T> {
        self.fetch_failure
            .set(Some(FetchLiteralLimitError { limit, actual }));
        Err(nom::Err::Failure(Error::new(input, ErrorKind::TooLarge)))
    }

    fn fetch_data<'a>(
        &self,
        input: &'a [u8],
        limit: usize,
    ) -> IResult<&'a [u8], Option<Cow<'a, [u8]>>> {
        match core::nil(input) {
            Ok((rest, _)) => return Ok((rest, None)),
            Err(nom::Err::Error(_)) => {}
            Err(error) => return Err(error),
        }
        if input.first() == Some(&b'{') {
            let (rest, _) = tag("{")(input)?;
            let (rest, count) = core::number(rest)?;
            let (_, _) = tag("}\r\n")(rest)?;
            if count as usize > limit {
                return self.fetch_limit(input, limit, count as usize);
            }
        } else if input.first() == Some(&b'"') {
            let mut escaped = false;
            let mut logical = 0;
            for &byte in &input[1..] {
                if byte == b'"' && !escaped {
                    break;
                }
                if byte == b'\\' && !escaped {
                    escaped = true;
                    continue;
                }
                escaped = false;
                logical += 1;
                if logical > limit {
                    return self.fetch_limit(input, limit, logical);
                }
            }
            let (rest, quoted) = core::quoted(input)?;
            if !quoted.contains(&b'\\') {
                return Ok((rest, Some(Cow::Borrowed(quoted))));
            }
            // IMAP quoting is transport escaping, not attachment content. Reserve
            // only the checked logical octets and charge owned bytes separately.
            self.charge(input, logical, true)?;
            let mut bytes = Vec::new();
            if bytes.try_reserve_exact(logical).is_err() {
                return self.fail(input, LimitKind::AggregateBytes);
            }
            self.charge(input, bytes.capacity().saturating_sub(logical), true)?;
            let mut characters = quoted.iter().copied();
            while let Some(byte) = characters.next() {
                let value = if byte == b'\\' {
                    characters
                        .next()
                        .ok_or_else(|| nom::Err::Failure(Error::new(input, ErrorKind::Escaped)))?
                } else {
                    byte
                };
                bytes.push(value);
            }
            return Ok((rest, Some(Cow::Owned(bytes))));
        }
        let (rest, data) = core::string(input)?;
        if data.len() > limit {
            return self.fetch_limit(input, limit, data.len());
        }
        Ok((rest, Some(Cow::Borrowed(data))))
    }

    fn fetch_body_section<'a>(
        &self,
        input: &'a [u8],
        limit: usize,
    ) -> IResult<&'a [u8], AttributeValue<'a>> {
        let (rest, _) = nom::bytes::streaming::tag_no_case("BODY")(input)?;
        let (rest, section) = crate::rfc3501::body::section_with_budget(rest, self)?;
        let (rest, index) =
            nom::combinator::opt(nom::sequence::delimited(tag("<"), core::number, tag(">")))
                .parse(rest)?;
        let (rest, _) = tag(" ")(rest)?;
        let (rest, data) = self.fetch_data(rest, limit)?;
        Ok((
            rest,
            AttributeValue::BodySection {
                section,
                index,
                data,
            },
        ))
    }
}
fn prefix(i: &[u8]) -> IResult<&[u8], u32> {
    let (i, _) = tag("* ")(i)?;
    let (i, n) = core::number(i)?;
    let (i, _) = nom::bytes::streaming::tag_no_case(" FETCH ")(i)?;
    Ok((i, n))
}
pub(crate) fn parse_response<'a>(i: &'a [u8], limits: &BodyStructureLimits) -> ParseResult<'a> {
    parse_response_with_usage(i, limits).0
}
pub(crate) fn parse_response_with_usage<'a>(
    i: &'a [u8],
    limits: &BodyStructureLimits,
) -> (ParseResult<'a>, BudgetUsage) {
    parse_response_with_fetch_limit(i, limits, None)
}

pub(crate) fn parse_response_with_fetch_limit<'a>(
    i: &'a [u8],
    limits: &BodyStructureLimits,
    fetch_literal_bytes: Option<usize>,
) -> (ParseResult<'a>, BudgetUsage) {
    let Ok((rest, sequence)) = prefix(i) else {
        return (
            Response::parse(i).map_err(ParseError::Syntax),
            BudgetUsage::default(),
        );
    };
    let mut budget = Budget::new(limits);
    budget.fetch_literal_bytes = fetch_literal_bytes;
    let result = (|| {
        let (rest, items) = budget.list(rest, |i| {
            if let Ok((rest, _)) =
                nom::bytes::streaming::tag_no_case::<_, _, Error<&[u8]>>("BODYSTRUCTURE ")(i)
            {
                return rfc3501::body_structure::BodyStructure::parse_with_budget(rest, &budget)
                    .map(|(r, v)| (r, AttributeValue::BodyStructure(v)));
            }
            if let Ok((rest, _)) =
                nom::bytes::streaming::tag_no_case::<_, _, Error<&[u8]>>("ENVELOPE ")(i)
            {
                let (rest, value) = Envelope::parse_with_budget(rest, &budget)?;
                budget.boxed::<Envelope>(i)?;
                return Ok((rest, AttributeValue::Envelope(Box::new(value))));
            }
            if let Some(limit) = budget.fetch_literal_bytes.filter(|_| {
                i.get(..5)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"BODY["))
            }) {
                return budget.fetch_body_section(i, limit);
            }
            AttributeValue::parse(i)
        })?;
        let mut rest = rest;
        while rest.first() == Some(&b' ') {
            rest = &rest[1..];
        }
        let (rest, _) = tag("\r\n")(rest)?;
        Ok((rest, Response::Fetch(sequence, items)))
    })();
    let result = result.map_err(|error| {
        if let Some(error) = budget.fetch_failure.get() {
            ParseError::FetchLiteralLimit(error)
        } else {
            match budget.failure.get() {
                Some(kind) => ParseError::Limit(kind),
                None => ParseError::Syntax(error),
            }
        }
    });
    let usage = budget.usage.get();
    let mut report = usage.report;
    report.nodes = usage.nodes;
    report.parameters = usage.parameters;
    report.total_bytes = usage.bytes;
    (result, report)
}
