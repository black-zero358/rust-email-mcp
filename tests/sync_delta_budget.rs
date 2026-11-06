//! Bounded CONDSTORE parser and command transport against synthetic IMAP.
use async_imap::imap_proto::{
    Response,
    bounded::{BodyStructureLimits, ParseError, SearchLimitKind},
};

fn parse(wire: &[u8]) -> Result<(&[u8], Response<'_>), ParseError<'_>> {
    Response::parse_bounded_condstore(wire, &BodyStructureLimits::default())
}

#[test]
fn unfinished_tokens_stop_at_their_limits_without_waiting_for_a_delimiter() {
    for (prefix, byte, allowed) in [
        ("* 1 FETCH (UID 9 FLAGS (", b'x', 256),
        ("* CAPABILITY IMAP4rev1 ", b'x', 256),
        ("* OK ", b'x', 4096),
    ] {
        let mut wire = prefix.as_bytes().to_vec();
        for length in 0..=allowed {
            assert!(
                matches!(parse(&wire), Err(ParseError::Syntax(error)) if error.is_incomplete()),
                "{prefix:?} after {length} bytes"
            );
            wire.push(byte);
        }
        assert!(
            matches!(parse(&wire), Err(ParseError::SearchLimit(error)) if error.kind == SearchLimitKind::ParserBytes),
            "{prefix:?} at the first extra byte"
        );
    }
}
