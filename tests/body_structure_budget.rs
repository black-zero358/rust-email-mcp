use async_imap::imap_proto::{
    Response,
    bounded::{BodyStructureLimits, LimitError, LimitKind, ParseError},
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
thread_local! {static TRACK:Cell<bool>=const{Cell::new(false)};static MAX:Cell<usize>=const{Cell::new(0)};}
struct Allocator;
// SAFETY: every allocation operation delegates unchanged to System; counters
// only read thread-local Cells and perform no allocation.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        TRACK.with(|t| {
            if t.get() {
                MAX.with(|m| m.set(m.get().max(l.size())))
            }
        });
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        TRACK.with(|t| {
            if t.get() {
                MAX.with(|m| m.set(m.get().max(n)))
            }
        });
        unsafe { System.realloc(p, l, n) }
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
const TEXT: &str = "(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1)";
fn fetch(body: &str) -> Vec<u8> {
    format!("* 1 FETCH (UID 7 BODYSTRUCTURE {body})\r\n").into_bytes()
}
fn multipart(children: &str) -> String {
    format!("({children} \"MIXED\")")
}
fn pairs(count: usize) -> String {
    format!(
        "({})",
        std::iter::repeat_n("\"x\" \"y\"", count)
            .collect::<Vec<_>>()
            .join(" ")
    )
}
fn params(count: usize, disposition: usize) -> String {
    format!(
        "(\"TEXT\" \"PLAIN\" {} NIL NIL \"7BIT\" 1 1 NIL (\"attachment\" {}))",
        pairs(count),
        if disposition == 0 {
            "NIL".into()
        } else {
            pairs(disposition)
        }
    )
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
fn envelope(addresses: usize) -> String {
    format!(
        "(NIL NIL ({}) NIL NIL NIL NIL NIL NIL NIL)",
        "(NIL NIL \"x\" \"host\")".repeat(addresses)
    )
}
fn message(addresses: usize) -> String {
    format!(
        "(\"MESSAGE\" \"RFC822\" NIL NIL NIL \"7BIT\" 1 {} {TEXT} 1)",
        envelope(addresses)
    )
}
#[test]
fn old_response_api_and_ordinary_fetch_are_preserved() {
    for raw in [b"* 1 FETCH (UID 7 FLAGS () RFC822.SIZE 8 INTERNALDATE \"01-Jan-2000 00:00:00 +0000\" SAVEDATE NIL)\r\n".as_slice(),b"* LIST (\\HasNoChildren) \"/\" INBOX\r\n",b"A1 OK finished\r\n",b"* STATUS INBOX (MESSAGES 0 UIDVALIDITY 9)\r\n"]{success(raw)}
    success(&fetch(TEXT));
    success(&fetch(&multipart(&format!("{TEXT}{TEXT}"))));
    success(&fetch(&message(1)));
    success(&fetch(&params(2, 2)));
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
#[test]
fn nodes_boundary_before_vector_growth() {
    success(&fetch(&multipart(&TEXT.repeat(1023))));
    limited(&fetch(&multipart(&TEXT.repeat(1024))), LimitKind::Nodes);
}
#[test]
fn per_node_parameters_include_disposition() {
    success(&fetch(&params(128, 128)));
    limited(&fetch(&params(129, 128)), LimitKind::NodeParameters);
}
#[test]
fn total_parameters_are_shared_across_children() {
    let body = params(256, 0);
    success(&fetch(&multipart(&body.repeat(32))));
    limited(
        &fetch(&multipart(&format!("{}{}", body.repeat(32), params(1, 0)))),
        LimitKind::TotalParameters,
    );
}
#[test]
fn single_string_exact_boundary() {
    let body = |n| {
        format!(
            "(\"TEXT\" \"PLAIN\" (\"x\" \"{}\") NIL NIL \"7BIT\" 1 1)",
            "v".repeat(n)
        )
    };
    success(&fetch(&body(128 * 1024)));
    limited(&fetch(&body(128 * 1024 + 1)), LimitKind::StringBytes);
}
#[test]
fn declared_literal_limit_precedes_incomplete_buffer_request() {
    let raw = b"* 1 FETCH (BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"name\" {4294967295}\r\n";
    limited(raw, LimitKind::StringBytes);
    let small = b"* 1 FETCH (BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"name\" {131072}\r\n";
    assert!(
        matches!(Response::parse_bounded(small,&BodyStructureLimits::default()),Err(ParseError::Syntax(error)) if error.is_incomplete())
    );
}
#[test]
fn unfinished_quoted_input_has_a_budget() {
    let mut raw = b"* 1 FETCH (BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"name\" \"".to_vec();
    raw.extend(vec![b'x'; 131073]);
    limited(&raw, LimitKind::StringBytes);
}
#[test]
fn lossy_expansion_is_counted_before_allocating() {
    let input = |n| {
        let mut s = b"* 1 FETCH (BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"x\" {".to_vec();
        s.extend(format!("{n}}}\r\n").as_bytes());
        s.extend(vec![255; n]);
        s.extend(b") NIL NIL \"7BIT\" 1 1))\r\n");
        s
    };
    success(&input(43690));
    let raw = input(43691);
    MAX.with(|m| m.set(0));
    TRACK.with(|t| t.set(true));
    let result = Response::parse_bounded(&raw, &BodyStructureLimits::default());
    TRACK.with(|t| t.set(false));
    assert_eq!(
        result.unwrap_err(),
        ParseError::Limit(LimitError::new(
            LimitKind::StringBytes,
            &BodyStructureLimits::default()
        ))
    );
    assert_eq!(MAX.with(|m| m.get()), 0);
}
#[test]
fn extension_depth_and_nodes_share_the_body_budget() {
    let ext = |n| format!("{}1{}", "(".repeat(n), ")".repeat(n));
    let body =
        |e: &str| format!("(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1 NIL NIL NIL NIL {e})");
    success(&fetch(&body(&ext(30))));
    limited(&fetch(&body(&ext(31))), LimitKind::Depth);
    let values = std::iter::repeat_n("1", 1022).collect::<Vec<_>>().join(" ");
    success(&fetch(&body(&format!("({values})"))));
    limited(&fetch(&body(&format!("({values} 1)"))), LimitKind::Nodes);
}
#[test]
fn nested_message_envelope_addresses_are_bounded() {
    success(&fetch(&message(1021)));
    limited(&fetch(&message(1022)), LimitKind::Nodes);
    let raw = format!("* 1 FETCH (ENVELOPE {})\r\n", envelope(1024));
    limited(raw.as_bytes(), LimitKind::Nodes);
}
#[test]
fn several_bodystructures_share_one_response_budget() {
    let a = multipart(&TEXT.repeat(511));
    let b = multipart(&TEXT.repeat(511));
    let raw = format!("* 1 FETCH (BODYSTRUCTURE {a} UID 7 BODYSTRUCTURE {b})\r\n");
    success(raw.as_bytes());
    let raw = format!(
        "* 1 FETCH (BODYSTRUCTURE {a} UID 7 BODYSTRUCTURE {})\r\n",
        multipart(&TEXT.repeat(512))
    );
    limited(raw.as_bytes(), LimitKind::Nodes);
}
#[test]
fn aggregate_budget_covers_strings_and_unused_reserved_slots() {
    let leaf = format!(
        "(\"TEXT\" \"PLAIN\" (\"x\" \"{}\") NIL NIL \"7BIT\" 1 1)",
        "v".repeat(65536)
    );
    let raw = fetch(&multipart(&leaf.repeat(16)));
    MAX.with(|m| m.set(0));
    TRACK.with(|t| t.set(true));
    let result = Response::parse_bounded(&raw, &BodyStructureLimits::default());
    TRACK.with(|t| t.set(false));
    assert_eq!(
        result.unwrap_err(),
        ParseError::Limit(LimitError::new(
            LimitKind::AggregateBytes,
            &BodyStructureLimits::default()
        ))
    );
    assert!(MAX.with(|m| m.get()) <= 1024 * 1024);
    let limits = BodyStructureLimits {
        aggregate_bytes: 1,
        ..BodyStructureLimits::default()
    };
    assert_eq!(
        Response::parse_bounded(&fetch(TEXT), &limits).unwrap_err(),
        ParseError::Limit(LimitError::new(LimitKind::AggregateBytes, &limits))
    );
}
#[test]
fn body_section_literals_keep_existing_wire_contract() {
    let mut raw = format!("* 1 FETCH (UID 7 BODY[] {{{}}}\r\n", 512 * 1024).into_bytes();
    raw.extend(vec![b'x'; 512 * 1024]);
    raw.extend(b")\r\n");
    success(&raw);
}
#[test]
fn failure_is_not_swallowed_as_unknown_attribute() {
    let raw = format!(
        "* 1 FETCH (BODYSTRUCTURE {} UID 7 SAVEDATE NIL)\r\n",
        params(257, 0)
    );
    limited(raw.as_bytes(), LimitKind::NodeParameters);
}
#[test]
fn fragmented_literals_and_escaped_names_preserve_semantics() {
    let raw=b"* 1 FETCH (BODYSTRUCTURE (\"APPLICATION\" \"OCTET-STREAM\" (\"name\" {5}\r\na\xffbcd) NIL NIL \"BASE64\" 8 NIL (\"attachment\" (\"filename\" \"a\\\"b\"))))\r\n";
    success(raw);
    for n in 0..raw.len() {
        let result = Response::parse_bounded(&raw[..n], &BodyStructureLimits::default());
        assert!(
            !matches!(result, Err(ParseError::Limit(_))),
            "prefix {n}: {result:?}"
        );
    }
}

#[test]
fn exact_text_and_message_budgets_do_not_charge_failed_alternatives() {
    let text = params(1, 0);
    let limits = BodyStructureLimits {
        depth: 1,
        nodes: 1,
        node_parameters: 1,
        total_parameters: 1,
        ..BodyStructureLimits::default()
    };
    let raw = fetch(&text);
    let (result, usage) = Response::parse_bounded_with_usage(&raw, &limits);
    result.unwrap();
    assert_eq!(usage.nodes, 1);
    assert_eq!(usage.parameters, 1);
    assert_eq!(usage.peak_depth, 1);
    let body = format!(
        "(\"MESSAGE\" \"RFC822\" (\"x\" \"y\") NIL NIL \"7BIT\" 1 (NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL) {text} 1)"
    );
    let raw = fetch(&body);
    let limits = BodyStructureLimits {
        depth: 2,
        nodes: 3,
        node_parameters: 1,
        total_parameters: 2,
        ..BodyStructureLimits::default()
    };
    let (result, usage) = Response::parse_bounded_with_usage(&raw, &limits);
    result.unwrap();
    assert_eq!(usage.nodes, 3);
    assert_eq!(usage.parameters, 2);
    assert_eq!(usage.peak_depth, 2);
    assert!(usage.largest_capacity_bytes <= usage.container_bytes);
    assert_eq!(
        usage.total_bytes,
        usage.string_bytes + usage.container_bytes
    );
}
#[test]
fn fragmented_reparses_reset_the_whole_response_budget() {
    let raw = fetch(&params(1, 0));
    let limits = BodyStructureLimits {
        depth: 1,
        nodes: 1,
        node_parameters: 1,
        total_parameters: 1,
        ..BodyStructureLimits::default()
    };
    for n in 0..raw.len() {
        let (result, usage) = Response::parse_bounded_with_usage(&raw[..n], &limits);
        assert!(
            !matches!(result, Err(ParseError::Limit(_))),
            "prefix {n}: {result:?}"
        );
        assert!(
            usage.nodes <= 1
                && usage.parameters <= 1
                && usage.total_bytes <= limits.aggregate_bytes
        );
    }
    for _ in 0..3 {
        let (result, usage) = Response::parse_bounded_with_usage(&raw, &limits);
        result.unwrap();
        assert_eq!(usage.nodes, 1);
        assert_eq!(usage.parameters, 1);
    }
}
#[test]
fn exact_accounting_rejects_before_reserving_the_next_capacity() {
    let raw = fetch(&multipart(&TEXT.repeat(40)));
    let (result, usage) = Response::parse_bounded_with_usage(&raw, &BodyStructureLimits::default());
    result.unwrap();
    assert_eq!(
        usage.largest_capacity_bytes,
        usage.largest_reservation_bytes
    );
    assert_eq!(
        usage.total_bytes,
        usage.container_bytes + usage.string_bytes
    );
    let limits = BodyStructureLimits {
        aggregate_bytes: usage.total_bytes,
        ..BodyStructureLimits::default()
    };
    Response::parse_bounded(&raw, &limits).unwrap();
    let limits = BodyStructureLimits {
        aggregate_bytes: usage.total_bytes - 1,
        ..limits
    };
    let (result, before) = Response::parse_bounded_with_usage(&raw, &limits);
    assert_eq!(
        result.unwrap_err(),
        ParseError::Limit(LimitError::new(LimitKind::AggregateBytes, &limits))
    );
    assert!(before.total_bytes <= limits.aggregate_bytes);
    assert!(before.largest_capacity_bytes <= before.container_bytes);
}
#[test]
fn optional_disposition_and_extension_cannot_swallow_budget_failure() {
    let body = format!(
        "(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1 NIL (\"attachment\" {}))",
        pairs(257)
    );
    limited(&fetch(&body), LimitKind::NodeParameters);
}

#[tokio::test]
async fn all_client_constructors_return_private_typed_limits_and_close_the_stream() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for application_buffer in [None, Some(1024)] {
        let mut body = TEXT.replace("PLAIN", "private-synthetic-marker");
        for _ in 0..32 {
            body = multipart(&body);
        }
        let response = fetch(&body);
        let (client_io, mut peer) = tokio::io::duplex(4096);
        let peer_task = tokio::spawn(async move {
            peer.write_all(&response).await.unwrap();
            // A caller retaining a failed Client cannot write another command.
            tokio::time::timeout(std::time::Duration::from_millis(50), peer.read_u8()).await
        });
        let mut client = match application_buffer {
            Some(bytes) => async_imap::Client::with_buffer_limit(client_io, bytes),
            None => async_imap::Client::new(client_io),
        };
        let error = async_imap::error::Error::Io(client.read_response().await.unwrap_err());
        let limit = async_imap::error::body_structure_limit(&error).unwrap();
        assert_eq!(
            limit,
            LimitError::new(LimitKind::Depth, &BodyStructureLimits::default())
        );
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(!rendered.contains("private-synthetic-marker"));
            assert!(!rendered.contains("FETCH") && !rendered.contains("TEXT"));
        }
        assert!(client.read_response().await.unwrap().is_none());
        assert!(client.run_command_and_check_ok("NOOP", None).await.is_err());
        assert!(peer_task.await.unwrap().is_err());
    }
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

#[tokio::test]
async fn fragmented_client_metadata_literal_and_following_response_keep_the_old_shape() {
    use tokio::io::AsyncWriteExt;
    let response = b"* 1 FETCH (UID 7 BODYSTRUCTURE (\"APPLICATION\" \"PDF\" (\"name\" {5}\r\na\xffbcd) NIL NIL \"BASE64\" 8))\r\nA1 OK complete\r\n";
    let (client_io, mut peer) = tokio::io::duplex(2);
    let task = tokio::spawn(async move {
        for chunk in response.chunks(3) {
            peer.write_all(chunk).await.unwrap();
        }
    });
    let mut client = async_imap::Client::with_buffer_limit(client_io, 1024);
    let first = client.read_response().await.unwrap().unwrap();
    let expected = Response::parse(response).unwrap().1;
    assert_eq!(first.parsed(), &expected);
    let second = client.read_response().await.unwrap().unwrap();
    assert!(matches!(
        second.parsed(),
        Response::Done {
            status: async_imap::imap_proto::Status::Ok,
            ..
        }
    ));
    task.await.unwrap();
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

#[test]
fn special_type_prefixes_preserve_incomplete_with_exact_small_string_budgets() {
    for (body, string_bytes) in [
        ("(\"TEXT\" \"X\" NIL NIL NIL \"7BIT\" 1 1)".to_owned(), 1),
        (message(1), 5),
    ] {
        let raw = fetch(&body);
        let limits = BodyStructureLimits {
            string_bytes,
            ..BodyStructureLimits::default()
        };
        Response::parse_bounded(&raw, &limits).unwrap();
        for length in 0..raw.len() {
            assert!(
                matches!(Response::parse_bounded(&raw[..length], &limits),
                Err(ParseError::Syntax(error)) if error.is_incomplete()),
                "prefix {length}, string budget {string_bytes}"
            );
        }
    }
}

#[test]
fn successful_lossy_strings_account_for_their_retained_capacities() {
    use async_imap::imap_proto::{AttributeValue, BodyStructure};
    fn input(count: usize) -> Vec<u8> {
        let mut raw = b"* 1 FETCH (BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (".to_vec();
        for n in 0..count {
            if n != 0 {
                raw.push(b' ');
            }
            raw.extend(b"\"x\" {43680}\r\n");
            raw.extend(vec![255; 43680]);
        }
        raw.extend(b") NIL NIL \"7BIT\" 1 1))\r\n");
        raw
    }
    let raw = input(7);
    let (result, usage) = Response::parse_bounded_with_usage(&raw, &BodyStructureLimits::default());
    let (_, Response::Fetch(_, attributes)) = result.unwrap() else {
        panic!("expected FETCH")
    };
    let AttributeValue::BodyStructure(BodyStructure::Text { common, .. }) = &attributes[0] else {
        panic!("expected TEXT")
    };
    let mut retained = 0;
    for (_, value) in common.ty.params.as_ref().unwrap() {
        let std::borrow::Cow::Owned(value) = value else {
            panic!("expected lossy owned string")
        };
        assert_eq!(value.capacity(), value.len());
        assert_eq!(value.len(), 131040);
        retained += value.capacity();
    }
    assert_eq!(retained, usage.owned_string_capacity_bytes);
    assert_eq!(usage.largest_string_capacity_bytes, 131040);
    assert!(retained <= usage.string_bytes);
    assert!(usage.total_bytes <= 1024 * 1024);
    limited(&input(8), LimitKind::AggregateBytes);
}

#[tokio::test]
async fn client_preserves_incomplete_for_nil_split_after_n_and_ni() {
    use tokio::io::AsyncWriteExt;
    for raw in &PREFIX_CASES[..3] {
        for start in raw
            .windows(3)
            .enumerate()
            .filter_map(|(n, value)| (value == b"NIL").then_some(n))
        {
            for prefix in [start + 1, start + 2] {
                let (client_io, mut peer) = tokio::io::duplex(1024);
                peer.write_all(&raw[..prefix]).await.unwrap();
                let mut client = async_imap::Client::with_buffer_limit(client_io, 1024);
                assert!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(2),
                        client.read_response()
                    )
                    .await
                    .is_err()
                );
                peer.write_all(&raw[prefix..]).await.unwrap();
                assert!(client.read_response().await.unwrap().is_some());
            }
        }
    }
}
