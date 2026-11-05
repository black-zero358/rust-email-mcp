use super::*;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream},
    task::JoinHandle,
};

const EXAMINE: &str = "EXAMINE \"INBOX\" (CONDSTORE)";
const SELECTED: &str = "* 0 EXISTS\r\n* OK [UIDVALIDITY 9]\r\n* OK [UIDNEXT 20]\r\n* OK [HIGHESTMODSEQ 12]\r\n{tag} OK [READ-ONLY] selected\r\n";
const FETCH: &str = "UID FETCH 7,8 (UID FLAGS MODSEQ) (CHANGEDSINCE 4)";

async fn session(
    commands: Vec<(&'static str, &'static str)>,
) -> (Session<DuplexStream>, JoinHandle<()>) {
    let (client, server) = tokio::io::duplex(4096);
    let task = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        for (expected, reply) in
            std::iter::once(("LOGIN \"synthetic\" \"local\"", "{tag} OK login\r\n")).chain(commands)
        {
            let mut line = String::new();
            assert_ne!(server.read_line(&mut line).await.unwrap(), 0);
            let (tag, actual) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
            assert_eq!(actual, expected);
            if server
                .write_all(reply.replace("{tag}", tag).as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
        let mut extra = String::new();
        assert_eq!(server.read_line(&mut extra).await.unwrap(), 0, "{extra}");
    });
    let session = async_imap::Client::new(client)
        .login("synthetic", "local")
        .await
        .unwrap();
    (session, task)
}

#[tokio::test]
async fn capability_and_nomodseq_require_explicit_command_evidence() {
    for (capability, supported) in [
        ("* CAPABILITY IMAP4rev1 QRESYNC\r\n{tag} OK done\r\n", true),
        ("* CAPABILITY IMAP4rev1\r\n{tag} OK done\r\n", false),
    ] {
        let mut script = vec![("CAPABILITY", capability)];
        if supported {
            script.push((EXAMINE, "* OK [UIDVALIDITY 9]\r\n* OK [UIDNEXT 20]\r\n* OK [NOMODSEQ]\r\n{tag} OK [READ-ONLY] done\r\n"));
        }
        let (mut session, peer) = session(script).await;
        assert_eq!(supports_condstore(&mut session).await.unwrap(), supported);
        if supported {
            let (selected, state) = examine(&mut session, &AccountKey::synthetic(), "INBOX")
                .await
                .unwrap();
            assert_eq!(selected.high_uid(), 19);
            assert_eq!(state, ModSeq::Unavailable);
        }
        drop(session);
        peer.await.unwrap();
    }
    let (mut session, peer) =
        session(vec![("CAPABILITY", "{tag} OK no capability data\r\n")]).await;
    assert!(supports_condstore(&mut session).await.is_err());
    assert!(session.run_command("NOOP").await.is_err());
    drop(session);
    peer.await.unwrap();
}

