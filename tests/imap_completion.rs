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
