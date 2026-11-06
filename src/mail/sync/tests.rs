use super::{
    delta::ModSeq,
    reconcile::{self, Mode},
    scan::Selected,
};
use crate::accounts::AccountKey;
use async_imap::Session;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream},
    task::JoinHandle,
};

async fn command(server: &mut BufReader<DuplexStream>, expected: &str) -> String {
    let mut line = String::new();
    assert_ne!(server.read_line(&mut line).await.unwrap(), 0);
    let (tag, actual) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
    assert_eq!(actual, expected);
    tag.into()
}

async fn selected(
    account: &AccountKey,
    validity: u32,
    uid_next: u32,
    reply: Option<(&str, Vec<u8>, &str)>,
) -> (Session<DuplexStream>, Selected, JoinHandle<()>) {
    let (client, server) = tokio::io::duplex(4096);
    let reply =
        reply.map(|(command, bytes, status)| (command.to_owned(), bytes, status.to_owned()));
    let task = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        let tag = command(&mut server, "LOGIN \"synthetic\" \"local\"").await;
        server
            .write_all(format!("{tag} OK login\r\n").as_bytes())
            .await
            .unwrap();
        let tag = command(&mut server, "EXAMINE \"INBOX\"").await;
        server.write_all(format!(
            "* 0 EXISTS\r\n* OK [UIDVALIDITY {validity}] valid\r\n* OK [UIDNEXT {uid_next}] next\r\n{tag} OK [READ-ONLY] selected\r\n"
        ).as_bytes()).await.unwrap();
        if let Some((expected, bytes, status)) = reply {
            let tag = command(&mut server, &expected).await;
            if server.write_all(&bytes).await.is_err() {
                return;
            }
            if status == "EOF" {
                server.shutdown().await.unwrap();
                return;
            }
            if status != "HANG"
                && server
                    .write_all(format!("{tag} {status} done\r\n").as_bytes())
                    .await
                    .is_err()
            {
                return;
            }
        }
        let mut extra = String::new();
        assert_eq!(server.read_line(&mut extra).await.unwrap(), 0, "{extra}");
    });
    let mut session = async_imap::Client::with_buffer_limit(client, 2 * 1024 * 1024)
        .login("synthetic", "local")
        .await
        .unwrap();
    let (observed, _) = Selected::examine(&mut session, account, "INBOX")
        .await
        .unwrap();
    assert!(observed.matches_account(account));
    (session, observed, task)
}

#[tokio::test]
async fn condstore_baseline_retains_explicit_floor_or_revocation_after_tagged_completion() {
    let account = AccountKey::synthetic();
    let flags = "UID FETCH 7,8 (UID FLAGS)";
    for (data, status, expected) in [
        (
            "* OK [HIGHESTMODSEQ 90]\r\n* 1 FETCH (UID 7 FLAGS ())\r\n",
            "OK [HIGHESTMODSEQ 110]",
            Some(ModSeq::Highest(90)),
        ),
        (
            "* OK [NOMODSEQ]\r\n* 1 FETCH (UID 7 FLAGS ())\r\n",
            "OK [HIGHESTMODSEQ 110]",
            Some(ModSeq::Unavailable),
        ),
        (
            "* OK [HIGHESTMODSEQ 90]\r\n* 1 FETCH (UID 7 FLAGS ())\r\n",
            "NO",
            None,
        ),
    ] {
        let (mut session, selected, peer) = selected(
            &account,
            9,
            20,
            Some((flags, data.as_bytes().to_vec(), status)),
        )
        .await;
        let result = reconcile::collect_condstore_flags(&mut session, &selected, &[7, 8]).await;
        if let Some(expected) = expected {
            let proof = result.unwrap();
            assert_eq!(proof.baseline_modseq, Some(expected));
            assert_eq!(proof.absent, [8]);
            assert_eq!(proof.present[0].uid, 7);
            assert!(proof.present[0].flags.is_empty());
        } else {
            assert!(result.is_err());
        }
        drop(session);
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn facts_merge_target_fragments_with_exact_header_origin_and_all_attributes() {
    let account = AccountKey::synthetic();
    let command = "UID FETCH 7 (UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER]<0.131073>)";
    for (origin, include_date, good) in [(0, true, true), (1, true, false), (0, false, false)] {
        let date = if include_date {
            " INTERNALDATE \" 7-Feb-2025 09:00:00 +0000\""
        } else {
            ""
        };
        let data = format!(
            "* 1 FETCH (UID 7 BODY[HEADER]<{origin}> {{8}}\r\nX: a\r\n\r\n)\r\n* 1 FETCH (UID 7 RFC822.SIZE 8 FLAGS (\\Seen){date})\r\n"
        );
        let (mut session, observed, server) =
            selected(&account, 7, 9, Some((command, data.into_bytes(), "OK"))).await;
        let result = reconcile::collect(&mut session, &observed, &[7], Mode::Facts).await;
        drop(session);
        server.await.unwrap();
        if good {
            let proof = result.unwrap();
            assert!(proof.absent.is_empty());
            let [item] = proof.present.as_slice() else {
                panic!("missing target facts")
            };
            assert_eq!(item.size, Some(8));
            assert_eq!(item.flags, ["Seen"]);
            assert!(
                item.internal_date
                    .as_deref()
                    .unwrap()
                    .starts_with("2025-02-07")
            );
            let header = item.header.as_ref().unwrap();
            assert!(header.complete);
            assert_eq!(header.bytes.as_ref(), b"X: a\r\n\r\n");
        } else {
            assert!(result.is_err());
        }
    }
}
