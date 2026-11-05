use super::*;
use base64::Engine;

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

#[test]
fn html_rendering_context_stays_shared_across_selected_leaves() {
    let mut context = DisplayContext::new(HtmlDisplay::Readable);
    let mut body = String::new();
    for number in 0..2 {
        let input = format!("<a href='https://example.test/{number}'>link</a>");
        let mut transfer =
            transfer_decode::Decoder::new(transfer_decode::Encoding::Identity, input.len() * 4);
        let mut charset =
            charset_decode::CharsetDecoder::new(b"utf-8", true, input.len() * 4).unwrap();
        let mut text = String::new();
        decode_transfer(
            &mut transfer,
            &mut charset,
            input.as_bytes(),
            false,
            &mut text,
        )
        .unwrap();
        decode_transfer(&mut transfer, &mut charset, &[], true, &mut text).unwrap();
        decode_charset(&mut charset, &[], true, &mut text).unwrap();
        body.push_str(&crate::html::render_selected(&text, &mut context).unwrap());
    }
    assert!(body.contains("https://example.test/0"));
    assert!(body.contains("https://example.test/1"));
    assert_eq!(
        serde_json::to_value(context.into_metadata()).unwrap()["html_parts"],
        2
    );
}
