use super::*;
use base64::Engine;

fn service() -> Arc<MailService> {
    Arc::new(
        MailService::new(
            crate::config::fixture(),
            Arc::new(Journal::open(std::path::Path::new(":memory:")).unwrap()),
        )
        .unwrap(),
    )
}

fn transfer(service: &MailService) -> Transfer {
    Transfer {
        principal:"owner".into(),account:service.account.clone(),mailbox:"收件箱".into(),uid:7,validity:9,
        part: entries(b"Content-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=synthetic.bin\r\n\r\nx").unwrap().remove(0),
        expires:Instant::now()+HANDLE_TTL,
    }
}

#[test]
fn opaque_ids_bind_every_reference_dimension_and_preserve_all_mime_indices() {
    let account = AccountKey::synthetic();
    let reference = AttachmentReference {
        alias: "default",
        account: &account,
        mailbox: "项目😀",
        uid: 7,
        validity: 9,
    };
    let id = reference.id(1001);
    assert_eq!(id.len(), 71);
    assert_eq!(reference.id(1001), id);
    assert_ne!(reference.id(1000), id);
    assert_ne!(
        AttachmentReference {
            uid: 8,
            ..reference
        }
        .id(1001),
        id
    );
    assert_ne!(
        AttachmentReference {
            validity: 10,
            ..reference
        }
        .id(1001),
        id
    );
    assert_ne!(
        AttachmentReference {
            mailbox: "项目",
            ..reference
        }
        .id(1001),
        id
    );
    assert_ne!(
        AttachmentReference {
            alias: "default",
            account: &AccountKey::synthetic(),
            ..reference
        }
        .id(1001),
        id
    );
    let mut raw = String::from("Content-Type: multipart/mixed; boundary=x\r\n\r\n");
    for _ in 0..1023 {
        raw.push_str("--x\r\nContent-Disposition: attachment; filename=same.txt\r\n\r\nx\r\n");
    }
    raw.push_str("--x--\r\n");
    let parts = entries(raw.as_bytes()).unwrap();
    assert_eq!(parts.len(), 1023);
    assert_eq!(
        select_part(&parts, &json!({"attachment_index":1022}), &reference).unwrap(),
        1022
    );
    let id = reference.id(1022);
    assert_eq!(
        select_part(&parts, &json!({"attachment_id":id}), &reference).unwrap(),
        1022
    );
    assert_eq!(
        select_part(
            &parts,
            &json!({"attachment_id":id,"attachment_index":1022}),
            &reference
        )
        .unwrap(),
        1022
    );
    let error = select_part(
        &parts,
        &json!({"attachment_id":id,"attachment_index":1000}),
        &reference,
    )
    .unwrap_err();
    assert_eq!(
        serde_json::to_value(error).unwrap()["details"]["reason"],
        "reference_mismatch"
    );
    assert_eq!(
        select_part(&parts, &json!({}), &reference)
            .unwrap_err()
            .code,
        "INVALID_INPUT"
    );
    assert_eq!(
        select_part(&parts, &json!({"attachment_index":1023}), &reference)
            .unwrap_err()
            .code,
        "ATTACHMENT_NOT_FOUND"
    );
}

#[test]
fn descriptor_distinguishes_unknown_encoding_size_accuracy_and_safe_names() {
    let account = AccountKey::synthetic();
    let reference = AttachmentReference {
        alias: "default",
        account: &account,
        mailbox: "INBOX",
        uid: 7,
        validity: 9,
    };
    let config = crate::config::fixture();
    for (encoding, body, size, accuracy, supported) in [
        ("8bit", "abc", Some(3), "exact", true),
        ("base64", "Y W J j\r\n", Some(3), "estimate", true),
        ("quoted-printable", "a=62c", None, "unknown", true),
        (
            "x-private-codec",
            "private encoded bytes",
            None,
            "unknown",
            false,
        ),
    ] {
        let raw = format!(
            "Content-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"../CON.txt\"\r\nContent-ID: <synthetic@example.test>\r\nContent-Transfer-Encoding: {encoding}\r\n\r\n{body}"
        );
        let parts = entries(raw.as_bytes()).unwrap();
        let metadata = descriptor(&parts[0], &reference, &config, None);
        assert_eq!(metadata["decoded_size"], json!(size));
        assert_eq!(metadata["size_accuracy"], accuracy);
        assert_eq!(metadata["downloadable"], supported);
        assert!(metadata["checksum"].is_null());
        assert!(metadata["detected_content_type"].is_null());
        assert_eq!(metadata["disposition"], "attachment");
        assert_eq!(metadata["content_id"], "<synthetic@example.test>");
        assert!(!metadata["safe_filename"].as_str().unwrap().contains('/'));
        if !supported {
            assert_eq!(
                metadata["unavailable_reason"],
                "unsupported_transfer_encoding"
            );
            let parsed = mime::parse(raw.as_bytes()).unwrap();
            assert_eq!(
                decode_part(&parsed, 1024).unwrap_err().code,
                "UNSUPPORTED_TRANSFER_ENCODING"
            );
        }
    }
    for name in ["CON.txt", "NUL", "..", "../a\\b:?.csv", "a\u{202e}.csv"] {
        let safe = metadata::safe_filename(Some(name), 3);
        assert!(!safe.contains(['/', '\\', ':', '?', '\u{202e}']));
        assert!(!["CON.txt", "NUL", ".."].contains(&safe.as_str()));
    }
    for name in ["COM¹.csv", "LPT³.csv", "CON .txt", "CONIN$", "CONOUT$"] {
        assert!(metadata::safe_filename(Some(name), 0).starts_with('_'));
    }
    let malformed = mime::parse(b"Content-Transfer-Encoding: base64\r\n\r\nA===").unwrap();
    assert_eq!(
        decode_part(&malformed, 1024).unwrap_err().code,
        "ATTACHMENT_DECODE_FAILED"
    );
}

