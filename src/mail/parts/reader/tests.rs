use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const TEXT: &str = "(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 3 1 NIL NIL)";

enum Fixture {
    Duplicates,
    HeaderBytes,
    HeaderBytesRejected,
    UnsolicitedStructureLimit,
    CommandCount,
}

async fn run(
    body: String,
    fixture: Fixture,
) -> (std::result::Result<PartManifest, FetchFailure>, usize) {
    let (client, server) = tokio::io::duplex(16384);
    let commands = Arc::new(AtomicUsize::new(0));
    let count = commands.clone();
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
        assert!(line.contains("UID FETCH 7 (UID BODYSTRUCTURE)"));
        let tag = line.split_whitespace().next().unwrap();
        server
            .write_all(
                format!("* 1 FETCH (UID 7 BODYSTRUCTURE {body})\r\n{tag} OK complete\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        loop {
            line.clear();
            if server.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
            let tag = line.split_whitespace().next().unwrap().to_owned();
            count.fetch_add(1, Ordering::SeqCst);
            let sections = line
                .split("BODY.PEEK[")
                .skip(1)
                .map(|part| part.split(']').next().unwrap().to_owned())
                .collect::<Vec<_>>();
            assert!(!sections.is_empty() && sections.len() <= SECTIONS_PER_COMMAND);
            if matches!(fixture, Fixture::UnsolicitedStructureLimit) {
                let name = "x".repeat(HEADER_BYTES + 1);
                let body = format!("(\"{name}\" \"PLAIN\" NIL NIL NIL \"7BIT\" 3)");
                let response = format!("* 2 FETCH (UID 8 BODYSTRUCTURE {body})\r\n");
                let _ = server.write_all(response.as_bytes()).await;
                return;
            }
            for section in sections {
                let headers = match fixture {
                    Fixture::Duplicates => b"Content-Type: multipart/mixed; boundary=m\r\nContent-Type: application/pdf\r\nContent-Disposition: inline\r\nContent-Disposition: attachment\r\n\r\n".to_vec(),
                    Fixture::HeaderBytes | Fixture::HeaderBytesRejected if section != "HEADER" => {
                        let mut header = b"Content-Type: text/plain\r\nX-Pad: ".to_vec();
                        header.resize(HEADER_BYTES - 4, b'x');
                        header.extend(b"\r\n\r\n");
                        header
                    }
                    Fixture::CommandCount if section != "HEADER" && section.split('.').count() == 32 => b"Content-Type: text/plain\r\n\r\n".to_vec(),
                    _ if section == "HEADER" || matches!(fixture, Fixture::CommandCount) => b"Content-Type: multipart/mixed; boundary=m\r\n\r\n".to_vec(),
                    _ => unreachable!(),
                };
                let response = format!(
                    "* 1 FETCH (UID 7 BODY[{section}]<0> {{{}}}\r\n",
                    headers.len()
                );
                if server.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
                if server.write_all(&headers).await.is_err() {
                    return;
                }
                if server.write_all(b")\r\n").await.is_err() {
                    return;
                }
            }
            let status = if count.load(Ordering::SeqCst) == 2
                && matches!(fixture, Fixture::HeaderBytesRejected)
            {
                "NO"
            } else {
                "OK"
            };
            if server
                .write_all(format!("{tag} {status} complete\r\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
    });
    let mut session = async_imap::Client::with_buffer_limit(client, 2 * 1024 * 1024)
        .login("synthetic", "test-only")
        .await
        .unwrap();
    let result = manifest(&mut session, 7).await;
    drop(session);
    backend.await.unwrap();
    (result, commands.load(Ordering::SeqCst))
}

#[tokio::test]
async fn reader_requires_original_headers_for_a_server_reclassified_basic_attachment() {
    let body =
        "(\"APPLICATION\" \"PDF\" NIL NIL NIL \"7BIT\" 3 NIL (\"ATTACHMENT\" NIL))".to_owned();
    let (result, commands) = run(body, Fixture::Duplicates).await;
    assert!(matches!(
        result,
        Err(FetchFailure::Fallback {
            reason: Fallback::MetadataConflict,
            synchronized: true
        })
    ));
    assert_eq!(commands, 1);
}

#[tokio::test]
async fn header_evidence_uses_one_shared_mebibyte_across_the_frontier() {
    let body = format!("({} \"MIXED\" (\"BOUNDARY\" \"m\") NIL)", TEXT.repeat(9));
    let (result, commands) = run(body, Fixture::HeaderBytes).await;
    assert!(matches!(
        result,
        Err(FetchFailure::Fallback {
            reason: Fallback::HeaderBudget,
            synchronized: true
        })
    ));
    assert_eq!(commands, 2);
}

#[tokio::test]
async fn header_budget_requires_matching_ok_before_allowing_raw_fallback() {
    let body = format!("({} \"MIXED\" (\"BOUNDARY\" \"m\") NIL)", TEXT.repeat(9));
    let (result, commands) = run(body, Fixture::HeaderBytesRejected).await;
    assert!(matches!(result, Err(FetchFailure::Imap(ImapError::No(_)))));
    assert_eq!(commands, 2);
}

#[tokio::test]
async fn unsolicited_structure_budget_during_header_fetch_is_terminal() {
    let (result, commands) = run(TEXT.to_owned(), Fixture::UnsolicitedStructureLimit).await;
    assert!(matches!(
        result,
        Err(FetchFailure::Imap(error)) if async_imap::error::body_structure_limit(&error).is_some()
    ));
    assert_eq!(commands, 1);
}

#[tokio::test]
async fn nested_header_frontiers_stop_at_the_shared_command_limit() {
    let mut body = format!("({} \"MIXED\" (\"BOUNDARY\" \"m\") NIL)", TEXT.repeat(900));
    for _ in 0..30 {
        body = format!("({body} \"MIXED\" (\"BOUNDARY\" \"m\") NIL)");
    }
    let (result, commands) = run(body, Fixture::CommandCount).await;
    assert!(matches!(
        result,
        Err(FetchFailure::Fallback {
            reason: Fallback::HeaderBudget,
            synchronized: true
        })
    ));
    assert_eq!(commands, HEADER_COMMANDS);
}
