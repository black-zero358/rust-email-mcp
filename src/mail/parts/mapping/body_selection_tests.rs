use super::{tests::*, *};
use crate::{
    mail::parts::body_tree::{BodyKind, Node},
    mime::selection,
};

fn compare_selection(raw: &str, structure: &str) -> (Vec<usize>, bool, usize) {
    let mut manifest = compare(raw, structure);
    let actual_bytes = manifest.parts.capacity() * size_of::<ManifestPart>()
        + manifest.body.nodes.capacity() * size_of::<BodyNode>()
        + manifest.body.root_headers.capacity()
        + manifest
            .parts
            .iter()
            .map(|part| {
                part.section.0.capacity() * size_of::<u32>()
                    + part.content_type.capacity()
                    + part.charset.capacity()
                    + part.transfer_encoding.capacity()
                    + part.disposition.as_ref().map_or(0, String::capacity)
                    + part.filename.as_ref().map_or(0, String::capacity)
                    + part.content_id.as_ref().map_or(0, String::capacity)
            })
            .sum::<usize>()
        + manifest
            .body
            .nodes
            .iter()
            .map(|node| {
                node.children.capacity() * size_of::<usize>()
                    + match &node.kind {
                        BodyKind::Leaf(_) => 0,
                        BodyKind::Container {
                            section,
                            media_type,
                            encoding,
                            content_id,
                            related_start,
                        } => {
                            section.0.capacity() * size_of::<u32>()
                                + media_type.capacity()
                                + encoding.capacity()
                                + content_id.as_ref().map_or(0, String::capacity)
                                + related_start.as_ref().map_or(0, String::capacity)
                        }
                    }
            })
            .sum::<usize>();
    assert_eq!(manifest.accounted_bytes, actual_bytes);
    assert!(actual_bytes <= MAX_BYTES);
    let parsed = crate::mime::parse(raw.as_bytes()).unwrap();
    let canonical = crate::mail::parts::from_raw(&parsed).unwrap();
    let mut expected = Vec::new();
    let unsupported =
        selection::select(&parsed, &mut expected, true).unwrap_or_else(|_| panic!("raw emptiness"));
    let expected = expected
        .into_iter()
        .map(|part| {
            canonical
                .entities
                .iter()
                .position(|other| std::ptr::eq(*other, part))
                .unwrap()
        })
        .collect::<Vec<_>>();
    fn find<'a, 'b>(part: &'a crate::mime::Part<'b>, path: &[u32]) -> &'a crate::mime::Part<'b> {
        if path.is_empty() {
            return part;
        }
        find(&part.subparts[(path[0] - 1) as usize], &path[1..])
    }
    let mut probes = 0;
    loop {
        let mut selected = Vec::new();
        match selection::select(
            Node {
                manifest: &manifest,
                index: 0,
            },
            &mut selected,
            true,
        ) {
            Ok(actual) => {
                let actual_parts = selected
                    .iter()
                    .map(|node| match manifest.body.nodes[node.index].kind {
                        BodyKind::Leaf(index) => index,
                        _ => panic!("container selected"),
                    })
                    .collect::<Vec<_>>();
                assert_eq!(actual, unsupported);
                assert_eq!(actual_parts, expected);
                for (index, node) in manifest.body.nodes.iter().enumerate() {
                    for child in &node.children {
                        assert_eq!(manifest.body.nodes[*child].parent, Some(index));
                    }
                }
                return (actual_parts, actual, probes);
            }
            Err(node) => {
                probes += 1;
                let index = node.index;
                let section = match &manifest.body.nodes[index].kind {
                    BodyKind::Leaf(part) => &manifest.parts[*part].section,
                    BodyKind::Container { section, .. } => section,
                };
                let entity = if parsed.subparts.is_empty() {
                    &parsed
                } else {
                    find(&parsed, section.numbers())
                };
                let nonempty = Some(entity.encoded_len() != 0);
                manifest.body.nodes[index].nonempty = nonempty;
                assert!(probes <= manifest.body.nodes.len());
            }
        }
    }
}

#[test]
fn alternatives_select_complete_plain_and_rollback_mixed_signatures() {
    let plain = raw_leaf("Content-Type: text/plain");
    let html = raw_leaf("Content-Type: text/html");
    let mixed = raw_multipart("mixed", "m", "", &[plain.clone(), html.clone()]);
    let ast_mixed = multipart(
        "MIXED",
        "m",
        &[text(), leaf("TEXT", "HTML", "NIL", "7BIT", 3, "NIL", "NIL")],
        "NIL",
    );
    let raw = raw_multipart("alternative", "a", "", &[plain, mixed]);
    let structure = multipart("ALTERNATIVE", "a", &[text(), ast_mixed], "NIL");
    assert_eq!(compare_selection(&raw, &structure), (vec![0], false, 0));
    let raw = raw_multipart(
        "alternative",
        "a",
        "",
        &["Content-Type: text/plain\r\n\r\n".into(), html],
    );
    let structure = multipart(
        "ALTERNATIVE",
        "a",
        &[
            leaf("TEXT", "PLAIN", "NIL", "7BIT", 0, "NIL", "NIL"),
            leaf("TEXT", "HTML", "NIL", "7BIT", 3, "NIL", "NIL"),
        ],
        "NIL",
    );
    assert_eq!(compare_selection(&raw, &structure), (vec![0], false, 0));
}

