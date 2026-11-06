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
