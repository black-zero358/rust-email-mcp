//! Opt-in SEARCH budgets against the real patched parser and async client.
use async_imap::{
    error::{Error, search_limit},
    imap_proto::{
        MailboxDatum, Response,
        bounded::{BodyStructureLimits, ParseError, SearchLimitError, SearchLimitKind},
    },
    types::CommandReadLimits,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

const QUERY: &str = "UID 1:200 HEADER References \"<thread@example.test>\"";
const WIRE_LIMIT: usize = CommandReadLimits::MAX_SEARCH_RESPONSE_BYTES;

fn parse(input: &[u8], uids: usize) -> Result<(&[u8], Response<'_>), ParseError<'_>> {
    Response::parse_bounded_with_search_limit(input, &BodyStructureLimits::default(), uids)
}

fn search_line(count: usize) -> String {
    let numbers = (1..=count).map(|uid| format!(" {uid}")).collect::<String>();
    format!("* SEARCH{numbers}\r\n")
}

#[test]
fn bounded_search_counts_entries_before_allocating_and_preserves_legal_prefixes() {
    for wire in [
        "* SEARCH\r\n".to_owned(),
        "* seArCh 4294967295   \r\n".to_owned(),
        search_line(199),
        search_line(200),
    ] {
        let bytes = wire.as_bytes();
        assert_eq!(parse(bytes, 200).unwrap(), Response::parse(bytes).unwrap());
        for end in 0..bytes.len() {
            assert!(
                matches!(parse(&bytes[..end],200),Err(ParseError::Syntax(error)) if error.is_incomplete()),
                "prefix length {end}, wire length {}",
                bytes.len(),
            );
        }
    }
    let wire = search_line(201);
    assert_eq!(
        parse(wire.as_bytes(), usize::MAX).unwrap_err(),
        ParseError::SearchLimit(SearchLimitError {
            kind: SearchLimitKind::UidCount,
            limit: 200,
            actual: 201,
        }),
    );
    assert!(Response::parse(wire.as_bytes()).is_ok());
    let prefix = search_line(200);
    for duplicate in [
        format!("{} 200\r\n", prefix.trim_end()),
        format!("* SEARCH 1{}", wire.strip_prefix("* SEARCH").unwrap()),
    ] {
        assert!(matches!(
            parse(duplicate.as_bytes(), 200),
            Err(ParseError::Syntax(error)) if !error.is_incomplete()
        ));
    }
    for malformed in [
        b"* SEARCH 0\r\n".as_slice(),
        b"* SEARCH 01\r\n",
        b"* SEARCH 4294967296\r\n",
        b"* SEARCH 12345678901",
        b"* SEARCH 1  2\r\n",
    ] {
        assert!(
            matches!(parse(malformed, 200),Err(ParseError::Syntax(error)) if !error.is_incomplete())
        );
    }
}

#[test]
fn side_response_grammar_bounds_nested_flag_capacity_before_allocation() {
    for wire in [
        "* 1 FETCH (UID 9 FLAGS (\\Seen \\Flagged ) MODSEQ (7))\r\n",
        "* 1 FETCH (FLAGS ())\r\n",
        "* FLAGS (\\Seen \\*)\r\n",
        "* FLAGS ( )\r\n",
        "* 4 EXISTS  \r\n",
    ] {
        assert_eq!(
            parse(wire.as_bytes(), 200).unwrap(),
            Response::parse(wire.as_bytes()).unwrap()
        );
        for end in 0..wire.len() {
            assert!(
                matches!(parse(&wire.as_bytes()[..end],200),Err(ParseError::Syntax(error)) if error.is_incomplete()),
                "prefix length {end}, wire {wire:?}",
            );
        }
    }
    for wire in [
        format!(
            "* 1 FETCH (UID 9 FLAGS (private-marker{}))\r\n",
            " x".repeat(10_000)
        ),
        format!("* FLAGS (private-marker{})\r\n", " x".repeat(10_000)),
    ] {
        assert!(wire.len() < WIRE_LIMIT);
        assert!(matches!(
            parse(wire.as_bytes(), 200),
            Err(ParseError::SearchLimit(error))
                if error.kind == SearchLimitKind::ParserBytes && error.limit == WIRE_LIMIT
        ));
        assert!(Response::parse(wire.as_bytes()).is_ok());
    }
    let wire = b"* SORT 1 2 3\r\n";
    assert!(matches!(parse(wire, 200), Err(ParseError::Syntax(_))));
    assert!(Response::parse(wire).is_ok());
}

async fn command(server: &mut BufReader<DuplexStream>, expected: &str) -> String {
    let mut line = String::new();
    server.read_line(&mut line).await.unwrap();
    let (tag, actual) = line.trim_end().split_once(' ').unwrap();
    assert_eq!(actual, expected);
    tag.into()
}

async fn login(server: &mut BufReader<DuplexStream>) {
    let tag = command(server, "LOGIN \"synthetic\" \"test-only\"").await;
    server
        .write_all(format!("{tag} OK login\r\n").as_bytes())
        .await
        .unwrap();
}

async fn no_more_commands(server: &mut BufReader<DuplexStream>) {
    let mut extra = String::new();
    assert_eq!(
        server.read_line(&mut extra).await.unwrap_or(0),
        0,
        "{extra}"
    );
}

async fn assert_closed(session: &mut async_imap::Session<DuplexStream>) {
    assert!(session.read_response().await.unwrap().is_none());
    assert!(session.run_command("NOOP").await.is_err());
    assert!(
        session
            .run_command_with_limits("UID SEARCH ALL", CommandReadLimits::search(1024, 10))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn bounded_session_requires_explicit_search_and_matching_tagged_ok() {
    for (data, terminal, success) in [
        ("* SEARCH\r\n", "OK", true),
        ("* 4 EXISTS\r\n* SEARCH 5\r\n", "OK", true),
        ("* 4 EXISTS\r\n", "OK", false),
        ("* SEARCH 5\r\n* SEARCH 6\r\n", "OK", false),
        ("* SEARCH 5 5\r\n", "OK", false),
        ("* SEARCH 5\r\n", "NO", false),
        ("* SEARCH 5\r\n", "BAD", false),
        ("* SEARCH 5\r\n", "WRONG", false),
        ("* SEARCH 5\r\n", "BYE", false),
        ("* SEARCH 5\r\n", "TAGGED-BYE", false),
        ("* SEARCH 5\r\n", "CONTINUE", false),
        ("* SEARCH 5\r\n", "EOF", false),
    ] {
        let (client, server) = tokio::io::duplex(1024);
        let backend = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            login(&mut server).await;
            let tag = command(&mut server, &format!("UID SEARCH {QUERY}")).await;
            let completion = match terminal {
                "WRONG" => "other OK private-marker\r\n".into(),
                "BYE" => "* BYE private-marker\r\n".into(),
                "TAGGED-BYE" => format!("{tag} BYE private-marker\r\n"),
                "CONTINUE" => "+ private-marker\r\n".into(),
                "EOF" => String::new(),
                status => format!("{tag} {status} private-marker\r\n"),
            };
            server
                .write_all(format!("{data}{completion}").as_bytes())
                .await
                .unwrap();
            if terminal == "EOF" {
                server.shutdown().await.unwrap();
            }
            no_more_commands(&mut server).await;
        });
        let mut session = async_imap::Client::new(client)
            .login("synthetic", "test-only")
            .await
            .unwrap();
        let result = session.uid_search_bounded(QUERY, 200).await;
        if success {
            let expected = if data.contains("SEARCH 5") {
                vec![5]
            } else {
                vec![]
            };
            assert_eq!(result.unwrap(), expected);
        } else {
            let error = result.unwrap_err();
            assert!(!format!("{error:?} {error}").contains("private-marker"));
            assert_closed(&mut session).await;
        }
        drop(session);
        backend.await.unwrap();
    }
}

#[tokio::test]
async fn fragmented_entry_limit_is_typed_and_terminal() {
    for count in [199, 200, 201] {
        let (client, server) = tokio::io::duplex(1024);
        let backend = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            login(&mut server).await;
            let tag = command(&mut server, &format!("UID SEARCH {QUERY}")).await;
            let wire = format!(
                "* OK private-marker\r\n{}{tag} OK completed\r\n",
                search_line(count)
            );
            for fragment in wire.as_bytes().chunks(3) {
                if server.write_all(fragment).await.is_err() {
                    return;
                }
                tokio::task::yield_now().await;
            }
            no_more_commands(&mut server).await;
        });
        let mut session = async_imap::Client::new(client)
            .login("synthetic", "test-only")
            .await
            .unwrap();
        let result = session.uid_search_bounded(QUERY, 200).await;
        if count <= 200 {
            assert_eq!(result.unwrap(), (1..=count as u32).collect::<Vec<_>>());
        } else {
            let error = result.unwrap_err();
            assert_eq!(
                search_limit(&error),
                Some(SearchLimitError {
                    kind: SearchLimitKind::UidCount,
                    limit: 200,
                    actual: 201
                })
            );
            assert!(!format!("{error:?} {error}").contains("private-marker"));
            assert_closed(&mut session).await;
        }
        drop(session);
        backend.await.unwrap();
    }
}

