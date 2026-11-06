//! Bounded ENABLED and VANISHED response parsing and transport.
use async_imap::imap_proto::{
    Response,
    bounded::{BodyStructureLimits, MAX_VANISHED_RANGES, ParseError, SearchLimitKind},
};

fn parse(wire: &[u8]) -> Result<(&[u8], Response<'_>), ParseError<'_>> {
    Response::parse_bounded_qresync(wire, &BodyStructureLimits::default())
}

#[test]
fn caps_raw_range_counts_before_allocation_and_rejects_invalid_endpoints() {
    let limit = format!(
        "* VANISHED (EARLIER) 9{}\r\n",
        ",9".repeat(MAX_VANISHED_RANGES - 1)
    );
    let (_, response) = parse(limit.as_bytes()).unwrap();
    assert!(
        matches!(response, Response::Vanished { uids, .. } if uids.len() == MAX_VANISHED_RANGES)
    );
    let overflow = format!("* VANISHED 9{}", ",9".repeat(MAX_VANISHED_RANGES));
    let long_capability = format!("* ENABLED {}", "X".repeat(257));
    for wire in [&overflow, &long_capability] {
        assert!(
            matches!(parse(wire.as_bytes()), Err(ParseError::SearchLimit(error)) if error.kind == SearchLimitKind::ParserBytes)
        );
    }
    for wire in [
        "* VANISHED 0\r\n",
        "* VANISHED 09\r\n",
        "* VANISHED 1:*\r\n",
        "* VANISHED *\r\n",
        "* VANISHED 4294967296\r\n",
        "* VANISHED 12345678901",
        "* VANISHED 1:9,\r\n",
        "* VANISHED (UNKNOWN) 1\r\n",
    ] {
        assert!(
            matches!(parse(wire.as_bytes()), Err(ParseError::Syntax(error)) if !error.is_incomplete()),
            "{wire:?}"
        );
    }
}