#[test]
fn related_absent_matched_and_unmatched_start_keep_raw_semantics() {
    for start in [None, Some("<b>"), Some("<missing>")] {
        let raw = raw_multipart(
            "related",
            "r",
            "",
            &[
                raw_leaf("Content-Type: text/plain\r\nContent-ID: <a>"),
                raw_leaf("Content-Type: text/html\r\nContent-ID: <b>"),
            ],
        );
        let raw = if let Some(start) = start {
            raw.replacen("boundary=r", &format!("boundary=r; start=\"{start}\""), 1)
        } else {
            raw
        };
        let structure = multipart(
            "RELATED",
            "r",
            &[
                leaf("TEXT", "PLAIN", "NIL", "7BIT", 3, "NIL", "\"<a>\""),
                leaf("TEXT", "HTML", "NIL", "7BIT", 3, "NIL", "\"<b>\""),
            ],
            "NIL",
        );
        let (selected, unsupported, probes) = compare_selection(&raw, &structure);
        assert_eq!(unsupported, start == Some("<missing>"));
        assert_eq!(probes, usize::from(unsupported));
        if !unsupported {
            assert_eq!(selected, vec![usize::from(start.is_some())]);
        }
    }
}

#[test]
fn interrupted_alternative_probe_discards_all_preceding_fragments() {
    let opaque="Content-Type: multipart/mixed; boundary=o\r\nContent-Transfer-Encoding: x-opaque\r\n\r\nencoded".to_owned();
    let raw = raw_multipart(
        "alternative",
        "a",
        "",
        &[
            raw_leaf("Content-Type: text/plain"),
            raw_multipart(
                "mixed",
                "m",
                "",
                &[raw_leaf("Content-Type: text/plain"), opaque],
            ),
        ],
    );
    let opaque_ast =
        "((\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1) \"MIXED\" (\"BOUNDARY\" \"o\"))";
    let structure = multipart(
        "ALTERNATIVE",
        "a",
        &[
            text(),
            multipart("MIXED", "m", &[text(), opaque_ast.into()], "NIL"),
        ],
        "NIL",
    );
    assert_eq!(compare_selection(&raw, &structure), (vec![0], false, 1));
}

#[test]
fn output_budget_accounts_real_vector_and_string_capacity() {
    let mut budget = Budget::default();
    let mut values = Vec::<BodyNode>::new();
    budget.grow(&mut values, 31).unwrap();
    assert_eq!(budget.bytes, values.capacity() * size_of::<BodyNode>());
    let text = String::with_capacity(4096);
    budget.owned(&text).unwrap();
    assert_eq!(
        budget.bytes,
        values.capacity() * size_of::<BodyNode>() + text.capacity()
    );
    let mut bytes = Vec::new();
    let before = budget.bytes;
    budget.grow(&mut bytes, 128 * 1024).unwrap();
    bytes.resize(128 * 1024, 0u8);
    assert_eq!(budget.bytes - before, bytes.capacity());
    assert!(budget.grow(&mut bytes, MAX_BYTES).is_err());
    let mut full = Budget::default();
    full.charge(MAX_BYTES).unwrap();
    let mut unallocated = Vec::<u8>::new();
    assert!(full.grow(&mut unallocated, 1).is_err());
    assert_eq!(unallocated.capacity(), 0);
}

#[test]
fn folded_and_unfolded_header_lines_share_one_budget_across_parts() {
    let structure = ast(&multipart("MIXED", "m", &[text(), text()], "NIL"));
    let root = b"Content-Type: multipart/mixed; boundary=m\r\n\r\n".to_vec();
    let child = |count| {
        let mut bytes = b"Content-Type: text/plain\r\nX-Trace: synthetic\r\n".to_vec();
        bytes.extend(b" fold\r\n".repeat(count));
        bytes.extend(b"\r\n");
        bytes
    };
    let limit = crate::mime::MAX_HEADER_LINES;
    let mut evidence = [
        HeaderEvidence {
            section: Section(vec![]),
            bytes: root,
        },
        HeaderEvidence {
            section: Section(vec![1]),
            bytes: child(limit / 2 - 4),
        },
        HeaderEvidence {
            section: Section(vec![2]),
            bytes: child(limit / 2 - 4),
        },
    ];
    assert!(matches!(
        map(&structure, &evidence),
        Ok(ManifestOutcome::Ready(_))
    ));
    evidence[2].bytes = child(limit / 2 - 3);
    assert!(matches!(
        map(&structure, &evidence),
        Err(MapError::HeaderLimit)
    ));
}

#[test]
fn implicit_mime_charset_matches_server_default_ascii() {
    let structure = leaf(
        "TEXT",
        "PLAIN",
        "(\"CHARSET\" \"US-ASCII\")",
        "7BIT",
        3,
        "NIL",
        "NIL",
    );
    let manifest = compare(&raw_leaf("Content-Type: text/plain"), &structure);
    assert_eq!(manifest.parts[0].charset, "us-ascii");
}