#[tokio::test]
async fn duplicate_search_is_protocol_failure_before_the_second_count_limit() {
    let (client, server) = tokio::io::duplex(1024);
    let backend = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        login(&mut server).await;
        command(&mut server, &format!("UID SEARCH {QUERY}")).await;
        // The first line leaves only one entry in the parser budget. If the
        // second line reaches UID parsing, it is misclassified as a shrinkable
        // response count overflow.
        server.write_all(search_line(199).as_bytes()).await.unwrap();
        server.write_all(b"* SeArCh").await.unwrap();
        tokio::task::yield_now().await;
        server.write_all(b" 1 2\r\n").await.unwrap();
        no_more_commands(&mut server).await;
    });
    let mut session = async_imap::Client::new(client)
        .login("synthetic", "test-only")
        .await
        .unwrap();
    let error = session.uid_search_bounded(QUERY, 200).await.unwrap_err();
    assert!(search_limit(&error).is_none());
    assert_closed(&mut session).await;
    drop(session);
    backend.await.unwrap();
}

#[test]
fn bounded_mailbox_metadata_accepts_split_facts_and_rejects_oversized_declarations() {
    let limits = BodyStructureLimits::default();
    let wire = b"* 1 FETCH (UID 9 FLAGS () INTERNALDATE \" 7-Feb-2025 09:00:00 +0000\" RFC822.SIZE 13 BODY[HEADER]<0> {8}\r\nX: a\r\n\r\n)\r\n";
    assert_eq!(
        Response::parse_bounded_mailbox_metadata(wire, &limits, 8).unwrap(),
        Response::parse(wire).unwrap(),
    );
    for end in 0..wire.len() {
        assert!(
            matches!(
                Response::parse_bounded_mailbox_metadata(&wire[..end], &limits, 8),
                Err(ParseError::Syntax(error)) if error.is_incomplete()
            ),
            "prefix length {end}"
        );
    }
    assert!(matches!(
        Response::parse_bounded_mailbox_metadata(b"* 1 FETCH (UID 9 BODY[HEADER]<0> {9}\r\n", &limits, 8),
        Err(ParseError::FetchLiteralLimit(error)) if error.limit == 8 && error.actual == 9
    ));
    assert!(matches!(
        Response::parse_bounded_mailbox_metadata(
            b"* 1 FETCH (UID 9 INTERNALDATE NIL)\r\n",
            &limits,
            8
        ),
        Err(ParseError::Syntax(_))
    ));
}