#[test]
fn download_urls_encode_unicode_and_keep_the_configured_base_path() {
    let resource = "https://例子.测试/邮件/mcp/";
    let url = download_url(resource, "synthetic-handle").unwrap();
    assert!(url.is_ascii());
    assert!(url.contains("/%E9%82%AE%E4%BB%B6/mcp/attachments/synthetic-handle"));
    assert_eq!(resource, "https://例子.测试/邮件/mcp/");
}

#[test]
fn descriptor_delivery_modes_obey_the_shared_encoded_input_budget() {
    let account = AccountKey::synthetic();
    let reference = AttachmentReference {
        alias: "default",
        account: &account,
        mailbox: "INBOX",
        uid: 7,
        validity: 9,
    };
    let mut config = (*crate::config::fixture()).clone();
    config.max_output = 16 * 1024;
    let inline_limit = config.max_output / 4;
    let body = format!(
        "{}YQ==",
        " ".repeat(mime::attachment_input_limit(inline_limit) + 1)
    );
    let raw = format!(
        "Content-Type: application/octet-stream\r\nContent-Disposition: attachment\r\nContent-Transfer-Encoding: base64\r\n\r\n{body}"
    );
    let parts = entries(raw.as_bytes()).unwrap();
    let metadata = descriptor(&parts[0], &reference, &config, None);
    assert_eq!(metadata["decoded_size"], 1);
    assert_eq!(metadata["size_accuracy"], "estimate");
    assert_eq!(metadata["downloadable"], true);
    assert_eq!(
        metadata["delivery"]["available_modes"],
        json!(["authenticated_http"])
    );
    let parsed = mime::parse(raw.as_bytes()).unwrap();
    assert_eq!(
        decode_part(&parsed, inline_limit).unwrap_err().code,
        "LIMIT_EXCEEDED"
    );
    assert_eq!(decode_part(&parsed, config.max_message).unwrap(), b"a");
    let mut over = parts[0].clone();
    over.wire_size = mime::attachment_input_limit(config.max_message) + 1;
    let metadata = descriptor(&over, &reference, &config, None);
    assert_eq!(metadata["downloadable"], false);
    assert_eq!(metadata["unavailable_reason"], "decoded_attachment_bytes");
    assert_eq!(metadata["delivery"]["available_modes"], json!([]));
}

#[tokio::test]
async fn http_failure_classes_are_small_private_and_release_transfer_permits() {
    let service = service();
    let handle = service
        .attachments
        .insert(transfer(&service))
        .await
        .unwrap();
    for (error, status, code) in [
        (
            MailError {
                code: "STALE_UID",
                message: "private-backend",
                retryable: false,
                details: None,
            },
            409,
            "STALE_UID",
        ),
        (
            MailError {
                code: "NOT_FOUND",
                message: "private-backend",
                retryable: false,
                details: None,
            },
            404,
            "MESSAGE_NOT_FOUND",
        ),
        (attachment_missing(), 404, "ATTACHMENT_NOT_FOUND"),
        (unsupported_encoding(), 422, "UNSUPPORTED_TRANSFER_ENCODING"),
        (
            MailError::limit_kind(LimitKind::DecodedAttachmentBytes),
            413,
            "LIMIT_EXCEEDED",
        ),
        (MailError::backend(), 502, "MAIL_BACKEND"),
    ] {
        let response = download_with(
            service.clone(),
            Principal("owner".into()),
            handle.clone(),
            "synthetic-request-id".into(),
            |_| async move { Err(error) },
        )
        .await;
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(response.headers()["x-request-id"], "synthetic-request-id");
        let bytes = axum::body::to_bytes(response.into_body(), 2048)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], code);
        assert!(!String::from_utf8_lossy(&bytes).contains("private-backend"));
        assert!(!String::from_utf8_lossy(&bytes).contains(&handle));
        assert_eq!(
            service.attachments.transfers.available_permits(),
            MAX_TRANSFERS
        );
    }
}

