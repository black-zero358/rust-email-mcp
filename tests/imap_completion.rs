use async_imap::{Client, error::Error};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn mailbox_commands_require_matching_tagged_completion() {
    for completion in ["OK", "NO", "WRONG_TAG"] {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (client, server) = tokio::io::duplex(2048);
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
                let (tag, command) = line.trim_end().split_once(' ').unwrap();
                assert_eq!(command, "EXAMINE \"INBOX\"");
                let completion = if completion == "WRONG_TAG" {
                    "OTHER OK synthetic\r\n".into()
                } else {
                    format!("{tag} {completion} synthetic\r\n")
                };
                server
                    .write_all(
                        format!("* 0 EXISTS\r\n* OK [UIDVALIDITY 99] synthetic\r\n{completion}")
                            .as_bytes(),
                    )
                    .await
                    .unwrap();
            });
            let mut session = Client::new(client)
                .login("synthetic", "test-only")
                .await
                .unwrap();
            let result = session.examine("INBOX").await;
            backend.await.unwrap();
            match completion {
                "OK" => assert_eq!(result.unwrap().uid_validity, Some(99)),
                "NO" => assert!(matches!(result, Err(Error::No(_)))),
                _ => assert!(result.is_err()),
            }
        })
        .await
        .unwrap();
    }
}