#[tokio::test]
async fn metadata_command_rejects_precommand_fetch_and_bounds_cumulative_wire() {
    const FETCH: &str = "UID FETCH 7 (UID FLAGS)";
    for precommand in [true, false] {
        let (client, server) = tokio::io::duplex(1024);
        let backend = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            let tag = command(&mut server, "LOGIN \"synthetic\" \"test-only\"").await;
            let prior = if precommand {
                "* 1 FETCH (UID 7 FLAGS ())\r\n"
            } else {
                ""
            };
            server
                .write_all(format!("{tag} OK login\r\n{prior}").as_bytes())
                .await
                .unwrap();
            let tag = command(&mut server, FETCH).await;
            let notices = if precommand {
                String::new()
            } else {
                "* OK synthetic notice\r\n".repeat(12)
            };
            let _ = server
                .write_all(format!("{notices}{tag} OK done\r\n").as_bytes())
                .await;
            no_more_commands(&mut server).await;
        });
        let mut session = async_imap::Client::new(client)
            .login("synthetic", "test-only")
            .await
            .unwrap();
        session
            .run_command_with_limits(FETCH, CommandReadLimits::mailbox_metadata(256, 0))
            .await
            .unwrap();
        let error = loop {
            match session.read_response().await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("bounded command lost its error"),
                Err(error) => break Error::from(error),
            }
        };
        if precommand {
            assert!(search_limit(&error).is_none());
        } else {
            assert_eq!(
                search_limit(&error).unwrap().kind,
                SearchLimitKind::ResponseBytes,
            );
        }
        assert_closed(&mut session).await;
        drop(session);
        backend.await.unwrap();
    }
}

