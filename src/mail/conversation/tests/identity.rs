use super::*;

fn id(input: &str) -> MessageId {
    MessageId::parse(input.as_bytes()).unwrap().unwrap()
}

#[test]
fn modern_obsolete_and_unicode_ids_normalize_without_losing_case() {
    let normalized = id(" (tag) <\"Alpha\" (suffix) @ (right) example . test> (end)");
    assert_eq!(normalized, id("<Alpha@example.test>"));
    assert_ne!(normalized, id("<alpha@example.test>"));
    assert_eq!(normalized.search_anchor(), "example");
    assert_eq!(id("<消息@例子.test>").search_anchor(), "test");
    assert_eq!(id("<消息@例子.测试>").search_anchor(), "");
    assert_eq!(id("<\"a\\\"b\"@[a@b>c]>").as_str(), "<\"a\\\"b\"@[a@b>c]>");
    assert_eq!(id("<x@[a@b>c]>").search_anchor(), "");
    for malformed in [
        "<a@@test>",
        "<@test>",
        "<a@>",
        "<a..b@test>",
        "<a@test> extra",
        "<a@test><b@test>",
        "=?utf-8?Q?<a@test>?=",
    ] {
        assert!(
            MessageId::parse(malformed.as_bytes()).unwrap().is_none(),
            "{malformed}"
        );
    }
    assert_eq!(
        MessageId::parse(format!("<{}@test>", "x".repeat(1000)).as_bytes()),
        Err(HeaderLimit)
    );
}

#[test]
fn facts_use_raw_identifier_fields_and_ignore_comment_and_phrase_lookalikes() {
    let raw = "Message-ID: <one@test>\r\nMessage-ID: <two@test>\r\nReferences: (not a reference <comment@test>) \"<phrase@test>\" <real@test>\r\n \t<real@test> <\"alias\"@test>\r\nIn-Reply-To: <回复@例子.test>\r\n\r\n";
    let (headers, _) = mailparse::parse_headers(raw.as_bytes()).unwrap();
    let facts = ThreadFacts::parse(&headers).unwrap();
    assert!(facts.id.is_none());
    assert_eq!(facts.references, [id("<real@test>"), id("<alias@test>")]);
    assert_eq!(facts.in_reply_to, [id("<回复@例子.test>")]);

    let (headers, _) =
        mailparse::parse_headers(b"Message-ID: =?utf-8?Q?=3Cencoded=40test=3E?=\r\n\r\n").unwrap();
    assert!(ThreadFacts::parse(&headers).unwrap().id.is_none());
}

#[test]
fn comparable_fingerprints_and_critical_headers_control_identity() {
    fn evidence(raw: &[u8]) -> Evidence {
        ThreadFacts::parse(&mailparse::parse_headers(raw).unwrap().0)
            .unwrap()
            .evidence
    }
    let mut first = evidence(b"Subject: same\r\nReceived: one\r\n\r\n");
    let mut second = evidence(b"Subject: same\r\nReceived: two\r\n\r\n");
    assert_eq!(first.compare(second), Comparison::Unverified);
    assert_eq!(
        first.compare(evidence(b"Subject: different\r\n\r\n")),
        Comparison::Conflict
    );
    first.content = Some(ContentFingerprint {
        method: 1,
        digest: [1; 32],
    });
    second.content = Some(ContentFingerprint {
        method: 2,
        digest: [2; 32],
    });
    assert_eq!(first.compare(second), Comparison::Unverified);
    second.content.as_mut().unwrap().method = 1;
    assert_eq!(first.compare(second), Comparison::Conflict);
    second.content = first.content;
    assert_eq!(first.compare(second), Comparison::Match);
}

#[test]
fn lossy_display_equivalence_cannot_merge_distinct_wire_headers() {
    let body = ContentFingerprint::raw_mime_body(b"same full MIME body\r\n");
    for (first, second) in [
        (
            b"Subject: =?utf-8?B?/w==?=\r\n\r\n".as_slice(),
            b"Subject: =?utf-8?B?/g==?=\r\n\r\n".as_slice(),
        ),
        (
            b"From: \xff <sender@example.test>\r\n\r\n",
            b"From: \xc3\xbf <sender@example.test>\r\n\r\n",
        ),
    ] {
        let first = mailparse::parse_headers(first).unwrap().0;
        let second = mailparse::parse_headers(second).unwrap().0;
        assert_eq!(first[0].get_value(), second[0].get_value());
        assert_ne!(first[0].get_value_raw(), second[0].get_value_raw());
        let mut first = ThreadFacts::parse(&first).unwrap().evidence;
        let mut second = ThreadFacts::parse(&second).unwrap().evidence;
        first.content = Some(body);
        second.content = Some(body);
        assert_eq!(first.compare(second), Comparison::Conflict);
    }
}

#[test]
fn comparable_raw_body_still_requires_matching_reply_and_mime_headers() {
    let content = ContentFingerprint::raw_mime_body(b"same raw bytes");
    assert_ne!(content, ContentFingerprint::raw_mime_body(b"same raw byte"));
    let evidence = |field: &str, value: &str| {
        let raw = format!("{field}: {value}\r\n\r\n");
        let mut facts =
            ThreadFacts::parse(&mailparse::parse_headers(raw.as_bytes()).unwrap().0).unwrap();
        facts.evidence.content = Some(content);
        facts.evidence
    };
    for field in [
        "Reply-To",
        "Content-Type",
        "Content-Transfer-Encoding",
        "Content-Disposition",
    ] {
        assert_eq!(
            evidence(field, "first").compare(evidence(field, "second")),
            Comparison::Conflict
        );
    }
}