#[tokio::test]
async fn four_slow_consumers_cancel_and_timeout_without_losing_capacity() {
    let service = service();
    let handle = service
        .attachments
        .insert(transfer(&service))
        .await
        .unwrap();
    let mut responses = Vec::new();
    for _ in 0..4 {
        responses.push(
            download_with(
                service.clone(),
                Principal("owner".into()),
                handle.clone(),
                uuid::Uuid::new_v4().to_string(),
                |_| async { Ok(vec![7; 65536]) },
            )
            .await,
        );
    }
    assert_eq!(service.attachments.transfers.available_permits(), 0);
    let rejected = download_with(
        service.clone(),
        Principal("owner".into()),
        handle.clone(),
        uuid::Uuid::new_v4().to_string(),
        |_| async { panic!("admission must precede fetch") },
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(rejected.headers()[header::RETRY_AFTER], "1");
    responses.pop();
    assert_eq!(service.attachments.transfers.available_permits(), 1);
    drop(responses);
    assert_eq!(service.attachments.transfers.available_permits(), 4);
    let running = service.clone();
    let token = handle.clone();
    let task = tokio::spawn(async move {
        download_with(
            running,
            Principal("owner".into()),
            token,
            uuid::Uuid::new_v4().to_string(),
            |_| std::future::pending(),
        )
        .await
    });
    tokio::task::yield_now().await;
    task.abort();
    let _ = task.await;
    assert_eq!(service.attachments.transfers.available_permits(), 4);
    let response = download_with(
        service.clone(),
        Principal("owner".into()),
        handle,
        uuid::Uuid::new_v4().to_string(),
        |_| std::future::pending(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(service.attachments.transfers.available_permits(), 4);
}

#[tokio::test]
async fn r00_csv_xlsx_pdf_bytes_survive_mime_decode_and_authenticated_download() {
    let service = service();
    let fixtures = [
        (
            &include_bytes!("../../../tests/fixtures/attachment_delivery/utf8-csv.csv")[..],
            "text/csv",
        ),
        (
            &include_bytes!("../../../tests/fixtures/attachment_delivery/workbook.xlsx")[..],
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ),
        (
            &include_bytes!("../../../tests/fixtures/attachment_delivery/paragraph.pdf")[..],
            "application/pdf",
        ),
    ];
    let reference = AttachmentReference {
        alias: "default",
        account: &service.account,
        mailbox: "收件箱",
        uid: 7,
        validity: 9,
    };
    for (bytes, content_type) in fixtures {
        let raw = format!(
            "Content-Type: {content_type}\r\nContent-Disposition: attachment; filename=\"验收 & 文件.bin\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        let inline = inline_download(
            raw.as_bytes(),
            &json!({"attachment_id":reference.id(0)}),
            &service.account,
            "收件箱",
            7,
            9,
            service.config.max_message,
        )
        .unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(inline["base64"].as_str().unwrap())
                .unwrap(),
            bytes
        );
        let handle = service
            .attachments
            .insert(transfer(&service))
            .await
            .unwrap();
        let payload = decode_part(
            &mime::parse(raw.as_bytes()).unwrap(),
            service.config.max_message,
        )
        .unwrap();
        let response = download_with(
            service.clone(),
            Principal("owner".into()),
            handle,
            uuid::Uuid::new_v4().to_string(),
            |_| async move { Ok(payload) },
        )
        .await;
        assert_eq!(
            response.headers()["x-content-sha256"],
            format!("{:x}", Sha256::digest(bytes))
        );
        assert_eq!(
            axum::body::to_bytes(response.into_body(), service.config.max_message)
                .await
                .unwrap()
                .as_ref(),
            bytes
        );
    }
}

#[tokio::test]
async fn attachment_fetch_reports_absence_only_after_successful_tagged_completion() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let cases = [
        (
            "* 2 FETCH (UID 99 FLAGS ())\r\n{tag} OK complete\r\n",
            None,
            "NOT_FOUND",
        ),
        (
            "* 1 FETCH (UID 7 FLAGS ())\r\n{tag} OK complete\r\n",
            None,
            "MAIL_BACKEND",
        ),
        (
            "* 1 FETCH (UID 7 RFC822.SIZE 3)\r\n{tag} NO refused\r\n",
            None,
            "MAIL_BACKEND",
        ),
        ("* 1 FETCH (UID 7 RFC822.SIZE 3)\r\n", None, "MAIL_BACKEND"),
        (
            "* 1 FETCH (UID 7 RFC822.SIZE 3)\r\n{tag} OK complete\r\n",
            Some("* 2 FETCH (UID 99 FLAGS ())\r\n{tag} OK complete\r\n"),
            "NOT_FOUND",
        ),
        (
            "* 1 FETCH (UID 7 RFC822.SIZE 3)\r\n{tag} OK complete\r\n",
            Some("* 1 FETCH (UID 7 FLAGS ())\r\n{tag} OK complete\r\n"),
            "MAIL_BACKEND",
        ),
        (
            "* 1 FETCH (UID 7 RFC822.SIZE 3)\r\n{tag} OK complete\r\n",
            Some("* 1 FETCH (UID 7 BODY[] {3}\r\nabc)\r\n{tag} NO refused\r\n"),
            "MAIL_BACKEND",
        ),
        (
            "* 1 FETCH (UID 7 RFC822.SIZE 8)\r\n{tag} OK complete\r\n",
            Some("* 1 FETCH (UID 7 BODY[] {4}\r\nYWJj)\r\n{tag} OK complete\r\n"),
            "MAIL_BACKEND",
        ),
        (
            "* 1 FETCH (UID 7 RFC822.SIZE 3)\r\n{tag} OK complete\r\n",
            Some("* 1 FETCH (UID 7 BODY[] {4}\r\nYWJj)\r\n{tag} OK complete\r\n"),
            "MAIL_BACKEND",
        ),
    ];
    for (metadata, body, expected) in cases {
        let (client, server) = tokio::io::duplex(8192);
        let backend = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            let mut line = String::new();
            server.read_line(&mut line).await.unwrap();
            let tag = line.split_whitespace().next().unwrap().to_owned();
            server
                .write_all(format!("{tag} OK login\r\n").as_bytes())
                .await
                .unwrap();
            for reply in std::iter::once(metadata).chain(body) {
                line.clear();
                server.read_line(&mut line).await.unwrap();
                let tag = line.split_whitespace().next().unwrap();
                server
                    .write_all(reply.replace("{tag}", tag).as_bytes())
                    .await
                    .unwrap();
            }
        });
        let mut session = async_imap::Client::new(client)
            .login("synthetic", "test-only")
            .await
            .unwrap();
        let error = service().raw_attachment(&mut session, 7).await.unwrap_err();
        assert_eq!(error.code, expected, "{metadata}");
        backend.await.unwrap();
    }
}

