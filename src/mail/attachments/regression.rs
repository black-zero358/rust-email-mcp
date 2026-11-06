use super::*;
use base64::Engine;

#[tokio::test]
async fn r00_csv_xlsx_pdf_bytes_survive_mime_decode_and_staged_download() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = (*crate::config::fixture()).clone();
    config.attachment_staging_dir = Some(directory.path().join("private"));
    let config = Arc::new(config);
    let store = AttachmentStore::new(config.clone());
    let account = AccountKey::synthetic();
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
        account: &account,
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
            &account,
            "收件箱",
            7,
            9,
            config.max_message,
        )
        .unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(inline["base64"].as_str().unwrap())
                .unwrap(),
            bytes
        );
        let part = entries(raw.as_bytes()).unwrap().remove(0);
        let transfer = Transfer {
            principal: "owner".into(),
            account: account.clone(),
            mailbox: "收件箱".into(),
            uid: 7,
            validity: 9,
            part,
            cache: None,
            expires: Instant::now() + HANDLE_TTL,
        };
        let decoded =
            decode_part(&mime::parse(raw.as_bytes()).unwrap(), config.max_message).unwrap();
        let response = download_staged_selected(
            &store,
            &config,
            transfer,
            uuid::Uuid::new_v4().to_string(),
            move |_, stage, _| async move {
                let stage = stage
                    .write_chunk(decoded)
                    .await
                    .map_err(parts::fetch::storage_error)?;
                let proof = parts::fetch::completion_for_test(stage.id());
                stage
                    .finish(proof, staging::ExpectedFile::default())
                    .await
                    .map_err(parts::fetch::storage_error)
            },
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/octet-stream"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.headers()["x-content-sha256"],
            format!("{:x}", Sha256::digest(bytes))
        );
        assert_eq!(
            axum::body::to_bytes(response.into_body(), config.max_message)
                .await
                .unwrap()
                .as_ref(),
            bytes
        );
        assert_eq!(store.transfers.available_permits(), MAX_TRANSFERS);
    }
}
