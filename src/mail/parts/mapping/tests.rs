use super::*;
use imap_proto::{AttributeValue, Response};
use std::borrow::Cow;

fn ast(wire: &str) -> BodyStructure<'static> {
    let wire = format!("* 1 FETCH (UID 7 BODYSTRUCTURE {wire})\r\n");
    let (rest, response) = Response::parse(wire.as_bytes()).unwrap();
    assert!(rest.is_empty());
    let Response::Fetch(_, attrs) = response.into_owned() else {
        panic!("FETCH");
    };
    attrs
        .into_iter()
        .find_map(|value| {
            if let AttributeValue::BodyStructure(node) = value {
                Some(node)
            } else {
                None
            }
        })
        .unwrap()
}
fn leaf(
    media: &str,
    subtype: &str,
    params: &str,
    cte: &str,
    size: usize,
    disposition: &str,
    cid: &str,
) -> String {
    let lines = if media.eq_ignore_ascii_case("text") {
        " 1"
    } else {
        ""
    };
    format!(
        "(\"{media}\" \"{subtype}\" {params} {cid} NIL \"{cte}\" {size}{lines} NIL {disposition})"
    )
}
fn text() -> String {
    leaf("TEXT", "PLAIN", "NIL", "7BIT", 3, "NIL", "NIL")
}
fn multipart(subtype: &str, boundary: &str, children: &[String], disposition: &str) -> String {
    format!(
        "({} \"{subtype}\" (\"BOUNDARY\" \"{boundary}\") {disposition})",
        children.join("")
    )
}
fn raw_multipart(subtype: &str, boundary: &str, headers: &str, children: &[String]) -> String {
    let mut raw =
        format!("Content-Type: multipart/{subtype}; boundary={boundary}\r\n{headers}\r\n");
    for child in children {
        raw.push_str(&format!("--{boundary}\r\n{child}\r\n"));
    }
    raw.push_str(&format!("--{boundary}--\r\n"));
    raw
}
fn raw_leaf(headers: &str) -> String {
    format!("{headers}\r\n\r\nabc")
}
fn leaves<'a, 'b>(part: &'a crate::mime::Part<'b>, out: &mut Vec<&'a crate::mime::Part<'b>>) {
    // Exact R05 mail::leaves branch order; the real bounded MIME parser supplies the tree.
    if part.subparts.is_empty() || part.is_attachment() {
        out.push(part);
    } else {
        for child in &part.subparts {
            leaves(child, out);
        }
    }
}
fn evidence(raw: &[u8]) -> Vec<HeaderEvidence> {
    fn visit(part: &crate::mime::Part<'_>, section: Section, out: &mut Vec<HeaderEvidence>) {
        let mut bytes = vec![];
        for header in &part.headers {
            bytes.extend_from_slice(header.get_key_raw());
            bytes.extend_from_slice(b": ");
            bytes.extend_from_slice(header.get_value_raw());
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(b"\r\n");
        out.push(HeaderEvidence {
            section: section.clone(),
            bytes,
        });
        if !part.is_attachment() {
            for (i, child) in part.subparts.iter().enumerate() {
                visit(child, section.child(i).unwrap(), out);
            }
        }
    }
    let parsed = crate::mime::parse(raw).unwrap();
    let root = Section(if parsed.ctype.mimetype.starts_with("multipart/") {
        vec![]
    } else {
        vec![1]
    });
    let mut out = vec![];
    visit(&parsed, root, &mut out);
    out
}
fn compare(raw: &str, structure: &str) -> PartManifest {
    let ast = ast(structure);
    let headers = evidence(raw.as_bytes());
    let ManifestOutcome::Ready(manifest) = map(&ast, &headers).unwrap() else {
        panic!("map: {:?}", map(&ast, &headers));
    };
    let parsed = crate::mime::parse(raw.as_bytes()).unwrap();
    let canonical = crate::mail::parts::from_raw(&parsed).unwrap();
    let mut expected = vec![];
    leaves(&parsed, &mut expected);
    assert_eq!(manifest.parts.len(), expected.len());
    let mut ordinal = 0;
    for (index, (actual, expected)) in manifest.parts.iter().zip(expected).enumerate() {
        assert_eq!(actual.section, canonical.manifest.parts[index].section);
        assert!(std::ptr::eq(canonical.entities[index], expected));
        assert_eq!(actual.attachment_index, index);
        assert_eq!(actual.is_attachment, expected.is_attachment());
        assert_eq!(actual.filename, expected.filename());
        assert_eq!(actual.content_type, expected.ctype.mimetype);
        let disposition = match expected.get_content_disposition().disposition {
            mailparse::DispositionType::Inline => Some("inline".to_owned()),
            mailparse::DispositionType::Attachment => Some("attachment".to_owned()),
            mailparse::DispositionType::FormData => Some("form-data".to_owned()),
            mailparse::DispositionType::Extension(value) if !value.is_empty() => Some(value),
            _ => None,
        };
        assert_eq!(actual.disposition, disposition);
        if actual.is_attachment {
            assert_eq!(actual.attachment_ordinal, Some(ordinal));
            ordinal += 1;
        } else {
            assert_eq!(actual.attachment_ordinal, None);
        }
    }
    manifest
}
#[test]
fn absent_empty_inline_unknown_dispositions_and_filename_intent_match_raw() {
    for (header, disposition, attached) in [
        ("", "NIL", false),
        ("Content-Disposition: ", "(\"\" NIL)", false),
        ("Content-Disposition: inline", "(\"INLINE\" NIL)", false),
        (
            "Content-Disposition: attachment",
            "(\"ATTACHMENT\" NIL)",
            true,
        ),
        (
            "Content-Disposition: x-private",
            "(\"X-PRIVATE\" NIL)",
            true,
        ),
        (
            "Content-Disposition: inline; filename=same.txt",
            "(\"INLINE\" (\"FILENAME\" \"same.txt\"))",
            true,
        ),
    ] {
        let raw = raw_leaf(&format!(
            "Content-Type: application/octet-stream\r\n{header}"
        ));
        let structure = leaf(
            "APPLICATION",
            "OCTET-STREAM",
            "NIL",
            "7BIT",
            3,
            disposition,
            "NIL",
        );
        let manifest = compare(&raw, &structure);
        assert_eq!(manifest.parts[0].is_attachment, attached);
        assert_eq!(manifest.parts[0].section.numbers(), &[1]);
    }
}
#[test]
fn headerless_leaf_requires_evidence_and_uses_the_default_media_type() {
    let node = ast(&text());
    assert!(matches!(
        map(&node, &[]).unwrap(),
        ManifestOutcome::NeedsHeaders(_)
    ));
    let manifest = compare("\r\nabc", &text());
    assert_eq!(manifest.parts[0].content_type, "text/plain");
}
#[test]
fn explicit_simple_attachment_requires_original_header_evidence() {
    let node = ast(&leaf(
        "APPLICATION",
        "PDF",
        "NIL",
        "BASE64",
        4,
        "(\"ATTACHMENT\" (\"FILENAME\" \"x.pdf\"))",
        "\"<cid>\"",
    ));
    assert!(matches!(
        map(&node, &[]).unwrap(),
        ManifestOutcome::NeedsHeaders(_)
    ));
    let headers = HeaderEvidence { section: Section(vec![1]), bytes: b"Content-Type: application/pdf\r\nContent-Disposition: attachment; filename=x.pdf\r\nContent-Transfer-Encoding: base64\r\nContent-ID: <cid>\r\n\r\n".to_vec() };
    let ManifestOutcome::Ready(manifest) = map(&node, &[headers]).unwrap() else {
        panic!("ready");
    };
    assert_eq!(manifest.parts[0].filename.as_deref(), Some("x.pdf"));
    assert_eq!(manifest.parts[0].content_id.as_deref(), Some("<cid>"));
    assert_eq!(manifest.parts[0].encoded_size, Some(4));
    assert_eq!(manifest.parts[0].transfer_encoding, "base64");
}
#[test]
fn rfc2231_names_need_headers_and_keep_success_and_failed_decode_attachment_intent() {
    for (value, expected) in [
        ("utf-8''%E4%B8%AD%E6%96%87.txt", Some("中文.txt")),
        ("x-unknown''abc.txt", None),
    ] {
        let structure = leaf(
            "APPLICATION",
            "OCTET-STREAM",
            "NIL",
            "7BIT",
            3,
            &format!("(\"INLINE\" (\"FILENAME*\" \"{value}\"))"),
            "NIL",
        );
        let node = ast(&structure);
        assert!(matches!(
            map(&node, &[]).unwrap(),
            ManifestOutcome::NeedsHeaders(_)
        ));
        let raw = raw_leaf(&format!(
            "Content-Type: application/octet-stream\r\nContent-Disposition: inline; filename*={value}"
        ));
        let manifest = compare(&raw, &structure);
        assert!(manifest.parts[0].is_attachment);
        assert_eq!(manifest.parts[0].filename.as_deref(), expected);
    }
    let structure = leaf(
        "APPLICATION",
        "OCTET-STREAM",
        "NIL",
        "7BIT",
        3,
        "(\"INLINE\" (\"FILENAME*0*\" \"utf-8''long%20\" \"FILENAME*1*\" \"name.txt\"))",
        "NIL",
    );
    let raw = raw_leaf(
        "Content-Type: application/octet-stream\r\nContent-Disposition: inline; filename*0*=utf-8''long%20; filename*1*=name.txt",
    );
    assert_eq!(
        compare(&raw, &structure).parts[0].filename.as_deref(),
        Some("long name.txt")
    );
}
#[test]
fn nested_alternatives_related_and_duplicate_filenames_preserve_dfs_and_sections() {
    let plain = raw_leaf("Content-Type: text/plain");
    let html = raw_leaf("Content-Type: text/html");
    let image =
        raw_leaf("Content-Type: image/png\r\nContent-Disposition: inline\r\nContent-ID: <image>");
    let attached = raw_leaf(
        "Content-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=same.txt",
    );
    let related = raw_multipart("related", "r", "", &[html, image]);
    let alternative = raw_multipart("alternative", "a", "", &[plain, related]);
    let raw = raw_multipart("mixed", "m", "", &[alternative, attached.clone(), attached]);
    let image = leaf(
        "IMAGE",
        "PNG",
        "NIL",
        "7BIT",
        3,
        "(\"INLINE\" NIL)",
        "\"<image>\"",
    );
    let related = multipart(
        "RELATED",
        "r",
        &[leaf("TEXT", "HTML", "NIL", "7BIT", 3, "NIL", "NIL"), image],
        "NIL",
    );
    let alternative = multipart("ALTERNATIVE", "a", &[text(), related], "NIL");
    let attached = leaf(
        "APPLICATION",
        "OCTET-STREAM",
        "NIL",
        "7BIT",
        3,
        "(\"ATTACHMENT\" (\"FILENAME\" \"same.txt\"))",
        "NIL",
    );
    let structure = multipart(
        "MIXED",
        "m",
        &[alternative, attached.clone(), attached],
        "NIL",
    );
    let manifest = compare(&raw, &structure);
    assert_eq!(
        manifest
            .parts
            .iter()
            .map(|v| v.section.body_selector())
            .collect::<Vec<_>>(),
        ["1.1", "1.2.1", "1.2.2", "2", "3"]
    );
    assert_eq!(manifest.parts[2].content_id.as_deref(), Some("<image>"));
    assert_eq!(manifest.parts[3].attachment_index, 3);
    assert_eq!(manifest.parts[4].attachment_index, 4);
}
#[test]
fn message_rfc822_is_one_entity_and_does_not_expose_its_internal_attachments() {
    let nested = raw_multipart(
        "mixed",
        "n",
        "",
        &[
            raw_leaf("Content-Type: text/plain"),
            raw_leaf("Content-Type: application/pdf\r\nContent-Disposition: attachment"),
        ],
    );
    let raw_message = format!(
        "Content-Type: message/rfc822\r\nContent-Disposition: attachment; filename=mail.eml\r\n\r\n{nested}"
    );
    let inner = multipart(
        "MIXED",
        "n",
        &[
            text(),
            leaf(
                "APPLICATION",
                "PDF",
                "NIL",
                "7BIT",
                3,
                "(\"ATTACHMENT\" NIL)",
                "NIL",
            ),
        ],
        "NIL",
    );
    let structure = format!(
        "(\"MESSAGE\" \"RFC822\" NIL NIL NIL \"7BIT\" {} (NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL) {inner} 10 NIL (\"ATTACHMENT\" (\"FILENAME\" \"mail.eml\")))",
        nested.len()
    );
    let raw = raw_multipart(
        "mixed",
        "m",
        "",
        &[raw_message, raw_leaf("Content-Type: text/plain")],
    );
    let manifest = compare(&raw, &multipart("MIXED", "m", &[structure, text()], "NIL"));
    assert_eq!(manifest.parts.len(), 2);
    assert_eq!(manifest.parts[0].section.numbers(), &[1]);
    assert_eq!(manifest.parts[1].section.numbers(), &[2]);
}
#[test]
fn multipart_attachment_requests_raw_fallback_without_publishing_new_indices() {
    let raw = raw_multipart(
        "mixed",
        "m",
        "Content-Disposition: attachment; filename=bundle.mime\r\n",
        &[
            raw_leaf("Content-Type: text/plain"),
            raw_leaf("Content-Type: application/pdf\r\nContent-Disposition: attachment"),
        ],
    );
    let node = ast(&multipart(
        "MIXED",
        "m",
        &[
            text(),
            leaf(
                "APPLICATION",
                "PDF",
                "NIL",
                "7BIT",
                3,
                "(\"ATTACHMENT\" NIL)",
                "NIL",
            ),
        ],
        "(\"ATTACHMENT\" (\"FILENAME\" \"bundle.mime\"))",
    ));
    assert_eq!(
        map(&node, &evidence(raw.as_bytes())).unwrap(),
        ManifestOutcome::RawFallback {
            reason: "whole_multipart_attachment"
        }
    );
    let parsed = crate::mime::parse(raw.as_bytes()).unwrap();
    let mut legacy = vec![];
    leaves(&parsed, &mut legacy);
    assert_eq!(legacy.len(), 1);
    assert!(legacy[0].is_attachment());
}
#[test]
fn unknown_transfer_encoding_is_opaque_even_when_ast_contains_a_tree() {
    let raw = raw_multipart(
        "mixed",
        "m",
        "Content-Transfer-Encoding: x-private\r\n",
        &[raw_leaf("Content-Type: text/plain")],
    );
    let structure = multipart("MIXED", "m", &[text()], "NIL");
    let manifest = compare(&raw, &structure);
    assert_eq!(manifest.parts.len(), 1);
    assert!(!manifest.parts[0].downloadable);
    assert_eq!(manifest.parts[0].transfer_encoding, "x-private");
}
#[test]
fn missing_and_conflicting_headers_never_publish_partial_indices() {
    let node = ast(&multipart("MIXED", "m", &[text(), text()], "NIL"));
    let ManifestOutcome::NeedsHeaders(requests) = map(&node, &[]).unwrap() else {
        panic!("headers");
    };
    assert_eq!(requests.len(), 1);
    assert!(requests[0].root_headers);
    let root_header = HeaderEvidence {
        section: Section(vec![]),
        bytes: b"Content-Type: multipart/mixed; boundary=m\r\n\r\n".to_vec(),
    };
    let ManifestOutcome::NeedsHeaders(requests) = map(&node, &[root_header]).unwrap() else {
        panic!("child headers");
    };
    assert_eq!(
        requests
            .iter()
            .map(|r| r.section.body_selector())
            .collect::<Vec<_>>(),
        ["1", "2"]
    );
    let node = ast(&leaf(
        "APPLICATION",
        "PDF",
        "NIL",
        "BASE64",
        4,
        "(\"ATTACHMENT\" NIL)",
        "NIL",
    ));
    for raw in [
        "Content-Type: text/plain\r\n\r\n",
        "Content-Type: application/pdf\r\nContent-Transfer-Encoding: binary\r\n\r\n",
        "Content-Type: application/pdf\r\nContent-Type: application/pdf\r\n\r\n",
    ] {
        assert!(matches!(
            map(
                &node,
                &[HeaderEvidence {
                    section: Section(vec![1]),
                    bytes: raw.as_bytes().to_vec()
                }]
            )
            .unwrap(),
            ManifestOutcome::RawFallback { .. }
        ));
    }
}
#[test]
fn structure_conversion_budgets_include_pruned_children_extensions_and_parameters() {
    let mut node = ast(&leaf(
        "APPLICATION",
        "PDF",
        "NIL",
        "7BIT",
        0,
        "(\"ATTACHMENT\" NIL)",
        "NIL",
    ));
    if let BodyStructure::Basic { common, .. } = &mut node {
        common.ty.params = Some(
            (0..257)
                .map(|_| (Cow::Borrowed("x"), Cow::Borrowed("v")))
                .collect(),
        );
    }
    assert_eq!(map(&node, &[]), Err(MapError::StructureLimit));
    let mut node = ast(&text());
    if let BodyStructure::Text { extension, .. } = &mut node {
        let mut nested = BodyExtension::Num(1);
        for _ in 0..34 {
            nested = BodyExtension::List(vec![nested]);
        }
        *extension = Some(nested);
    }
    assert_eq!(map(&node, &[]), Err(MapError::StructureLimit));
    let mut node = ast(&text());
    if let BodyStructure::Text { other, .. } = &mut node {
        other.id = Some(Cow::Owned("x".repeat(MAX_STRING + 1)));
    }
    assert_eq!(map(&node, &[]), Err(MapError::StructureLimit));
    let children = (0..1024).map(|_| ast(&text())).collect();
    let mut node = ast(&multipart("MIXED", "m", &[text()], "NIL"));
    if let BodyStructure::Multipart { bodies, .. } = &mut node {
        *bodies = children;
    }
    assert_eq!(map(&node, &[]), Err(MapError::StructureLimit));
}

#[test]
fn type_parameter_and_cid_conflicts_request_whole_message_fallback() {
    let node = ast(&leaf(
        "TEXT",
        "PLAIN",
        "(\"CHARSET\" \"UTF-8\")",
        "7BIT",
        3,
        "(\"INLINE\" NIL)",
        "\"<expected>\"",
    ));
    for raw in [
        "Content-Type: text/plain; charset=us-ascii\r\nContent-Disposition: inline\r\nContent-ID: <expected>\r\n\r\n",
        "Content-Type: text/plain; charset=utf-8\r\nContent-Disposition: inline\r\nContent-ID: <other>\r\n\r\n",
    ] {
        assert_eq!(
            map(
                &node,
                &[HeaderEvidence {
                    section: Section(vec![1]),
                    bytes: raw.as_bytes().to_vec()
                }]
            )
            .unwrap(),
            ManifestOutcome::RawFallback {
                reason: "bodystructure_header_conflict"
            }
        );
    }
    let raw = "Content-Type: text/plain; charset=utf-8\r\nContent-Disposition: inline\r\nContent-ID: <expected>\r\n\r\n";
    assert!(matches!(
        map(
            &node,
            &[HeaderEvidence {
                section: Section(vec![1]),
                bytes: raw.as_bytes().to_vec()
            }]
        )
        .unwrap(),
        ManifestOutcome::Ready(_)
    ));
    let node = ast(&multipart("MIXED", "wrong", &[text()], "NIL"));
    assert_eq!(
        map(
            &node,
            &[HeaderEvidence {
                section: Section(vec![]),
                bytes: b"Content-Type: multipart/mixed; boundary=actual\r\n\r\n".to_vec()
            }]
        )
        .unwrap(),
        ManifestOutcome::RawFallback {
            reason: "bodystructure_header_conflict"
        }
    );
}
#[test]
fn all_1023_leaf_positions_are_preserved_without_attachment_renumbering() {
    let plain = leaf("TEXT", "PLAIN", "NIL", "7BIT", 0, "(\"INLINE\" NIL)", "NIL");
    let mut children = vec![plain; 1023];
    children[1022] = leaf(
        "APPLICATION",
        "PDF",
        "NIL",
        "7BIT",
        0,
        "(\"ATTACHMENT\" NIL)",
        "NIL",
    );
    let node = ast(&multipart("MIXED", "m", &children, "NIL"));
    let mut evidence = vec![HeaderEvidence {
        section: Section(vec![]),
        bytes: b"Content-Type: multipart/mixed; boundary=m\r\n\r\n".to_vec(),
    }];
    for index in 1..=1023 {
        evidence.push(HeaderEvidence {
            section: Section(vec![index]),
            bytes: if index == 1023 {
                b"Content-Type: application/pdf\r\nContent-Disposition: attachment\r\n\r\n".to_vec()
            } else {
                b"Content-Type: text/plain\r\nContent-Disposition: inline\r\n\r\n".to_vec()
            },
        });
    }
    let ManifestOutcome::Ready(manifest) = map(&node, &evidence).unwrap() else {
        panic!("ready");
    };
    assert_eq!(manifest.parts.len(), 1023);
    let last = manifest.parts.last().unwrap();
    assert_eq!(last.attachment_index, 1022);
    assert_eq!(last.attachment_ordinal, Some(0));
    assert_eq!(last.section.numbers(), &[1023]);
}
#[test]
fn empty_attachments_and_invalid_cte_are_distinct_from_invalid_base64_payloads() {
    let node = ast(&leaf(
        "APPLICATION",
        "OCTET-STREAM",
        "NIL",
        "BASE64",
        0,
        "(\"ATTACHMENT\" NIL)",
        "NIL",
    ));
    let ManifestOutcome::Ready(manifest) = map(&node, &[HeaderEvidence { section: Section(vec![1]), bytes: b"Content-Type: application/octet-stream\r\nContent-Disposition: attachment\r\nContent-Transfer-Encoding: base64\r\n\r\n".to_vec() }]).unwrap() else {
        panic!("ready");
    };
    assert_eq!(manifest.parts[0].encoded_size, Some(0));
    assert!(manifest.parts[0].downloadable);
    let node = ast(&leaf(
        "APPLICATION",
        "OCTET-STREAM",
        "NIL",
        "X-UNKNOWN",
        3,
        "(\"ATTACHMENT\" NIL)",
        "NIL",
    ));
    let ManifestOutcome::Ready(manifest) = map(&node, &[HeaderEvidence { section: Section(vec![1]), bytes: b"Content-Type: application/octet-stream\r\nContent-Disposition: attachment\r\nContent-Transfer-Encoding: x-unknown\r\n\r\n".to_vec() }]).unwrap() else {
        panic!("ready");
    };
    assert!(!manifest.parts[0].downloadable);
    // Manifest establishes encoding support; validation of actual Base64 bytes is
    // deliberately left to the bounded decoder after fetching the complete entity.
    let parsed = crate::mime::parse(b"Content-Type: application/octet-stream\r\nContent-Disposition: attachment\r\nContent-Transfer-Encoding: base64\r\n\r\n!").unwrap();
    assert!(parsed.body_raw(32).is_err());
}
#[test]
fn header_evidence_limits_and_trailing_body_never_enter_mime_interpretation() {
    let node = ast(&text());
    assert_eq!(
        map(
            &node,
            &[HeaderEvidence {
                section: Section(vec![1]),
                bytes: vec![0; MAX_STRING + 1]
            }]
        ),
        Err(MapError::HeaderLimit)
    );
    assert_eq!(
        map(
            &node,
            &[HeaderEvidence {
                section: Section(vec![1]),
                bytes: b"Content-Type: text/plain\r\n\r\nbody\r\n\r\n".to_vec()
            }]
        )
        .unwrap(),
        ManifestOutcome::RawFallback {
            reason: "incomplete_mime_headers"
        }
    );
}

fn message_structure(octets: usize, cte: &str) -> String {
    format!(
        "(\"MESSAGE\" \"RFC822\" NIL NIL NIL \"{cte}\" {octets} (NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL) {} 1 NIL NIL)",
        text()
    )
}
#[test]
fn digest_missing_child_media_type_defaults_to_whole_rfc822_entity() {
    let nested = "Subject: inner\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment\r\n\r\nabc";
    let child = format!("\r\n{nested}");
    let explicit_text = raw_leaf("Content-Type: text/plain");
    let raw = raw_multipart("digest", "d", "", &[child, explicit_text]);
    let structure = multipart(
        "DIGEST",
        "d",
        &[message_structure(nested.len(), "7BIT"), text()],
        "NIL",
    );
    let manifest = compare(&raw, &structure);
    assert_eq!(manifest.parts.len(), 2);
    assert_eq!(manifest.parts[0].content_type, "message/rfc822");
    assert_eq!(manifest.parts[0].section.numbers(), &[1]);
    assert!(!manifest.parts[0].is_attachment);
    assert_eq!(manifest.parts[1].content_type, "text/plain");
}
#[test]
fn digest_default_is_scoped_to_immediate_children_and_respects_explicit_type() {
    let inner = "Subject: inner\r\n\r\nabc";
    let digest = raw_multipart(
        "digest",
        "d",
        "",
        &[
            format!("Content-Transfer-Encoding: 8bit\r\n\r\n{inner}"),
            raw_leaf("Content-Type: text/plain"),
        ],
    );
    let raw = raw_multipart("mixed", "m", "", &[digest, "\r\nabc".into()]);
    let structure = multipart(
        "MIXED",
        "m",
        &[
            multipart(
                "DIGEST",
                "d",
                &[message_structure(inner.len(), "8BIT"), text()],
                "NIL",
            ),
            text(),
        ],
        "NIL",
    );
    let manifest = compare(&raw, &structure);
    assert_eq!(
        manifest
            .parts
            .iter()
            .map(|p| p.content_type.as_str())
            .collect::<Vec<_>>(),
        ["message/rfc822", "text/plain", "text/plain"]
    );
    assert_eq!(
        manifest
            .parts
            .iter()
            .map(|p| p.section.body_selector())
            .collect::<Vec<_>>(),
        ["1.1", "1.2", "2"]
    );
    assert_eq!(manifest.parts[0].transfer_encoding, "8bit");
}
#[test]
fn digest_default_conflict_with_server_structure_uses_raw_fallback() {
    let raw = raw_multipart("digest", "d", "", &["\r\nSubject: inner\r\n\r\nabc".into()]);
    let node = ast(&multipart("DIGEST", "d", &[text()], "NIL"));
    assert_eq!(
        map(&node, &evidence(raw.as_bytes())).unwrap(),
        ManifestOutcome::RawFallback {
            reason: "bodystructure_header_conflict"
        }
    );
}

#[test]
fn digest_explicit_empty_content_type_keeps_raw_semantics_and_requests_fallback() {
    let raw = raw_multipart("digest", "d", "", &[raw_leaf("Content-Type:")]);
    let parsed = crate::mime::parse(raw.as_bytes()).unwrap();
    assert_eq!(parsed.subparts[0].ctype.mimetype, "");
    for child in [text(), message_structure(3, "7BIT")] {
        let node = ast(&multipart("DIGEST", "d", &[child], "NIL"));
        assert_eq!(
            map(&node, &evidence(raw.as_bytes())).unwrap(),
            ManifestOutcome::RawFallback {
                reason: "bodystructure_header_conflict"
            }
        );
    }
}

#[test]
fn duplicate_original_headers_cannot_publish_a_server_reclassified_leaf_index() {
    let raw = b"Content-Type: multipart/mixed; boundary=m\r\nContent-Type: application/pdf\r\nContent-Disposition: inline\r\nContent-Disposition: attachment\r\n\r\n--m\r\nContent-Type: text/plain\r\n\r\na\r\n--m\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment\r\n\r\nb\r\n--m--\r\n";
    let parsed = crate::mime::parse(raw).unwrap();
    let canonical = crate::mail::parts::from_raw(&parsed).unwrap();
    assert_eq!(canonical.entities.len(), 2);
    assert!(!canonical.manifest.parts[0].is_attachment);
    assert_eq!(canonical.manifest.parts[1].attachment_index, 1);
    let node = ast(&leaf(
        "APPLICATION",
        "PDF",
        "NIL",
        "7BIT",
        3,
        "(\"ATTACHMENT\" NIL)",
        "NIL",
    ));
    let ManifestOutcome::NeedsHeaders(requests) = map(&node, &[]).unwrap() else {
        panic!("original headers required");
    };
    assert!(requests[0].root_headers);
    let end = raw
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap()
        + 4;
    assert_eq!(
        map(
            &node,
            &[HeaderEvidence {
                section: requests[0].section.clone(),
                bytes: raw[..end].to_vec()
            }]
        )
        .unwrap(),
        ManifestOutcome::RawFallback {
            reason: "duplicate_mime_metadata"
        }
    );
}

#[test]
fn encoded_or_boundaryless_multipart_requires_raw_shape_fallback() {
    let structure = multipart("MIXED", "m", &[text(), text()], "NIL");
    for encoding in ["base64", "quoted-printable"] {
        let raw = format!(
            "Content-Type: multipart/mixed; boundary=m\r\nContent-Transfer-Encoding: {encoding}\r\n\r\nENCODED_CONTAINER_WITHOUT_RAW_BOUNDARIES"
        );
        let parsed = crate::mime::parse(raw.as_bytes()).unwrap();
        assert_eq!(
            crate::mail::parts::from_raw(&parsed)
                .unwrap()
                .entities
                .len(),
            1
        );
        assert_eq!(
            map(&ast(&structure), &evidence(raw.as_bytes())).unwrap(),
            ManifestOutcome::RawFallback {
                reason: "multipart_shape_unverified"
            }
        );
    }
    let structure = format!("({}{} \"MIXED\" NIL NIL)", text(), text());
    let raw = b"Content-Type: multipart/mixed\r\n\r\nopaque";
    let parsed = crate::mime::parse(raw).unwrap();
    assert_eq!(
        crate::mail::parts::from_raw(&parsed)
            .unwrap()
            .entities
            .len(),
        1
    );
    assert_eq!(
        map(&ast(&structure), &evidence(raw)).unwrap(),
        ManifestOutcome::RawFallback {
            reason: "multipart_shape_unverified"
        }
    );
}
#[test]
fn maximum_depth_mapping_keeps_its_original_leaf_index() {
    use imap_proto::{BodyContentCommon, ContentType};
    let mut node = ast(&text());
    for _ in 0..31 {
        node = BodyStructure::Multipart {
            common: BodyContentCommon {
                ty: ContentType {
                    ty: Cow::Borrowed("MULTIPART"),
                    subtype: Cow::Borrowed("MIXED"),
                    params: Some(vec![(Cow::Borrowed("BOUNDARY"), Cow::Borrowed("m"))]),
                },
                disposition: None,
                language: None,
                location: None,
            },
            bodies: vec![node],
            extension: None,
        };
    }
    // Construct the AST directly: this test isolates the mapper from protocol parsing.
    // Headers are direct evidence and do not materialize another parser stack.
    let mut headers = vec![HeaderEvidence {
        section: Section(vec![]),
        bytes: b"Content-Type: multipart/mixed; boundary=m\r\n\r\n".to_vec(),
    }];
    for depth in 1..=31 {
        headers.push(HeaderEvidence {
            section: Section(vec![1; depth]),
            bytes: if depth == 31 {
                b"Content-Type: text/plain\r\n\r\n".to_vec()
            } else {
                b"Content-Type: multipart/mixed; boundary=m\r\n\r\n".to_vec()
            },
        });
    }
    let ManifestOutcome::Ready(manifest) = map(&node, &headers).unwrap() else {
        panic!("ready");
    };
    assert_eq!(manifest.parts.len(), 1);
    assert_eq!(manifest.parts[0].section.numbers(), vec![1; 31]);
    assert_eq!(manifest.parts[0].attachment_index, 0);
}