#[tokio::test]
async fn parser_allocation_failure_in_unsolicited_fetch_is_private_and_terminal() {
    let (client, server) = tokio::io::duplex(1024);
    let backend = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        login(&mut server).await;
        command(&mut server, &format!("UID SEARCH {QUERY}")).await;
        let wire = format!(
            "* 1 FETCH (UID 9 FLAGS (private-marker{}))\r\n",
            " x".repeat(10_000)
        );
        let _ = server.write_all(wire.as_bytes()).await;
        no_more_commands(&mut server).await;
    });
    let mut session = async_imap::Client::new(client)
        .login("synthetic", "test-only")
        .await
        .unwrap();
    let error = session.uid_search_bounded(QUERY, 200).await.unwrap_err();
    let limit = search_limit(&error).unwrap();
    assert_eq!(limit.kind, SearchLimitKind::ParserBytes);
    assert_eq!(limit.limit, WIRE_LIMIT);
    assert!(limit.actual > WIRE_LIMIT);
    assert!(!format!("{error:?} {error}").contains("private-marker"));
    assert_closed(&mut session).await;
    drop(session);
    backend.await.unwrap();
}

#[tokio::test]
async fn bounded_collector_consumes_flag_updates_without_retaining_response_buffers() {
    let (client, server) = tokio::io::duplex(1024);
    let backend = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        login(&mut server).await;
        let tag = command(&mut server, &format!("UID SEARCH {QUERY}")).await;
        let flags = "* 1 FETCH (UID 9 FLAGS (\\Seen))\r\n".repeat(120);
        server
            .write_all(format!("{flags}* OK [PERMANENTFLAGS (\\Seen \\*)] notice\r\n* 4 EXISTS\r\n* SEARCH 9\r\n{tag} OK [READ-ONLY] done\r\n").as_bytes())
            .await
            .unwrap();
        no_more_commands(&mut server).await;
    });
    let mut session = async_imap::Client::new(client)
        .login("synthetic", "test-only")
        .await
        .unwrap();
    assert_eq!(session.uid_search_bounded(QUERY, 200).await.unwrap(), [9]);
    assert!(matches!(
        session.unsolicited_responses.try_recv().unwrap(),
        async_imap::types::UnsolicitedResponse::Exists(4)
    ));
    assert!(session.unsolicited_responses.is_empty());
    drop(session);
    backend.await.unwrap();
}

#[tokio::test]
async fn command_wire_budget_counts_every_frame_through_the_completion() {
    for total in [WIRE_LIMIT - 1, WIRE_LIMIT, WIRE_LIMIT + 1] {
        let (client, server) = tokio::io::duplex(1024);
        let backend = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            login(&mut server).await;
            let tag = command(&mut server, &format!("UID SEARCH {QUERY}")).await;
            let first = "* 4 EXISTS\r\n";
            let noise_prefix = "* OK private-marker ";
            let search = "* SEARCH 5 7\r\n";
            let completion = format!("{tag} OK completed\r\n");
            let padding =
                total - first.len() - noise_prefix.len() - 2 - search.len() - completion.len();
            let wire = format!(
                "{first}{noise_prefix}{}\r\n{search}{completion}",
                "n".repeat(padding)
            );
            assert_eq!(wire.len(), total);
            for fragment in wire.as_bytes().chunks(317) {
                if server.write_all(fragment).await.is_err() {
                    return;
                }
                tokio::task::yield_now().await;
            }
            no_more_commands(&mut server).await;
        });
        let mut session = async_imap::Client::new(client)
            .login("synthetic", "test-only")
            .await
            .unwrap();
        let result = session.uid_search_bounded(QUERY, 200).await;
        if total <= WIRE_LIMIT {
            assert_eq!(result.unwrap(), [5, 7]);
        } else {
            let error = result.unwrap_err();
            assert_eq!(
                search_limit(&error),
                Some(SearchLimitError {
                    kind: SearchLimitKind::ResponseBytes,
                    limit: WIRE_LIMIT,
                    actual: WIRE_LIMIT + 1
                })
            );
            assert!(!format!("{error:?} {error}").contains("private-marker"));
            assert_closed(&mut session).await;
        }
        drop(session);
        backend.await.unwrap();
    }
}

