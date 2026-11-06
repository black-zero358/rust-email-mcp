//! Opt-in SEARCH budgets against the real patched parser and async client.
use async_imap::{
    error::search_limit,
    imap_proto::bounded::{SearchLimitError, SearchLimitKind},
    types::CommandReadLimits,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

const QUERY: &str = "UID 1:200 HEADER References \"<thread@example.test>\"";
const WIRE_LIMIT: usize = CommandReadLimits::MAX_SEARCH_RESPONSE_BYTES;

async fn command(server: &mut BufReader<DuplexStream>, expected: &str) -> String {
    let mut line = String::new();
    server.read_line(&mut line).await.unwrap();
    let (tag, actual) = line.trim_end().split_once(' ').unwrap();
    assert_eq!(actual, expected);
    tag.into()
}

async fn login(server: &mut BufReader<DuplexStream>) {
    let tag = command(server, "LOGIN \"synthetic\" \"test-only\"").await;
    server
        .write_all(format!("{tag} OK login\r\n").as_bytes())
        .await
        .unwrap();
}

#[tokio::test]
async fn command_wire_budget_counts_every_frame_through_the_completion() {
    for total in [WIRE_LIMIT, WIRE_LIMIT + 1] {
        let (client, server) = tokio::io::duplex(1024);
        let backend = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            login(&mut server).await;
            let tag = command(&mut server, &format!("UID SEARCH {QUERY}")).await;
            let first = "* 4 EXISTS\r\n";
            let noise_prefix = "* OK private-marker ";
            let search = "* SEARCH 5 7\r\n";
            let completion = format!("{tag} OK completed\r\n");
            let padding =
                total - first.len() - noise_prefix.len() - 2 - search.len() - completion.len();
            let wire = format!(
                "{first}{noise_prefix}{}\r\n{search}{completion}",
                "n".repeat(padding)
            );
            assert_eq!(wire.len(), total);
            for fragment in wire.as_bytes().chunks(317) {
                if server.write_all(fragment).await.is_err() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        });
        let mut session = async_imap::Client::new(client)
            .login("synthetic", "test-only")
            .await
            .unwrap();
        let result = session.uid_search_bounded(QUERY, 200).await;
        if total <= WIRE_LIMIT {
            assert_eq!(result.unwrap(), [5, 7]);
        } else {
            let error = result.unwrap_err();
            assert_eq!(
                search_limit(&error),
                Some(SearchLimitError {
                    kind: SearchLimitKind::ResponseBytes,
                    limit: WIRE_LIMIT,
                    actual: WIRE_LIMIT + 1
                })
            );
            assert!(!format!("{error:?} {error}").contains("private-marker"));
            assert!(session.read_response().await.unwrap().is_none());
        }
        drop(session);
        backend.await.unwrap();
    }
}
