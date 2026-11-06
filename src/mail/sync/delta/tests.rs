use super::*;
use crate::mail::sync::tests::scripted_session;

const EXAMINE: &str = "EXAMINE \"INBOX\" (CONDSTORE)";
const SELECTED: &str = "* 0 EXISTS\r\n* OK [UIDVALIDITY 9]\r\n* OK [UIDNEXT 20]\r\n* OK [HIGHESTMODSEQ 12]\r\n{tag} OK [READ-ONLY] selected\r\n";
const FETCH: &str = "UID FETCH 7,8 (UID FLAGS MODSEQ) (CHANGEDSINCE 4)";
const CONDSTORE: Capabilities = Capabilities {
    condstore: true,
    qresync: false,
    enable: false,
};

#[tokio::test]
async fn incomplete_changed_attributes_and_conflicting_duplicate_modseq_discard_connection() {
    for reply in [
        "* 1 FETCH (UID 7 MODSEQ (5))\r\n{tag} OK done\r\n",
        "* 3 FETCH (FLAGS ())\r\n{tag} OK done\r\n",
        "* 3 FETCH (MODSEQ (5))\r\n{tag} OK done\r\n",
        "* 3 FETCH (FLAGS () FLAGS () MODSEQ (5))\r\n{tag} OK done\r\n",
        "* 3 FETCH (UID 7 UID 8 FLAGS () MODSEQ (5))\r\n{tag} OK done\r\n",
        "* 3 FETCH (UID 0 FLAGS () MODSEQ (5))\r\n{tag} OK done\r\n",
        "* 1 FETCH (UID 7 FLAGS ())\r\n{tag} OK done\r\n",
        "* 1 FETCH (UID 7 FLAGS () MODSEQ (4))\r\n{tag} OK done\r\n",
        "* 1 FETCH (UID 7 FLAGS () MODSEQ (5))\r\n* 1 FETCH (UID 7 FLAGS (\\Seen) MODSEQ (5))\r\n{tag} OK done\r\n",
        "* 1 FETCH (UID 7 FLAGS () MODSEQ (5))\r\n{tag} NO failed\r\n",
    ] {
        let (mut session, peer) =
            scripted_session(vec![(EXAMINE, SELECTED), (FETCH, reply)], false).await;
        let (selected, _) = examine(&mut session, &AccountKey::synthetic(), "INBOX")
            .await
            .unwrap();
        assert!(
            collect(&mut session, &selected, &[7, 8], 4, CONDSTORE)
                .await
                .is_err()
        );
        assert!(session.run_command("NOOP").await.is_err());
        drop(session);
        peer.await.unwrap();
    }
}

const UNSOLICITED: &str = "* 1 FETCH (UID 7 FLAGS () MODSEQ (10))\r\n* 3 FETCH (FLAGS (\\Answered) MODSEQ (12))\r\n* 2 FETCH (UID 8 FLAGS (\\Seen) MODSEQ (11))\r\n{tag} OK done\r\n";

#[tokio::test]
async fn condstore_only_accepts_an_independent_flags_notification_without_uid() {
    for (advertised, pure_condstore) in [
        (
            "* CAPABILITY IMAP4rev1 CONDSTORE\r\n{tag} OK capabilities\r\n",
            true,
        ),
        (
            "* CAPABILITY IMAP4rev1 CONDSTORE QRESYNC\r\n{tag} OK capabilities\r\n",
            false,
        ),
    ] {
        let (mut client, peer) = scripted_session(
            vec![
                ("CAPABILITY", advertised),
                (EXAMINE, SELECTED),
                (FETCH, UNSOLICITED),
            ],
            false,
        )
        .await;
        let caps = capabilities(&mut client).await.unwrap();
        assert!(caps.condstore && !caps.enable);
        assert_eq!(caps.qresync, !pure_condstore);
        let (selected, _) = examine(&mut client, &AccountKey::synthetic(), "INBOX")
            .await
            .unwrap();
        let result = collect(&mut client, &selected, &[7, 8], 4, caps).await;
        drop(client);
        peer.await.unwrap();
        if pure_condstore {
            let changed = result.expect("both targets provided complete UID evidence");
            assert_eq!(
                changed.flags.iter().map(|f| f.uid).collect::<Vec<_>>(),
                [7, 8]
            );
        } else {
            assert!(result.is_err());
        }
    }
}
