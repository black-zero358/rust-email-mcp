use super::*;
use imap_proto::{AttributeValue, Response};

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
    assert_eq!(manifest.parts.len(), canonical.manifest.parts.len());
    for (actual, expected) in manifest.parts.iter().zip(&canonical.manifest.parts) {
        assert_eq!(actual.section, expected.section);
        assert_eq!(actual.attachment_index, expected.attachment_index);
        assert_eq!(actual.attachment_ordinal, expected.attachment_ordinal);
        assert_eq!(actual.is_attachment, expected.is_attachment);
        assert_eq!(actual.filename, expected.filename);
        assert_eq!(actual.content_type, expected.content_type);
        assert_eq!(actual.disposition, expected.disposition);
    }
    manifest
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
