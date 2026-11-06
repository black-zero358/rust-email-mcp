use super::*;
use base64::Engine;

#[tokio::test]
async fn structured_alternative_retries_render_failure_but_keeps_command_failure() {
    use crate::mail::{
        parts::{body_tree::BodyNode, mapping::Section},
        tests::fetch_script,
    };

    let good = "<b>Complete fallback</b>";
    let bad = "Discard this<svg>unsupported</svg>";
    let raw = format!(
        "Content-Type: multipart/alternative; boundary=a\r\n\r\n--a\r\nContent-Type: text/html\r\n\r\n{good}\r\n--a\r\nContent-Type: text/html\r\n\r\n{bad}\r\n--a--\r\n"
    );
    let parsed = mime::parse(raw.as_bytes()).unwrap();
    for completion in ["OK", "NO"] {
        let mut manifest = super::super::from_raw(&parsed).unwrap().manifest;
        manifest.body.nodes.push(BodyNode {
            parent: None,
            children: vec![1, 2],
            kind: BodyKind::Container {
                section: Section(vec![]),
                media_type: "multipart/alternative".into(),
                encoding: "7bit".into(),
                content_id: None,
                related_start: None,
            },
            nonempty: None,
        });
        manifest.body.nodes.extend((0..2).map(|index| BodyNode {
            parent: Some(0),
            children: Vec::new(),
            kind: BodyKind::Leaf(index),
            nonempty: None,
        }));
        let data = |section, text: &str, status| {
            (
                format!("UID FETCH 7 (UID BODY.PEEK[{section}]<0.65536>)"),
                format!(
                    "* 1 FETCH (UID 7 BODY[{section}]<0> {{{}}}\r\n{text})\r\n",
                    text.len()
                ),
                status,
            )
        };
        let eof = |section, len| {
            (
                format!("UID FETCH 7 (UID BODY.PEEK[{section}]<{len}.1>)"),
                format!("* 1 FETCH (UID 7 BODY[{section}]<{len}> {{0}}\r\n)\r\n"),
                "OK",
            )
        };
        let mut script = vec![data(2, bad, completion)];
        if completion == "OK" {
            script.extend([eof(2, bad.len()), data(1, good, "OK"), eof(1, good.len())]);
        }
        let (mut session, backend) = fetch_script(script).await;
        let result = canonical_structured(&mut session, 7, &mut manifest).await;
        backend.await.unwrap();
        if completion == "OK" {
            let Ok(record) = result else {
                panic!("expected a renderable alternative")
            };
            assert_eq!(
                record.display(HtmlDisplay::Legacy).unwrap().text,
                "Complete fallback"
            );
        } else {
            assert!(matches!(result, Err(StructuredFault::Command(_))));
        }
    }
}

#[test]
fn transfer_and_charset_cross_chunk_pipeline_matches_the_raw_oracle() {
    let utf8 = "中文与emoji😀\n".repeat(16).into_bytes();
    let cases = vec![
        ("utf-8", utf8),
        ("utf-8", "abc中文😀".repeat(8000).into_bytes()),
        ("utf-16", b"\xff\xfe\x2d\x4e\x3d\xd8\x00\xde".to_vec()),
        ("gbk", b"\xd6\xd0\x90\x30\x81\x30".to_vec()),
        ("iso-2022-jp", b"ascii\x1b$B$3$s$K$A$O\x1b(B\x1b$".to_vec()),
        ("iso-2022-jp", vec![0xef, 0xbb]),
        ("utf-8", vec![0xf0, 0x9f, 0x98]),
    ];
    for (label, bytes) in cases {
        for (name, encoding) in [
            ("7bit", transfer_decode::Encoding::Identity),
            ("base64", transfer_decode::Encoding::Base64),
            (
                "quoted-printable",
                transfer_decode::Encoding::QuotedPrintable,
            ),
        ] {
            let encoded = match encoding {
                transfer_decode::Encoding::Identity => bytes.clone(),
                transfer_decode::Encoding::Base64 => base64::engine::general_purpose::STANDARD
                    .encode(&bytes)
                    .into_bytes(),
                transfer_decode::Encoding::QuotedPrintable => bytes
                    .iter()
                    .flat_map(|byte| format!("={byte:02X}").into_bytes())
                    .collect(),
            };
            let mut raw=format!("Content-Type: text/plain; charset={label}\r\nContent-Transfer-Encoding: {name}\r\n\r\n").into_bytes();
            raw.extend_from_slice(&encoded);
            let expected = mime::parse(&raw).unwrap().plain_body().unwrap();
            for chunk in [1, 2, 3, 65535, 65536] {
                if encoded.len() > 16384 && chunk < 65535 {
                    continue;
                }
                let reserve = encoded.len() * 4;
                let mut transfer = transfer_decode::Decoder::new(encoding, reserve);
                let mut charset = charset_decode::CharsetDecoder::new(
                    label.as_bytes(),
                    !encoded.is_empty(),
                    reserve,
                )
                .unwrap();
                let mut text = String::new();
                for bytes in encoded.chunks(chunk) {
                    decode_transfer(&mut transfer, &mut charset, bytes, false, &mut text).unwrap();
                }
                decode_transfer(&mut transfer, &mut charset, &[], true, &mut text).unwrap();
                decode_charset(&mut charset, &[], true, &mut text).unwrap();
                assert_eq!(text, expected, "{label}/{name}/chunk={chunk}");
            }
        }
    }
}
