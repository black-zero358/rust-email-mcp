use super::*;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream},
    task::JoinHandle,
};

const ENABLE: (&str, &str) = (
    "ENABLE QRESYNC",
    "* ENABLED QRESYNC\r\n{tag} OK enabled\r\n",
);

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