#[tokio::test]
async fn expired_wrong_principal_or_account_handles_return_private_reprepare_errors() {
    let service = service();
    let mut expired = transfer(&service);
    expired.expires = Instant::now() - Duration::from_secs(1);
    let expired = service.attachments.insert(expired).await.unwrap();
    let handle = service
        .attachments
        .insert(transfer(&service))
        .await
        .unwrap();
    let mut wrong_account = transfer(&service);
    wrong_account.account = AccountKey::synthetic();
    let wrong_account = service.attachments.insert(wrong_account).await.unwrap();
    for (token, principal) in [
        (expired, "owner"),
        (handle, "other"),
        (wrong_account, "owner"),
        ("unknown-synthetic".into(), "owner"),
    ] {
        let response = download_with(
            service.clone(),
            Principal(principal.into()),
            token.clone(),
            uuid::Uuid::new_v4().to_string(),
            |_| async { panic!("invalid handle must not fetch") },
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(response.into_body(), 2048)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "ATTACHMENT_HANDLE_NOT_FOUND");
        assert_eq!(body["error"]["action"], "prepare_attachment_download");
        assert!(!String::from_utf8_lossy(&bytes).contains(&token));
    }
    let fresh = service
        .attachments
        .insert(transfer(&service))
        .await
        .unwrap();
    let response = download_with(
        service,
        Principal("owner".into()),
        fresh,
        uuid::Uuid::new_v4().to_string(),
        |_| async { Ok(b"fresh complete bytes".to_vec()) },
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}
