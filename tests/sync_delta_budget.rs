//! Bounded CONDSTORE parser and command transport against synthetic IMAP.
use async_imap::{
    Client,
    error::search_limit,
    imap_proto::{
        AttributeValue, Response, ResponseCode, Status,
        bounded::{BodyStructureLimits, MAX_MODSEQ, ParseError, SearchLimitKind},
    },
    types::CommandReadLimits,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

fn parse(wire: &[u8]) -> Result<(&[u8], Response<'_>), ParseError<'_>> {
    Response::parse_bounded_condstore(wire, &BodyStructureLimits::default())
}

#[test]
fn positive_63_bit_selection_and_fetch_preserve_fragmented_prefixes() {
    for line in [
        "* CAPABILITY IMAP4rev1 CONDSTORE AUTH=PLAIN\r\n",
        "* OK [NOMODSEQ] message\r\n",
        "* OK [HIGHESTMODSEQ 9223372036854775807]\r\n",
        "* OK [UIDNEXT 4294967295] next\r\n",
        "* OK [UIDVALIDITY 9] valid\r\n",
        "A2 OK [READ-ONLY] done\r\n",
        "A2 OK [HIGHESTMODSEQ 17] done\r\n",
        "* 1 FETCH (UID 9 FLAGS () MODSEQ (9223372036854775807))\r\n",
        "* 1 FETCH (MODSEQ (1) FLAGS (\\Seen foo) UID 9)\r\n",
        "* 0 EXISTS\r\n",
        "* FLAGS (\\Seen \\*)\r\n",
        "* FLAGS ( \\Seen OIB-Seen-[Gmail]/All )\r\n",
        "* 1 FETCH (UID 9 FLAGS ( ) MODSEQ (7)  )\r\n",
    ] {
        let wire = line.as_bytes();
        assert!(parse(wire).is_ok(), "{line:?}");
        for end in 0..wire.len() {
            assert!(
                matches!(parse(&wire[..end]), Err(ParseError::Syntax(error)) if error.is_incomplete()),
                "{line:?} prefix {end}"
            );
        }
    }
    let (_, response) = parse(b"* OK [HIGHESTMODSEQ 9223372036854775807]\r\n").unwrap();
    assert!(matches!(response, Response::Data { outcome, .. }
        if outcome.code == Some(ResponseCode::HighestModSeq(MAX_MODSEQ))));
    let (_, response) = parse(b"* 1 FETCH (UID 9 FLAGS () MODSEQ (7))\r\n").unwrap();
    assert!(matches!(response, Response::Fetch(1, attrs)
        if attrs == [AttributeValue::Uid(9), AttributeValue::Flags(vec![]), AttributeValue::ModSeq(7)]));
}

#[test]
fn malformed_numbers_and_unrelated_grammars_cannot_yield_evidence() {
    for wire in [
        "* OK [HIGHESTMODSEQ 9223372036854775808]\r\n",
        "* OK [HIGHESTMODSEQ 0]\r\n",
        "* OK [HIGHESTMODSEQ 7 extra]\r\n",
        "* OK [NOMODSEQ extra]\r\n",
        "* OK [UIDNEXT 4294967296]\r\n",
        "* OK [UIDNEXT 0]\r\n",
        "* OK [UIDNEXT 09]\r\n",
        "* OK [UIDVALIDITY 9\r\n",
        "* OK [HIGHESTMODSEQ 7]unseparated\r\n",
        "* 1 FETCH (UID 9 FLAGS () MODSEQ (0))\r\n",
        "* 1 FETCH (MODSEQ (9223372036854775808))\r\n",
        "* 1 FETCH (MODSEQ (18446744073709551615))\r\n",
        "* 1 FETCH (BODY[] {99999999}\r\n",
        "* VANISHED (EARLIER) 1:4294967295\r\n",
    ] {
        assert!(
            matches!(parse(wire.as_bytes()), Err(ParseError::Syntax(error)) if !error.is_incomplete()),
            "{wire:?}"
        );
    }
}

#[test]
fn caps_atoms_flags_and_response_text_before_retaining_owned_capacity() {
    for wire in [
        format!("* CAPABILITY IMAP4rev1{}\r\n", " x".repeat(128)),
        format!("* CAPABILITY IMAP4rev1 {}\r\n", "x".repeat(257)),
        format!(
            "* 1 FETCH (UID 9 FLAGS (x{}) MODSEQ (7))\r\n",
            " x".repeat(128)
        ),
        format!(
            "* 1 FETCH (UID 9 FLAGS ({}) MODSEQ (7))\r\n",
            "x".repeat(257)
        ),
        format!("* OK {}\r\n", "x".repeat(4097)),
    ] {
        assert!(
            matches!(parse(wire.as_bytes()), Err(ParseError::SearchLimit(error)) if error.kind == SearchLimitKind::ParserBytes)
        );
    }
}

#[test]
fn unfinished_tokens_stop_at_their_limits_without_waiting_for_a_delimiter() {
    for (prefix, byte, allowed) in [
        ("* 1 FETCH (UID 9 FLAGS (", b'x', 256),
        ("* 1 FETCH (UID 9 FLAGS (\\", b'x', 255),
        ("* 1 FETCH (UID 9 FLAGS (", b' ', 64),
        ("* 1 FETCH (UID 9 FLAGS (seen", b' ', 64),
        ("* 1 FETCH (UID 9 FLAGS () MODSEQ (7)", b' ', 64),
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

async fn command(server: &mut BufReader<DuplexStream>, expected: &str) -> String {
    let mut line = String::new();
    assert_ne!(server.read_line(&mut line).await.unwrap(), 0);
    let (tag, actual) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
    assert_eq!(actual, expected);
    tag.into()
}

async fn logged_in(server: &mut BufReader<DuplexStream>) {
    let tag = command(server, "LOGIN \"synthetic\" \"local\"").await;
    server
        .write_all(format!("{tag} OK login\r\n").as_bytes())
        .await
        .unwrap();
}

#[tokio::test]
async fn bounds_cumulative_wire_bytes_and_terminal_faults_close_the_stream() {
    const REQUEST: &str = "UID FETCH 9 (UID FLAGS MODSEQ) (CHANGEDSINCE 4)";
    for fault in ["wire", "tag", "bye", "eof", "continuation"] {
        let (client, server) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            logged_in(&mut server).await;
            let tag = command(&mut server, REQUEST).await;
            let data = match fault {
                "wire" => format!("* OK {}\r\n", "x".repeat(4000)).repeat(34),
                "tag" => format!("{tag}x OK wrong\r\n"),
                "bye" => format!("* BYE closed\r\n{tag} OK ignored\r\n"),
                "eof" => "* 1 FETCH (UID 9 FLAGS () MODSEQ (".into(),
                _ => "+ continue\r\n".into(),
            };
            let _ = server.write_all(data.as_bytes()).await;
        });
        let mut session = Client::with_buffer_limit(client, 1024 * 1024)
            .login("synthetic", "local")
            .await
            .unwrap();
        session
            .run_command_with_limits(REQUEST, CommandReadLimits::condstore())
            .await
            .unwrap();
        let failure = loop {
            match session.read_response().await {
                Ok(Some(response)) => assert!(!matches!(response.parsed(), Response::Done { .. })),
                Ok(None) => break None,
                Err(error) => break Some(error),
            }
        };
        if fault == "wire" {
            let error = failure.unwrap();
            let limit = search_limit(&error.into()).unwrap();
            assert_eq!(limit.kind, SearchLimitKind::ResponseBytes);
            assert_eq!(limit.limit, CommandReadLimits::MAX_SEARCH_RESPONSE_BYTES);
        } else if fault != "eof" {
            assert!(failure.is_some(), "{fault}");
        }
        assert!(session.run_command("NOOP").await.is_err(), "{fault}");
        drop(session);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn matching_completion_leaves_following_mailbox_notification_for_next_read() {
    let (client, server) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        logged_in(&mut server).await;
        let tag = command(&mut server, "CAPABILITY").await;
        server
            .write_all(
                format!("* CAPABILITY IMAP4rev1 CONDSTORE\r\n{tag} OK done\r\n* 1 EXISTS\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let tag = command(&mut server, "NOOP").await;
        server
            .write_all(format!("{tag} OK done\r\n").as_bytes())
            .await
            .unwrap();
    });
    let mut session = Client::new(client)
        .login("synthetic", "local")
        .await
        .unwrap();
    let tag = session
        .run_command_with_limits("CAPABILITY", CommandReadLimits::condstore())
        .await
        .unwrap();
    assert!(matches!(
        session.read_response().await.unwrap().unwrap().parsed(),
        Response::Capabilities(_)
    ));
    assert!(
        matches!(session.read_response().await.unwrap().unwrap().parsed(), Response::Done { tag: received, status: Status::Ok, .. } if received == &tag)
    );
    session.noop().await.unwrap();
    server.await.unwrap();
}
