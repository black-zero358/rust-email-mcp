//! Bounded ENABLED and VANISHED response parsing and transport.
use async_imap::{
    Client,
    error::search_limit,
    imap_proto::{
        Capability, Response, Status,
        bounded::{BodyStructureLimits, MAX_VANISHED_RANGES, ParseError, SearchLimitKind},
    },
    types::CommandReadLimits,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

fn parse(wire: &[u8]) -> Result<(&[u8], Response<'_>), ParseError<'_>> {
    Response::parse_bounded_qresync(wire, &BodyStructureLimits::default())
}

#[test]
fn compressed_ranges_and_enabled_tokens_preserve_all_legal_prefixes() {
    for line in [
        "* ENABLED\r\n",
        "* ENABLED QrEsYnC CONDSTORE\r\n",
        "* VANISHED (EARLIER) 4294967295:1,42:40,42\r\n",
        "* vanished 12,16:27\r\n",
        "* 8 FETCH (UID 7 FLAGS (\\Seen) MODSEQ (8))\r\n",
        "A2 OK [HIGHESTMODSEQ 8] done\r\n",
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
    let (_, response) = parse(b"* VANISHED (EARLIER) 4294967295:1,42:40,42\r\n").unwrap();
    assert!(
        matches!(response, Response::Vanished { earlier: true, uids }
        if uids == [1..=u32::MAX, 40..=42, 42..=42])
    );
    let (_, response) = parse(b"* ENABLED QRESYNC\r\n").unwrap();
    assert!(matches!(response, Response::Enabled(atoms)
        if atoms == [Capability::Atom("QRESYNC".into())]));
    assert!(
        Response::parse_bounded_condstore(
            b"* ENABLED QRESYNC\r\n",
            &BodyStructureLimits::default()
        )
        .is_err()
    );
    assert!(matches!(
        Response::parse(b"* ENABLED QRESYNC\r\n"),
        Ok((_, Response::Capabilities(_)))
    ));
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
        "* VANISHED \r\n",
        "* VANISHED (EARLIER) \r\n",
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

async fn command(peer: &mut BufReader<DuplexStream>, expected: &str) -> String {
    let mut line = String::new();
    assert_ne!(peer.read_line(&mut line).await.unwrap(), 0);
    let (tag, actual) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
    assert_eq!(actual, expected);
    tag.into()
}

#[tokio::test]
async fn prefetched_enabled_and_vanished_cannot_satisfy_new_commands() {
    for (request, prefetched) in [
        ("ENABLE QRESYNC", "* ENABLED QRESYNC\r\n"),
        (
            "UID FETCH 9 (UID FLAGS MODSEQ) (CHANGEDSINCE 4 VANISHED)",
            "* VANISHED (EARLIER) 9\r\n",
        ),
    ] {
        let (client, peer) = tokio::io::duplex(4096);
        let peer = tokio::spawn(async move {
            let mut peer = BufReader::new(peer);
            let tag = command(&mut peer, "LOGIN \"synthetic\" \"local\"").await;
            peer.write_all(format!("{tag} OK login\r\n{prefetched}").as_bytes())
                .await
                .unwrap();
            let tag = command(&mut peer, request).await;
            let _ = peer
                .write_all(format!("{tag} OK new command\r\n").as_bytes())
                .await;
        });
        let mut session = Client::new(client)
            .login("synthetic", "local")
            .await
            .unwrap();
        session
            .run_command_with_limits(request, CommandReadLimits::qresync())
            .await
            .unwrap();
        assert!(session.read_response().await.is_err());
        assert!(session.run_command("NOOP").await.is_err());
        drop(session);
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn qresync_wire_budget_includes_all_side_responses_and_completion() {
    for fault in ["wire", "wrong-tag", "bye", "eof", "ok"] {
        let (client, peer) = tokio::io::duplex(4096);
        let peer = tokio::spawn(async move {
            let mut peer = BufReader::new(peer);
            let tag = command(&mut peer, "LOGIN \"synthetic\" \"local\"").await;
            peer.write_all(format!("{tag} OK login\r\n").as_bytes())
                .await
                .unwrap();
            let tag = command(&mut peer, "ENABLE QRESYNC").await;
            let reply = match fault {
                "wire" => format!("* OK {}\r\n", "x".repeat(4000)).repeat(34),
                "wrong-tag" => format!("* ENABLED QRESYNC\r\n{tag}x OK wrong tag\r\n"),
                "bye" => format!("* BYE closed\r\n{tag} OK late\r\n"),
                "eof" => "* ENABLED QRE".into(),
                _ => format!("* ENABLED QRESYNC\r\n{tag} OK done\r\n"),
            };
            let _ = peer.write_all(reply.as_bytes()).await;
        });
        let mut session = Client::new(client)
            .login("synthetic", "local")
            .await
            .unwrap();
        let tag = session
            .run_command_with_limits("ENABLE QRESYNC", CommandReadLimits::qresync())
            .await
            .unwrap();
        let mut completed = false;
        let failure = loop {
            match session.read_response().await {
                Ok(Some(response)) => {
                    if matches!(response.parsed(), Response::Done { tag: received, status: Status::Ok, .. } if received == &tag)
                    {
                        completed = true;
                        break None;
                    }
                }
                Ok(None) => break None,
                Err(error) => break Some(error),
            }
        };
        assert_eq!(completed, fault == "ok");
        if fault == "wire" {
            let error = search_limit(&failure.unwrap().into()).unwrap();
            assert_eq!(error.kind, SearchLimitKind::ResponseBytes);
        } else if !matches!(fault, "eof" | "ok") {
            assert!(failure.is_some());
        }
        if fault != "ok" {
            assert!(session.run_command("NOOP").await.is_err());
        }
        drop(session);
        peer.await.unwrap();
    }
}
