//! Command-scoped payload limits preserve normal raw reads and legal section syntax.
use async_imap::types::CommandReadLimits;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn typed_literal_error_closes_the_stream_and_contains_no_response_data() {
    let (client, server) = tokio::io::duplex(1024);
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
        server
            .write_all(b"* 1 FETCH (UID 99 BODY[HEADER.FIELDS (private_marker)]<0> {65537}\r\n")
            .await
            .unwrap();
    });
    let mut session = async_imap::Client::with_buffer_limit(client, 128 * 1024)
        .login("synthetic", "test-only")
        .await
        .unwrap();
    session
        .run_command_with_limits(
            "UID FETCH 7 (UID BODY.PEEK[2]<0.65536>)",
            CommandReadLimits::body_section_literal_bytes(65536),
        )
        .await
        .unwrap();
    let error = async_imap::error::Error::Io(session.read_response().await.unwrap_err());
    assert_eq!(
        async_imap::error::body_section_literal_limit(&error)
            .unwrap()
            .limit,
        65536
    );
    assert!(!format!("{error:?}").contains("private_marker"));
    assert!(session.read_response().await.unwrap().is_none());
    assert!(session.run_command("NOOP").await.is_err());
    backend.await.unwrap();
}
