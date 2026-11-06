use async_imap::{Client, error::Error};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn scripted_session(
    expected_command: &'static str,
    data: String,
    status: &'static str,
) -> (
    async_imap::Session<tokio::io::DuplexStream>,
    tokio::task::JoinHandle<()>,
) {
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
        assert_eq!(command, expected_command);
        let completion = if status == "WRONG_TAG" {
            "OTHER OK synthetic\r\n".into()
        } else {
            format!("{tag} {status} synthetic\r\n")
        };
        server
            .write_all(format!("{data}{completion}").as_bytes())
            .await
            .unwrap();
    });
    let session = Client::new(client)
        .login("synthetic", "test-only")
        .await
        .unwrap();
    (session, backend)
}

#[tokio::test]
async fn mailbox_commands_require_matching_tagged_completion() {
    for completion in ["OK", "NO", "WRONG_TAG"] {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (mut session, backend) = scripted_session(
                "EXAMINE \"INBOX\"",
                "* 0 EXISTS\r\n* OK [UIDVALIDITY 99] synthetic\r\n".into(),
                completion,
            )
            .await;
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

#[tokio::test]
async fn capability_lookup_ignores_ascii_case_in_extension_atoms() {
    for advertised in [
        "iMaP4ReV1 move UidPlus auth=PLAIN x-Vendor",
        "IMAP4rev1 MOVE UIDPLUS AUTH=PLAIN X-VENDOR",
    ] {
        let (mut session, backend) =
            scripted_session("CAPABILITY", format!("* CAPABILITY {advertised}\r\n"), "OK").await;
        let capabilities = session.capabilities().await.unwrap();
        backend.await.unwrap();
        for requested in ["MOVE", "uidplus", "imap4rev1", "aUtH=PLAIN", "X-VENDOR"] {
            assert!(capabilities.has_str(requested), "{advertised}: {requested}");
        }
        for missing in ["MOVER", "AUTH=OTHER", "测试"] {
            assert!(!capabilities.has_str(missing), "{advertised}: {missing}");
        }
    }
}
