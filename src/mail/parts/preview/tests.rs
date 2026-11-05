use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

type Exchange = (String, Vec<u8>, &'static str);

fn ok(command: impl Into<String>, response: impl Into<Vec<u8>>) -> Exchange {
    (command.into(), response.into(), "OK")
}

fn literal(section: &str, origin: usize, bytes: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "* 1 FETCH (UID 7 BODY[{section}]<{origin}> {{{}}}\r\n",
        bytes.len()
    )
    .into_bytes();
    response.extend_from_slice(bytes);
    response.extend_from_slice(b")\r\n");
    response
}

fn leaf(charset: &str, encoding: &str, octets: usize) -> String {
    format!(
        "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"{charset}\") NIL NIL \"{encoding}\" {octets} 1 NIL NIL)"
    )
}

fn structure(value: &str) -> Exchange {
    ok(
        "UID FETCH 7 (UID BODYSTRUCTURE)",
        format!("* 1 FETCH (UID 7 BODYSTRUCTURE {value})\r\n").into_bytes(),
    )
}

fn single(charset: &str, encoding: &str, octets: usize) -> Vec<Exchange> {
    vec![
        structure(&leaf(charset, encoding, octets)),
        ok(
            "UID FETCH 7 (UID BODY.PEEK[HEADER]<0.131073>)",
            literal(
                "HEADER",
                0,
                format!("Content-Type: text/plain; charset={charset}\r\nContent-Transfer-Encoding: {encoding}\r\n\r\n").as_bytes(),
            ),
        ),
    ]
}

fn body(section: &str, origin: usize, request: usize, bytes: &[u8]) -> Exchange {
    ok(
        format!("UID FETCH 7 (UID BODY.PEEK[{section}]<{origin}.{request}>)"),
        literal(section, origin, bytes),
    )
}

async fn run(script: Vec<Exchange>, mut remaining: usize) -> (Result<Preview, ReadFault>, usize) {
    let (client, server) = tokio::io::duplex(16384);
    let task = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        for (expected, reply, status) in
            std::iter::once(ok("LOGIN \"synthetic\" \"test\"", Vec::new())).chain(script)
        {
            let mut line = String::new();
            server.read_line(&mut line).await.unwrap();
            let (tag, command) = line.trim_end().split_once(' ').unwrap();
            assert_eq!(command, expected);
            server.write_all(&reply).await.unwrap();
            server
                .write_all(format!("{tag} {status} synthetic\r\n").as_bytes())
                .await
                .unwrap();
        }
        let mut line = String::new();
        assert_eq!(server.read_line(&mut line).await.unwrap(), 0, "{line}");
    });
    let mut session = async_imap::Client::new(client)
        .login("synthetic", "test")
        .await
        .unwrap();
    let result = read(&mut session, 7, &mut remaining).await;
    drop(session);
    task.await.unwrap();
    (result, remaining)
}

fn preview(text: &str, truncated: bool) -> Preview {
    Preview::Text {
        text: text.into(),
        truncated,
    }
}

#[tokio::test]
async fn complete_leaves_share_budget_and_collapse_whitespace_across_boundaries() {
    let first = " \t中文\r\n".as_bytes();
    let second = " 😀  结束 \n".as_bytes();
    let header = b"Content-Type: text/plain; charset=utf-8\r\n\r\n";
    let mut child_headers = literal("1.MIME", 0, header);
    child_headers.extend(literal("2.MIME", 0, header));
    let (result, remaining) = run(
        vec![
            structure(&format!(
                "({}{} \"MIXED\" (\"BOUNDARY\" \"p\") NIL)",
                leaf("utf-8", "7bit", first.len()),
                leaf("utf-8", "7bit", second.len())
            )),
            ok(
                "UID FETCH 7 (UID BODY.PEEK[HEADER]<0.131073>)",
                literal(
                    "HEADER",
                    0,
                    b"Content-Type: multipart/mixed; boundary=p\r\n\r\n",
                ),
            ),
            ok(
                "UID FETCH 7 (UID BODY.PEEK[1.MIME]<0.131073> BODY.PEEK[2.MIME]<0.131073>)",
                child_headers,
            ),
            body("1", 0, first.len(), first),
            body("1", first.len(), 1, b""),
            body("2", 0, second.len(), second),
            body("2", second.len(), 1, b""),
        ],
        PAGE_ENCODED_BYTES,
    )
    .await;
    assert_eq!(result.ok(), Some(preview("中文 😀 结束", false)));
    assert_eq!(
        remaining,
        PAGE_ENCODED_BYTES - first.len() - second.len() - 2
    );
}