#[tokio::test]
async fn select_evidence_requires_read_only_and_one_untagged_modseq_state() {
    for reply in [
        "* OK [UIDVALIDITY 9]\r\n* OK [UIDNEXT 20]\r\n* OK [HIGHESTMODSEQ 12]\r\n{tag} OK [READ-WRITE] wrong\r\n",
        "* OK [UIDVALIDITY 9]\r\n* OK [UIDNEXT 20]\r\n{tag} OK [READ-ONLY] missing modseq\r\n",
        "* OK [UIDVALIDITY 9]\r\n* OK [UIDNEXT 20]\r\n* OK [HIGHESTMODSEQ 12]\r\n* OK [NOMODSEQ]\r\n{tag} OK [READ-ONLY] conflict\r\n",
        "* OK [UIDVALIDITY 9]\r\n* OK [HIGHESTMODSEQ 12]\r\n{tag} OK [READ-ONLY] missing uidnext\r\n",
        "* OK [UIDVALIDITY 9]\r\n* OK [UIDNEXT 20]\r\n* OK [HIGHESTMODSEQ 12]\r\n{tag} NO rejected\r\n",
    ] {
        let (mut session, peer) = session(vec![(EXAMINE, reply)]).await;
        assert!(
            examine(&mut session, &AccountKey::synthetic(), "INBOX")
                .await
                .is_err()
        );
        assert!(session.run_command("NOOP").await.is_err());
        drop(session);
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn changed_subset_uses_actual_empty_flags_latest_modseq_and_explicit_completion_watermark() {
    let (mut session, peer) = session(vec![
        (EXAMINE, SELECTED),
        (FETCH, "* 1 FETCH (UID 7 FLAGS () MODSEQ (14))\r\n* 1 FETCH (MODSEQ (13) UID 7 FLAGS (\\Seen))\r\n* 4 FETCH (UID 9 FLAGS () MODSEQ (20))\r\n* 3 EXPUNGE\r\n* OK [HIGHESTMODSEQ 12]\r\n{tag} OK [HIGHESTMODSEQ 11] pending\r\n"),
        (FETCH, "{tag} OK no changed flags\r\n"),
    ]).await;
    let (selected, state) = examine(&mut session, &AccountKey::synthetic(), "INBOX")
        .await
        .unwrap();
    assert_eq!(state, ModSeq::Highest(12));
    assert_eq!(
        collect(&mut session, &selected, &[7, 8], 4).await.unwrap(),
        Changed {
            flags: vec![ChangedFlags {
                uid: 7,
                modseq: 14,
                flags: vec![]
            }],
            highest_modseq: Some(11),
            lowest_modseq: Some(11),
            modseq_unavailable: false,
        }
    );
    assert_eq!(
        collect(&mut session, &selected, &[7, 8], 4).await.unwrap(),
        Changed::default()
    );
    drop(session);
    peer.await.unwrap();
}

#[tokio::test]
async fn later_highest_modseq_cannot_hide_a_lower_observation_or_nomodseq() {
    let (mut session, peer) = session(vec![
        (EXAMINE, SELECTED),
        (
            FETCH,
            "* OK [HIGHESTMODSEQ 3]\r\n{tag} OK [HIGHESTMODSEQ 14] done\r\n",
        ),
        (
            FETCH,
            "* OK [NOMODSEQ]\r\n{tag} OK [HIGHESTMODSEQ 14] done\r\n",
        ),
    ])
    .await;
    let (selected, _) = examine(&mut session, &AccountKey::synthetic(), "INBOX")
        .await
        .unwrap();
    let rollback = collect(&mut session, &selected, &[7, 8], 4).await.unwrap();
    assert_eq!(
        (rollback.lowest_modseq, rollback.highest_modseq),
        (Some(3), Some(14))
    );
    assert!(!rollback.modseq_unavailable);
    let revoked = collect(&mut session, &selected, &[7, 8], 4).await.unwrap();
    assert!(revoked.modseq_unavailable);
    drop(session);
    peer.await.unwrap();
}

#[tokio::test]
async fn incomplete_changed_attributes_and_conflicting_duplicate_modseq_discard_connection() {
    for reply in [
        "* 1 FETCH (UID 7 MODSEQ (5))\r\n{tag} OK done\r\n",
        "* 1 FETCH (UID 7 FLAGS ())\r\n{tag} OK done\r\n",
        "* 1 FETCH (UID 7 FLAGS () MODSEQ (4))\r\n{tag} OK done\r\n",
        "* 1 FETCH (UID 7 FLAGS () MODSEQ (5))\r\n* 1 FETCH (UID 7 FLAGS (\\Seen) MODSEQ (5))\r\n{tag} OK done\r\n",
        "* 1 FETCH (UID 7 FLAGS () MODSEQ (5))\r\n{tag} NO failed\r\n",
    ] {
        let (mut session, peer) = session(vec![(EXAMINE, SELECTED), (FETCH, reply)]).await;
        let (selected, _) = examine(&mut session, &AccountKey::synthetic(), "INBOX")
            .await
            .unwrap();
        assert!(collect(&mut session, &selected, &[7, 8], 4).await.is_err());
        assert!(session.run_command("NOOP").await.is_err());
        drop(session);
        peer.await.unwrap();
    }
}
