//! Command-scoped payload limits preserve normal raw reads and legal section syntax.
use async_imap::{
    imap_proto::{
        AttributeValue, Response,
        bounded::{BodyStructureLimits, ParseError},
    },
    types::CommandReadLimits,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn parse(input: &[u8], limit: usize) -> Result<(&[u8], Response<'_>), ParseError<'_>> {
    Response::parse_bounded_with_fetch_limit(input, &BodyStructureLimits::default(), limit)
}
fn body<'a>(response: &'a Response<'_>) -> &'a [u8] {
    let Response::Fetch(_, attrs) = response else {
        panic!("FETCH")
    };
    attrs
        .iter()
        .find_map(|attr| {
            if let AttributeValue::BodySection {
                data: Some(data), ..
            } = attr
            {
                Some(data.as_ref())
            } else {
                None
            }
        })
        .unwrap()
}

#[test]
fn declared_literal_is_checked_before_requesting_payload_for_any_uid_or_section() {
    for (uid, section) in [(7, "2"), (99, "9"), (99, "HEADER.FIELDS (SUBJECT)")] {
        let raw = format!("* 1 FETCH (UID {uid} BODY[{section}]<0> {{65537}}\r\n");
        let error = parse(raw.as_bytes(), 65536).unwrap_err();
        assert!(
            matches!(error, ParseError::FetchLiteralLimit(error) if error.limit==65536 && error.actual==65537)
        );
        let raw = format!("* 1 FETCH (UID {uid} BODY[{section}]<0> {{65536}}\r\n");
        assert!(
            matches!(parse(raw.as_bytes(),65536),Err(ParseError::Syntax(error)) if error.is_incomplete())
        );
    }
}

#[test]
fn full_section_grammar_and_every_legal_prefix_remain_supported() {
    for section in [
        "",
        "2",
        "2.MIME",
        "2.3.TEXT",
        "HEADER",
        "HEADER.FIELDS ()",
        "HEADER.FIELDS ( )",
        "HEADER.FIELDS.NOT (SUBJECT FROM)",
        "2.HEADER.FIELDS (\"SUBJECT\")",
        "HEADER.FIELDS ({7}\r\nSUBJECT)",
    ] {
        let raw = format!("* 1 FETCH (UID 99 BODY[{section}]<0> \"\")\r\n");
        let bounded = parse(raw.as_bytes(), 1).unwrap();
        assert_eq!(bounded, Response::parse(raw.as_bytes()).unwrap());
        for end in 0..raw.len() {
            assert!(
                matches!(parse(&raw.as_bytes()[..end],1),Err(ParseError::Syntax(error)) if error.is_incomplete()),
                "section {section}, prefix {end}"
            );
        }
    }
}

#[test]
fn quoted_payloads_use_logical_octets_and_unescape_without_changing_default_parser() {
    for (wire, expected) in [
        (r#""a\\b""#, b"a\\b".as_slice()),
        (r#""a\"b""#, b"a\"b".as_slice()),
        (r#""\\""#, b"\\".as_slice()),
        (r#""\"""#, b"\"".as_slice()),
        (r#""""#, b"".as_slice()),
    ] {
        let raw = format!("* 1 FETCH (UID 7 BODY[2]<0> {wire})\r\n");
        let (_, response) = parse(raw.as_bytes(), expected.len()).unwrap();
        assert_eq!(body(&response), expected);
        for end in 0..raw.len() {
            assert!(
                matches!(parse(&raw.as_bytes()[..end],expected.len()),Err(ParseError::Syntax(error)) if error.is_incomplete()),
                "quoted {wire}, prefix {end}"
            );
        }
    }
    let raw = b"* 1 FETCH (UID 7 BODY[2]<0> \"a\\\\b\")\r\n";
    // Ordinary parsing deliberately retains its preexisting wire representation.
    assert_eq!(body(&Response::parse(raw).unwrap().1), br"a\\b");
    let too_large = b"* 1 FETCH (UID 7 BODY[2]<0> \"a\\\\b\")\r\n";
    assert!(
        matches!(parse(too_large,2),Err(ParseError::FetchLiteralLimit(error)) if error.actual==3)
    );
}

#[tokio::test]
async fn typed_literal_error_closes_the_stream_and_contains_no_response_data() {
    let (client, server) = tokio::io::duplex(1024);
    let backend = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        let mut line = String::new();
        server.read_line(&mut line).await.unwrap();
        let tag = line.split_whitespace().next().unwrap();
        server
            .write_all(format!("{tag} OK login\r\n").as_bytes())
            .await
            .unwrap();
        line.clear();
        server.read_line(&mut line).await.unwrap();
        server
            .write_all(b"* 1 FETCH (UID 99 BODY[HEADER.FIELDS (private_marker)]<0> {65537}\r\n")
            .await
            .unwrap();
    });
    let mut session = async_imap::Client::with_buffer_limit(client, 128 * 1024)
        .login("synthetic", "test-only")
        .await
        .unwrap();
    session
        .run_command_with_limits(
            "UID FETCH 7 (UID BODY.PEEK[2]<0.65536>)",
            CommandReadLimits::body_section_literal_bytes(65536),
        )
        .await
        .unwrap();
    let error = async_imap::error::Error::Io(session.read_response().await.unwrap_err());
    assert_eq!(
        async_imap::error::body_section_literal_limit(&error)
            .unwrap()
            .limit,
        65536
    );
    assert!(!format!("{error:?}").contains("private_marker"));
    assert!(session.read_response().await.unwrap().is_none());
    assert!(session.run_command("NOOP").await.is_err());
    backend.await.unwrap();
}

#[tokio::test]
async fn matching_tag_releases_the_command_cap_for_a_subsequent_raw_read() {
    let (client, server) = tokio::io::duplex(1024);
    let backend = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        let mut line = String::new();
        server.read_line(&mut line).await.unwrap();
        let tag = line.split_whitespace().next().unwrap();
        server
            .write_all(format!("{tag} OK login\r\n").as_bytes())
            .await
            .unwrap();
        line.clear();
        server.read_line(&mut line).await.unwrap();
        let tag = line.split_whitespace().next().unwrap();
        server
            .write_all(
                format!("* 1 FETCH (UID 7 BODY[2]<0> {{1}}\r\nx)\r\n{tag} OK done\r\n").as_bytes(),
            )
            .await
            .unwrap();
        line.clear();
        server.read_line(&mut line).await.unwrap();
        server
            .write_all(b"* 1 FETCH (UID 7 BODY[] {65537}\r\n")
            .await
            .unwrap();
        server.write_all(&vec![b'x'; 65537]).await.unwrap();
        server.write_all(b")\r\n").await.unwrap();
    });
    let mut session = async_imap::Client::with_buffer_limit(client, 128 * 1024)
        .login("synthetic", "test-only")
        .await
        .unwrap();
    session
        .run_command_with_limits(
            "UID FETCH 7 (UID BODY.PEEK[2]<0.1>)",
            CommandReadLimits::body_section_literal_bytes(1),
        )
        .await
        .unwrap();
    assert!(session.read_response().await.unwrap().is_some());
    assert!(matches!(
        session.read_response().await.unwrap().unwrap().parsed(),
        Response::Done { .. }
    ));
    session
        .run_command("UID FETCH 7 (UID BODY.PEEK[])")
        .await
        .unwrap();
    let raw = session.read_response().await.unwrap().unwrap();
    assert_eq!(body(raw.parsed()).len(), 65537);
    backend.await.unwrap();
}
