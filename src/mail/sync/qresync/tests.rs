use super::*;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream},
    task::JoinHandle,
};

const ENABLE: (&str, &str) = (
    "ENABLE QRESYNC",
    "* ENABLED QRESYNC\r\n{tag} OK enabled\r\n",
);
const EXAMINE: &str = "EXAMINE \"INBOX\" (CONDSTORE)";
const OPEN: &str = "* 0 EXISTS\r\n* OK [UIDVALIDITY 9]\r\n* OK [UIDNEXT 100]\r\n* OK [HIGHESTMODSEQ 12]\r\n{tag} OK [READ-ONLY] selected\r\n";
const FETCH: &str = "UID FETCH 2,4 (UID FLAGS MODSEQ) (CHANGEDSINCE 4 VANISHED)";

async fn session(
    commands: Vec<(&'static str, &'static str)>,
) -> (Session<DuplexStream>, JoinHandle<()>) {
    let (client, peer) = tokio::io::duplex(4096);
    let peer = tokio::spawn(async move {
        let mut peer = BufReader::new(peer);
        for (expected, reply) in
            std::iter::once(("LOGIN \"synthetic\" \"local\"", "{tag} OK login\r\n")).chain(commands)
        {
            let mut line = String::new();
            assert_ne!(peer.read_line(&mut line).await.unwrap(), 0);
            let (tag, actual) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
            assert_eq!(actual, expected);
            if peer
                .write_all(reply.replace("{tag}", tag).as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
        let mut extra = String::new();
        assert_eq!(peer.read_line(&mut extra).await.unwrap(), 0, "{extra}");
    });
    let session = async_imap::Client::new(client)
        .login("synthetic", "local")
        .await
        .unwrap();
    (session, peer)
}

#[tokio::test]
async fn enable_requires_positive_evidence_on_the_same_connection() {
    let (mut client, peer) = session(vec![
        (
            "CAPABILITY",
            "* CAPABILITY IMAP4rev1 ENABLE qReSyNc\r\n{tag} OK capability\r\n",
        ),
        ENABLE,
        (EXAMINE, OPEN),
    ])
    .await;
    let capabilities = delta::capabilities(&mut client).await.unwrap();
    assert!(capabilities.condstore && capabilities.qresync && capabilities.enable);
    let opened = enable(client)
        .await
        .unwrap()
        .examine_plain(&AccountKey::synthetic(), "INBOX")
        .await
        .unwrap();
    assert_eq!(opened.modseq, ModSeq::Highest(12));
    drop(opened);
    peer.await.unwrap();

    for response in [
        "{tag} OK no evidence\r\n",
        "* ENABLED CONDSTORE\r\n{tag} OK wrong extension\r\n",
        "* CAPABILITY IMAP4rev1 QRESYNC\r\n{tag} OK wrong kind\r\n",
        "* ENABLED QRESYNC\r\n* ENABLED QRESYNC\r\n{tag} OK duplicate\r\n",
        "* ENABLED QRESYNC\r\n{tag} BAD rejected\r\n",
    ] {
        let (client, peer) = session(vec![(ENABLE.0, response)]).await;
        assert!(enable(client).await.is_err());
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn known_examine_merges_ranges_and_retains_realtime_order_and_watermarks() {
    let (client, peer) = session(vec![ENABLE, (
        "EXAMINE \"INBOX\" (QRESYNC (9 4 1:3,7,98))",
        "* OK [UIDVALIDITY 9]\r\n* OK [UIDNEXT 100]\r\n* OK [HIGHESTMODSEQ 12]\r\n* VANISHED (EARLIER) 3:1,2\r\n* 1 FETCH (UID 7 FLAGS (\\Seen) MODSEQ (14))\r\n* VANISHED 90,7\r\n* 7 FETCH (UID 92 MODSEQ (14))\r\n* 8 FETCH (UID 98 FLAGS () MODSEQ (15))\r\n* OK [HIGHESTMODSEQ 11]\r\n* OK [HIGHESTMODSEQ 13]\r\n{tag} OK [READ-ONLY] selected\r\n",
    )]).await;
    let opened = enable(client)
        .await
        .unwrap()
        .examine_known(&AccountKey::synthetic(), "INBOX", 9, 4, &[1, 2, 3, 7, 98])
        .await
        .unwrap();
    assert_eq!(opened.modseq, ModSeq::Highest(12));
    assert_eq!(opened.evidence.highest_modseq, Some(13));
    assert_eq!(opened.evidence.lowest_modseq, Some(11));
    assert_eq!(opened.evidence.earlier, std::slice::from_ref(&(1..=3)));
    assert_eq!(opened.evidence.live, [7..=7, 90..=90]);
    assert_eq!(
        opened.evidence.flags,
        [ChangedFlags {
            uid: 98,
            modseq: 15,
            flags: vec![]
        }]
    );
    drop(opened);
    peer.await.unwrap();
}

#[tokio::test]
async fn empty_and_sparse_uid_pages_never_request_an_entire_mailbox() {
    let (client, peer) = session(vec![ENABLE, (EXAMINE, OPEN), (FETCH,
        "* VANISHED (EARLIER) 2\r\n* VANISHED 4294967295:100,105:106\r\n* 1 FETCH (UID 4 FLAGS () MODSEQ (15))\r\n{tag} OK [HIGHESTMODSEQ 12] completed\r\n",
    )]).await;
    let mut opened = enable(client)
        .await
        .unwrap()
        .examine_plain(&AccountKey::synthetic(), "INBOX")
        .await
        .unwrap();
    assert_eq!(opened.collect(&[], 4).await.unwrap(), Evidence::default());
    let evidence = opened.collect(&[2, 4], 4).await.unwrap();
    assert_eq!(evidence.earlier, std::slice::from_ref(&(2..=2)));
    assert_eq!(evidence.live, std::slice::from_ref(&(100..=u32::MAX)));
    assert_eq!(evidence.flags[0].uid, 4);
    drop(opened);
    peer.await.unwrap();
}

#[tokio::test]
async fn rejects_late_earlier_inexact_sets_and_deletion_conflicts() {
    for reply in [
        "* 1 FETCH (UID 4 FLAGS () MODSEQ (7))\r\n* VANISHED (EARLIER) 2\r\n{tag} OK late earlier\r\n",
        "* VANISHED (EARLIER) 2:4\r\n{tag} OK not requested\r\n",
        "* 1 FETCH (UID 4 MODSEQ (7))\r\n{tag} OK missing requested flags\r\n",
        "* VANISHED 4\r\n* 1 FETCH (UID 4 FLAGS () MODSEQ (7))\r\n{tag} OK resurrected\r\n",
        "* VANISHED (EARLIER) 2\r\n* VANISHED 2\r\n{tag} OK both phases\r\n",
        "* 1 EXPUNGE\r\n{tag} OK missing UID\r\n",
        "* VANISHED (EARLIER) 2\r\n{tag} NO failed\r\n",
    ] {
        let (client, peer) = session(vec![ENABLE, (EXAMINE, OPEN), (FETCH, reply)]).await;
        let mut opened = enable(client)
            .await
            .unwrap()
            .examine_plain(&AccountKey::synthetic(), "INBOX")
            .await
            .unwrap();
        assert!(opened.collect(&[2, 4], 4).await.is_err());
        assert!(opened.session.run_command("NOOP").await.is_err());
        drop(opened);
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn a_new_epoch_or_nomodseq_never_returns_old_delta_evidence() {
    for (reply, epoch, state) in [
        (
            "* OK [UIDVALIDITY 10]\r\n* OK [UIDNEXT 2]\r\n* OK [HIGHESTMODSEQ 8]\r\n* VANISHED (EARLIER) 4\r\n{tag} OK [READ-ONLY] epoch changed\r\n",
            10,
            ModSeq::Highest(8),
        ),
        (
            "* OK [UIDVALIDITY 9]\r\n* OK [UIDNEXT 5]\r\n* OK [NOMODSEQ]\r\n* VANISHED (EARLIER) 4\r\n{tag} OK [READ-ONLY] no modseq\r\n",
            9,
            ModSeq::Unavailable,
        ),
    ] {
        let (client, peer) =
            session(vec![ENABLE, ("EXAMINE \"INBOX\" (QRESYNC (9 4 4))", reply)]).await;
        let opened = enable(client)
            .await
            .unwrap()
            .examine_known(&AccountKey::synthetic(), "INBOX", 9, 4, &[4])
            .await
            .unwrap();
        assert_eq!(
            (opened.selected.uid_validity, opened.modseq),
            (epoch, state)
        );
        assert!(opened.evidence.earlier.is_empty());
        assert!(opened.evidence.live.is_empty());
        assert!(opened.evidence.flags.is_empty());
        drop(opened);
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn completed_nomodseq_preserves_only_realtime_deletion_for_the_scope_fence() {
    let (client, peer) = session(vec![ENABLE, (EXAMINE, OPEN), (FETCH,
        "* VANISHED (EARLIER) 2\r\n* VANISHED 90\r\n* 1 FETCH (UID 4 FLAGS () MODSEQ (15))\r\n* OK [NOMODSEQ]\r\n{tag} OK completed\r\n",
    )]).await;
    let mut opened = enable(client)
        .await
        .unwrap()
        .examine_plain(&AccountKey::synthetic(), "INBOX")
        .await
        .unwrap();
    let evidence = opened.collect(&[2, 4], 4).await.unwrap();
    assert!(evidence.modseq_unavailable);
    assert!(evidence.earlier.is_empty());
    assert!(evidence.flags.is_empty());
    assert_eq!(evidence.live.as_slice(), std::slice::from_ref(&(90..=90)));
    drop(opened);
    peer.await.unwrap();
}
