use async_imap::imap_proto::{
    Response,
    bounded::{BodyStructureLimits, LimitError, LimitKind, ParseError},
};
const TEXT: &str = "(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1)";
fn fetch(body: &str) -> Vec<u8> {
    format!("* 1 FETCH (UID 7 BODYSTRUCTURE {body})\r\n").into_bytes()
}
fn multipart(children: &str) -> String {
    format!("({children} \"MIXED\")")
}
fn success(input: &[u8]) {
    let (rest, bounded) = Response::parse_bounded(input, &BodyStructureLimits::default()).unwrap();
    let (rest_old, old) = Response::parse(input).unwrap();
    assert_eq!(rest, rest_old);
    assert_eq!(bounded, old);
}
fn limited(input: &[u8], kind: LimitKind) {
    assert_eq!(
        Response::parse_bounded(input, &BodyStructureLimits::default()).unwrap_err(),
        ParseError::Limit(LimitError::new(kind, &BodyStructureLimits::default()))
    );
}
#[test]
fn depth_boundary_before_recursion() {
    let mut body = TEXT.to_string();
    for _ in 0..31 {
        body = multipart(&body);
    }
    success(&fetch(&body));
    body = multipart(&body);
    limited(&fetch(&body), LimitKind::Depth);
}

#[tokio::test]
async fn declared_structure_literal_is_rejected_before_client_buffer_growth() {
    use tokio::io::AsyncWriteExt;
    let (client_io, mut peer) = tokio::io::duplex(256);
    peer.write_all(b"* 1 FETCH (BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"name\" {4294967295}\r\n")
        .await
        .unwrap();
    let mut client = async_imap::Client::with_buffer_limit(client_io, 1024);
    let error = async_imap::error::Error::Io(client.read_response().await.unwrap_err());
    assert_eq!(
        async_imap::error::body_structure_limit(&error),
        Some(LimitError::new(
            LimitKind::StringBytes,
            &BodyStructureLimits::default()
        ))
    );
    assert!(client.read_response().await.unwrap().is_none());
    assert!(
        async_imap::error::body_structure_limit(&async_imap::error::Error::Io(
            std::io::Error::other("ordinary failure")
        ))
        .is_none()
    );
}

const PREFIX_CASES: &[&[u8]] = &[
    include_bytes!("fixtures/body_structure/text.imap"),
    include_bytes!("fixtures/body_structure/message.imap"),
    include_bytes!("fixtures/body_structure/extensions.imap"),
    include_bytes!("fixtures/body_structure/literal.imap"),
];

#[test]
fn all_proper_prefixes_match_the_unmodified_registry_oracle() {
    // Oracle categories come from unmodified registry 0.17.0, not from the
    // legacy entry that shares local helpers with the bounded implementation.
    for raw in PREFIX_CASES {
        for length in 0..raw.len() {
            assert!(
                matches!(Response::parse_bounded(&raw[..length], &BodyStructureLimits::default()),
                Err(ParseError::Syntax(error)) if error.is_incomplete()),
                "prefix {length} of {}",
                raw.len()
            );
        }
        Response::parse_bounded(raw, &BodyStructureLimits::default()).unwrap();
    }
}