#[tokio::test]
async fn partial_transfer_and_charset_sequences_are_not_flushed() {
    for (encoding, bytes, sample, expected) in [
        ("7bit", "a😀zzz".as_bytes(), 4, "a"),
        ("base64", b"YWJjZA==".as_slice(), 6, "abc"),
        ("quoted-printable", b"ok=C3=A9".as_slice(), 5, "ok"),
    ] {
        let mut script = single("utf-8", encoding, bytes.len());
        script.push(body("1", 0, sample, &bytes[..sample]));
        let (result, remaining) = run(script, sample).await;
        assert_eq!(result.ok(), Some(preview(expected, true)), "{encoding}");
        assert_eq!(remaining, 0);
    }
}

#[tokio::test]
async fn per_message_budget_limits_the_request_even_with_page_credit() {
    let bytes = vec![b'a'; MESSAGE_ENCODED_BYTES + 1];
    let mut script = single("utf-8", "7bit", bytes.len());
    script.push(body(
        "1",
        0,
        MESSAGE_ENCODED_BYTES,
        &bytes[..MESSAGE_ENCODED_BYTES],
    ));
    let (result, remaining) = run(script, PAGE_ENCODED_BYTES).await;
    assert_eq!(
        result.ok(),
        Some(preview(&"a".repeat(DISPLAY_CODE_POINTS), true))
    );
    assert_eq!(remaining, PAGE_ENCODED_BYTES - MESSAGE_ENCODED_BYTES);

    let (result, remaining) = run(vec![], 0).await;
    assert_eq!(result.ok(), Some(Preview::BudgetSkipped));
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn terminal_command_faults_and_local_charset_errors_keep_distinct_state() {
    let mut script = single("utf-8", "7bit", 3);
    script.push((
        "UID FETCH 7 (UID BODY.PEEK[1]<0.3>)".into(),
        literal("1", 0, b"abc"),
        "NO",
    ));
    let (result, remaining) = run(script, 10).await;
    let fault = result.unwrap_err();
    assert_eq!(fault.error.code, "MAIL_BACKEND");
    assert!(!fault.reusable);
    assert_eq!(remaining, 7);

    let (result, remaining) = run(single("utf-7", "7bit", 3), 10).await;
    let fault = result.unwrap_err();
    assert_eq!(fault.error.code, "UNSUPPORTED_BODY");
    assert!(fault.reusable);
    assert_eq!(remaining, 10);
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

#[test]
fn validated_raw_uses_the_same_codecs_and_only_charges_sampled_bytes() {
    let base64 = b"Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\nYWJjZA==";
    let mut remaining = 6;
    assert_eq!(
        from_raw(base64, &mut remaining).ok(),
        Some(preview("abc", true))
    );
    assert_eq!(remaining, 0);
    let mut remaining = 8;
    assert_eq!(
        from_raw(base64, &mut remaining).ok(),
        Some(preview("abcd", false))
    );
    assert_eq!(remaining, 0);
    let utf7 = b"Content-Type: text/plain; charset=utf-7\r\n\r\n+Ti0-";
    let mut remaining = 10;
    let fault = from_raw(utf7, &mut remaining).unwrap_err();
    assert_eq!(fault.error.code, "UNSUPPORTED_BODY");
    assert!(fault.reusable);
    assert_eq!(remaining, 10);
}
