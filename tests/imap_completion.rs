use async_imap::{Session, error::Error};
use futures::TryStreamExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

async fn collect(session: &mut Session<DuplexStream>, op: &str) -> Result<usize, Error> {
    Ok(match op {
        "UID STORE" => session
            .uid_store("7", "+FLAGS.SILENT (\\Seen)")
            .await?
            .try_collect::<Vec<_>>()
            .await?
            .len(),
        "UID EXPUNGE" => session
            .uid_expunge("7")
            .await?
            .try_collect::<Vec<_>>()
            .await?
            .len(),
        "LIST" => session
            .list(Some(""), Some("*"))
            .await?
            .try_collect::<Vec<_>>()
            .await?
            .len(),
        "UID SEARCH" => session.uid_search("UID 1:500 TEXT synthetic").await?.len(),
        "UID FETCH" => session
            .uid_fetch("7", "(UID FLAGS)")
            .await?
            .try_collect::<Vec<_>>()
            .await?
            .len(),
        _ => unreachable!(),
    })
}

#[tokio::test]
async fn completion_matrix_rejects_failure_after_empty_or_partial_results() {
    for (op, row) in [
        ("UID STORE", "* 1 FETCH (UID 7 FLAGS (\\Seen))\r\n"),
        ("UID EXPUNGE", "* 1 EXPUNGE\r\n"),
        ("LIST", "* LIST () \"/\" \"INBOX\"\r\n"),
        ("UID SEARCH", "* SEARCH 7\r\n"),
        ("UID FETCH", "* 1 FETCH (UID 7 FLAGS (\\Seen))\r\n"),
    ] {
        for status in ["OK", "NO", "BAD", "EOF", "WRONG_TAG"] {
            for partial in [false, true] {
                let (client, server) = tokio::io::duplex(8192);
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
                    let (tag, command) = line.split_once(' ').unwrap();
                    assert!(command.starts_with(op), "{command}");
                    let mut response = if partial {
                        row.to_owned()
                    } else {
                        String::new()
                    };
                    match status {
                        "EOF" => {}
                        "WRONG_TAG" => response.push_str("OTHER OK wrong command\r\n"),
                        _ => response.push_str(&format!("{tag} {status} completion\r\n")),
                    }
                    server.write_all(response.as_bytes()).await.unwrap();
                });
                let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    let mut session = async_imap::Client::new(client)
                        .login("synthetic", "test-only")
                        .await
                        .unwrap();
                    collect(&mut session, op).await
                })
                .await
                .unwrap();
                backend.await.unwrap();
                match status {
                    "OK" => assert_eq!(result.unwrap(), usize::from(partial), "{op}"),
                    "NO" => assert!(matches!(result, Err(Error::No(_))), "{op}: {result:?}"),
                    "BAD" => assert!(matches!(result, Err(Error::Bad(_))), "{op}: {result:?}"),
                    _ => assert!(result.is_err(), "{op}/{status}/{partial}: {result:?}"),
                }
            }
        }
    }
}

#[tokio::test]
async fn mailbox_commands_require_matching_tagged_completion() {
    for op in ["EXAMINE", "SELECT", "SELECT_CONDSTORE", "STATUS"] {
        for status in ["OK", "NO", "BAD", "EOF", "WRONG_TAG"] {
            for partial in [false, true] {
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    let (client, server) = tokio::io::duplex(2048);
                    let backend = tokio::spawn(async move {
                        let mut server = BufReader::new(server);
                        let mut line = String::new();
                        server.read_line(&mut line).await.unwrap();
                        let tag = line.split_whitespace().next().unwrap();
                        server.write_all(format!("{tag} OK login\r\n").as_bytes()).await.unwrap();
                        line.clear();
                        server.read_line(&mut line).await.unwrap();
                        let (tag, command) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
                        let expected = match op {
                            "SELECT_CONDSTORE" => "SELECT \"INBOX\" (CONDSTORE)",
                            "STATUS" => "STATUS \"INBOX\" (MESSAGES UIDVALIDITY UIDNEXT)",
                            "SELECT" => "SELECT \"INBOX\"",
                            _ => "EXAMINE \"INBOX\"",
                        };
                        assert_eq!(command, expected);
                        let mut reply = String::new();
                        if partial {
                            reply.push_str(if op == "STATUS" {
                                "* STATUS INBOX (MESSAGES 0 UIDVALIDITY 99 UIDNEXT 1)\r\n"
                            } else {
                                "* 0 EXISTS\r\n* OK [UIDVALIDITY 99] synthetic\r\n* OK [UIDNEXT 1] synthetic\r\n"
                            });
                        }
                        match status {
                            "EOF" => {}
                            "WRONG_TAG" => reply.push_str("OTHER OK synthetic\r\n"),
                            _ => reply.push_str(&format!("{tag} {status} synthetic\r\n")),
                        }
                        server.write_all(reply.as_bytes()).await.unwrap();
                    });
                    let mut session = async_imap::Client::new(client).login("synthetic", "test-only").await.unwrap();
                    let result = match op {
                        "EXAMINE" => session.examine("INBOX").await,
                        "SELECT" => session.select("INBOX").await,
                        "SELECT_CONDSTORE" => session.select_condstore("INBOX").await,
                        _ => session.status("INBOX", "(MESSAGES UIDVALIDITY UIDNEXT)").await,
                    };
                    backend.await.unwrap();
                    match status {
                        "OK" => {
                            let mailbox = result.unwrap();
                            assert_eq!(mailbox.exists, 0);
                            assert_eq!(mailbox.uid_validity, partial.then_some(99));
                            assert_eq!(mailbox.uid_next, partial.then_some(1));
                        }
                        "NO" => assert!(matches!(result, Err(Error::No(_))), "{op}: {result:?}"),
                        "BAD" => assert!(matches!(result, Err(Error::Bad(_))), "{op}: {result:?}"),
                        _ => assert!(result.is_err(), "{op}/{status}/{partial}: {result:?}"),
                    }
                }).await.unwrap();
            }
        }
    }
}