#[tokio::test]
async fn declared_literal_in_noise_cannot_make_the_command_allocate_past_budget() {
    let (client, server) = tokio::io::duplex(1024);
    let backend = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        login(&mut server).await;
        command(&mut server, &format!("UID SEARCH {QUERY}")).await;
        server
            .write_all(
                b"* 4 EXISTS\r\n* 1 FETCH (UID 9 BODY[HEADER.FIELDS (private-marker)] {131072}\r\n",
            )
            .await
            .unwrap();
        no_more_commands(&mut server).await;
    });
    let mut session = async_imap::Client::new(client)
        .login("synthetic", "test-only")
        .await
        .unwrap();
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        session.uid_search_bounded(QUERY, 200),
    )
    .await
    .unwrap()
    .unwrap_err();
    let limit = search_limit(&error).unwrap();
    assert_eq!(limit.kind, SearchLimitKind::ResponseBytes);
    assert_eq!(limit.limit, WIRE_LIMIT);
    assert!(limit.actual > WIRE_LIMIT);
    assert!(!format!("{error:?} {error}").contains("private-marker"));
    assert_closed(&mut session).await;
    drop(session);
    backend.await.unwrap();
}

#[tokio::test]
async fn matching_tag_retains_buffered_tail_and_does_not_limit_the_next_ordinary_search() {
    let (client, server) = tokio::io::duplex(1024);
    let backend = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        login(&mut server).await;
        let tag = command(&mut server, &format!("UID SEARCH {QUERY}")).await;
        let completion = format!("* SEARCH 7\r\n{tag} OK complete\r\n");
        server
            .write_all(format!("{completion}* 4 EXISTS\r\n").as_bytes())
            .await
            .unwrap();
        let tag = command(&mut server, "UID SEARCH ALL").await;
        server
            .write_all(format!("{}{tag} OK complete\r\n", search_line(201)).as_bytes())
            .await
            .unwrap();
        no_more_commands(&mut server).await;
    });
    let mut session = async_imap::Client::new(client)
        .login("synthetic", "test-only")
        .await
        .unwrap();
    assert_eq!(session.uid_search_bounded(QUERY, 1).await.unwrap(), [7]);
    assert!(matches!(
        session.read_response().await.unwrap().unwrap().parsed(),
        Response::MailboxData(MailboxDatum::Exists(4))
    ));
    assert_eq!(session.uid_search("ALL").await.unwrap().len(), 201);
    drop(session);
    backend.await.unwrap();
}

#[tokio::test]
async fn lower_limits_and_preexisting_search_data_do_not_escape_the_scope() {
    let (client, server) = tokio::io::duplex(1024);
    let backend = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        login(&mut server).await;
        let tag = command(&mut server, &format!("UID SEARCH {QUERY}")).await;
        server
            .write_all(
                format!("* SEARCH 7\r\n{tag} OK complete\r\n* 4 EXISTS\r\n* SEARCH 8\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        command(&mut server, &format!("UID SEARCH {QUERY}")).await;
        no_more_commands(&mut server).await;
    });
    let mut session = async_imap::Client::new(client)
        .login("synthetic", "test-only")
        .await
        .unwrap();
    assert_eq!(session.uid_search_bounded(QUERY, 1).await.unwrap(), [7]);
    session
        .run_command_with_limits(
            format!("UID SEARCH {QUERY}"),
            CommandReadLimits::search(64, 1),
        )
        .await
        .unwrap();
    assert!(matches!(
        session.read_response().await.unwrap().unwrap().parsed(),
        Response::MailboxData(MailboxDatum::Exists(4))
    ));
    let error = Error::Io(session.read_response().await.unwrap_err());
    assert!(search_limit(&error).is_none());
    assert_closed(&mut session).await;
    drop(session);
    backend.await.unwrap();
}
