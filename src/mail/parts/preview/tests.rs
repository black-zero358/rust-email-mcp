use super::*;

fn preview(text: &str, truncated: bool) -> Preview {
    Preview::Text {
        text: text.into(),
        truncated,
    }
}

#[test]
fn cap_counts_unicode_scalar_values_and_drops_the_trailing_separator() {
    let mut output = PreviewText::default();
    assert!(!output.append(&"😀".repeat(DISPLAY_CODE_POINTS - 1)));
    assert!(output.append("\r\n中"));
    assert_eq!(
        output.finish(true),
        preview(&"😀".repeat(DISPLAY_CODE_POINTS - 1), true)
    );
}

#[tokio::test]
async fn utf7_preview_uses_renderable_alternative_in_both_paths() {
    use crate::mail::tests::fetch_script;

    let root = "Content-Type: multipart/alternative; boundary=a\r\n\r\n";
    let utf8 = "Content-Type: text/plain; charset=UTF-8\r\n\r\n";
    let utf7 = "Content-Type: text/plain; charset=UTF-7\r\n\r\n";
    let raw = format!("{root}--a\r\n{utf8}fallback\r\n--a\r\n{utf7}+AOk-\r\n--a--\r\n");
    let expected = preview("fallback", false);
    let mut remaining = PAGE_ENCODED_BYTES;
    let from_raw = from_raw(raw.as_bytes(), &mut remaining).map_err(|e| e.error.code);

    let literal =
        |section, value: &str| format!(" BODY[{section}]<0> {{{}}}\r\n{value}", value.len());
    let script = vec![
        ("UID FETCH 7 (UID BODYSTRUCTURE)".into(),
            "* 1 FETCH (UID 7 BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 8 1 NIL NIL NIL NIL)(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-7\") NIL NIL \"7BIT\" 5 1 NIL NIL NIL NIL) \"ALTERNATIVE\" (\"BOUNDARY\" \"a\") NIL NIL))\r\n".into(), "OK"),
        ("UID FETCH 7 (UID BODY.PEEK[HEADER]<0.131073>)".into(),
            format!("* 1 FETCH (UID 7{})\r\n", literal("HEADER", root)), "OK"),
        ("UID FETCH 7 (UID BODY.PEEK[1.MIME]<0.131073> BODY.PEEK[2.MIME]<0.131073>)".into(),
            format!("* 1 FETCH (UID 7{}{})\r\n", literal("1.MIME", utf8), literal("2.MIME", utf7)), "OK"),
        ("UID FETCH 7 (UID BODY.PEEK[1]<0.8>)".into(),
            format!("* 1 FETCH (UID 7{})\r\n", literal("1", "fallback")), "OK"),
        ("UID FETCH 7 (UID BODY.PEEK[1]<8.1>)".into(),
            "* 1 FETCH (UID 7 BODY[1]<8> {0}\r\n)\r\n".into(), "OK"),
    ];
    let (mut session, backend) = fetch_script(script).await;
    let mut remaining = PAGE_ENCODED_BYTES;
    let structured = read(&mut session, 7, &mut remaining, |error| error)
        .await
        .map_err(|e| e.error.code);
    assert_eq!(from_raw, Ok(preview("fallback", false)));
    assert_eq!(structured, Ok(expected));
    backend.await.unwrap();
}
