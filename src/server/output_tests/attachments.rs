//! The R00 byte fixtures traverse the real signed-JWT MCP/attachment router.
use super::*;
use crate::auth::{Auth, require_auth};
use aws_lc_rs::{
    encoding::{AsDer, Pkcs8V1Der},
    rsa::{KeyPair, KeySize},
    signature::KeyPair as _,
};
use axum::{
    http::{StatusCode, header},
    routing::get,
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
struct Fixture {
    fixture_id: String,
    filename: String,
    mime_type: String,
    decoded_size: usize,
    sha256: String,
    path: String,
}
fn fixtures() -> Vec<Fixture> {
    #[derive(Deserialize)]
    struct Manifest {
        fixtures: Vec<Fixture>,
    }
    serde_json::from_str::<Manifest>(include_str!(
        "../../../tests/fixtures/attachment_delivery/manifest.json"
    ))
    .unwrap()
    .fixtures
}
fn fixture_bytes(fixture: &Fixture) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/attachment_delivery")
            .join(&fixture.path),
    )
    .unwrap()
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn encoded_filename(name: &str) -> String {
    name.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"._-".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn fixture_message(fixtures: &[Fixture]) -> Vec<u8> {
    let mut message = b"Message-ID: <fixtures@example.test>\r\nContent-Type: multipart/mixed; boundary=fixture-boundary\r\n\r\n--fixture-boundary\r\nContent-Type: text/plain\r\n\r\nSynthetic attachment acceptance\r\n".to_vec();
    for fixture in fixtures {
        let bytes = fixture_bytes(fixture);
        assert_eq!(bytes.len(), fixture.decoded_size);
        assert_eq!(digest(&bytes), fixture.sha256);
        message.extend_from_slice(format!("--fixture-boundary\r\nContent-Type: {}\r\nContent-Disposition: attachment; filename*=utf-8''{}\r\nContent-Transfer-Encoding: base64\r\n\r\n",fixture.mime_type,encoded_filename(&fixture.filename)).as_bytes());
        for line in STANDARD.encode(bytes).as_bytes().chunks(76) {
            message.extend_from_slice(line);
            message.extend_from_slice(b"\r\n");
        }
    }
    message.extend_from_slice(b"--fixture-boundary--\r\n");
    message
}

struct Credentials {
    auth: Arc<Auth>,
    signing: EncodingKey,
    header: Header,
    base_claims: Value,
}
impl Credentials {
    fn new(config: Arc<config::Config>) -> Self {
        let key = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let public = key.public_key();
        let jwks = serde_json::from_value(json!({"keys":[{"kty":"RSA","kid":"isolated-attachment","alg":"RS256","use":"sig","n":URL_SAFE_NO_PAD.encode(public.modulus().big_endian_without_leading_zero()),"e":URL_SAFE_NO_PAD.encode(public.exponent().big_endian_without_leading_zero())}]})).unwrap();
        let der = AsDer::<Pkcs8V1Der>::as_der(&key).unwrap();
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----",
            STANDARD.encode(der.as_ref())
        );
        let signing = EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let base_claims = json!({"sub":"owner","iss":config.issuer,"aud":config.audience,"exp":now+300,"nbf":now-1,"scope":"openid email:mcp","typ":"Bearer"});
        let auth = Auth::from_test_jwks(config, jwks);
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("isolated-attachment".into());
        Self {
            auth,
            signing,
            header,
            base_claims,
        }
    }
    fn token(&self) -> String {
        self.changed("sub", json!("owner"))
    }
    fn changed(&self, field: &str, value: Value) -> String {
        let mut claims = self.base_claims.clone();
        claims[field] = value;
        encode(&self.header, &claims, &self.signing).unwrap()
    }
}

fn router(service: Arc<MailService>, auth: Arc<Auth>) -> Router {
    let mail = service.clone();
    let transport = StreamableHttpService::new(
        move || Ok(EmailServer(mail.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    Router::new()
        .route(
            "/mcp/attachments/{token}",
            get(crate::mail::attachments::download),
        )
        .with_state(service)
        .nest_service("/mcp", transport)
        .layer(middleware::from_fn(limit_rpc_id))
        .layer(middleware::from_fn_with_state(auth, require_auth))
}
async fn call(app: &Router, token: &str, name: &str, args: Value) -> Value {
    let response = app.clone().oneshot(Request::post("/mcp").header("Host","localhost")
        .header(header::AUTHORIZATION,format!("Bearer {token}"))
        .header(header::CONTENT_TYPE,"application/json").header(header::ACCEPT,"application/json, text/event-stream").header("MCP-Protocol-Version","2025-06-18")
        .body(Body::from(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let bytes = axum::body::to_bytes(response.into_body(), 3 * 1024 * 1024)
        .await
        .unwrap();
    let wire: Value = serde_json::from_slice(&bytes).unwrap();
    let content = wire["result"]["structuredContent"].clone();
    assert_eq!(content["request_id"], request_id);
    assert_contract(name, &content);
    content
}
async fn get_attachment(app: &Router, token: Option<&str>, url: &str) -> axum::response::Response {
    let path = if url.starts_with("https://") {
        reqwest::Url::parse(url).unwrap().path().to_owned()
    } else {
        url.into()
    };
    let mut request = Request::get(path).header("Host", "localhost");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn signed_mcp_to_attachment_get_preserves_all_r00_file_bytes_and_hashes() {
    let fixture = fixtures();
    let h = Harness::new(
        BTreeMap::from([(7, fixture_message(&fixture))]),
        None,
        2 * 1024 * 1024,
    )
    .await;
    let credentials = Credentials::new(h.service.config.clone());
    let app = router(h.service.clone(), credentials.auth.clone());
    let token = credentials.token();
    let listed = call(
        &app,
        &token,
        "list_attachments",
        json!({"uid":7,"uid_validity":9}),
    )
    .await;
    let descriptors = listed["data"]["attachments"].as_array().unwrap();
    assert_eq!(descriptors.len(), 8);
    assert!(listed["data"]["next_attachment_offset"].is_null());
    for (index, fixture) in fixture.iter().enumerate() {
        let descriptor = &descriptors[index];
        assert_eq!(descriptor["attachment_index"], index + 1);
        assert_eq!(descriptor["filename"], fixture.filename);
        assert_eq!(descriptor["declared_content_type"], fixture.mime_type);
        assert_eq!(descriptor["delivery"]["client_delivery_required"], true);
        let id = descriptor["attachment_id"].as_str().unwrap();
        let prepared = call(
            &app,
            &token,
            "prepare_attachment_download",
            json!({"uid":7,"uid_validity":9,"attachment_id":id}),
        )
        .await;
        assert_eq!(prepared["data"]["attachment_id"], id);
        assert_eq!(prepared["data"]["metadata"]["filename"], fixture.filename);
        let response = get_attachment(
            &app,
            Some(&token),
            prepared["data"]["download_url"].as_str().unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{}", fixture.fixture_id);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/octet-stream"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        let disposition = response.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap();
        assert!(disposition.contains(&format!(
            "filename*=UTF-8''{}",
            encoded_filename(descriptor["safe_filename"].as_str().unwrap())
        )));
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(bytes.len(), fixture.decoded_size);
        assert_eq!(digest(&bytes), fixture.sha256);
        assert_eq!(bytes.as_ref(), fixture_bytes(fixture));
        let inline = call(
            &app,
            &token,
            "download_attachment",
            json!({"uid":7,"uid_validity":9,"attachment_index":index+1,"attachment_id":id}),
        )
        .await;
        if fixture.decoded_size <= 512 * 1024 {
            let decoded = STANDARD
                .decode(inline["data"]["base64"].as_str().unwrap())
                .unwrap();
            assert_eq!(digest(&decoded), fixture.sha256);
            assert_eq!(inline["data"]["checksum"]["value"], fixture.sha256);
        } else {
            assert_eq!(inline["error"]["code"], "LIMIT_EXCEEDED");
        }
    }
    assert_ne!(
        descriptors[3]["attachment_id"],
        descriptors[4]["attachment_id"]
    );
    assert_eq!(descriptors[3]["filename"], descriptors[4]["filename"]);
}

async fn assert_http_error(
    response: axum::response::Response,
    status: StatusCode,
    code: &str,
    phase: &str,
) -> Value {
    assert_eq!(response.status(), status);
    assert!(
        response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(uuid::Uuid::parse_str(&request_id).is_ok());
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    let mut error_properties = json!({"code":{"type":"string","enum":["UNAUTHORIZED","AUTH_UNAVAILABLE","ATTACHMENT_HANDLE_NOT_FOUND","MESSAGE_NOT_FOUND","ATTACHMENT_NOT_FOUND","STALE_UID","UNSUPPORTED_TRANSFER_ENCODING","LIMIT_EXCEEDED","BUSY","MAIL_BACKEND","ATTACHMENT_DECODE_FAILED","TIMEOUT"]},"message":{"type":"string"},
        "phase":{"type":"string","enum":["authentication","handle_lookup","mailbox_reference","message_fetch","attachment_decode","transfer_admission","backend_fetch"]},
        "action":{"type":"string","enum":["refresh_authentication","prepare_attachment_download","refresh_message_reference","locate_message","choose_supported_attachment","use_smaller_attachment","retry_with_backoff","retry_read"]}});
    let mut required = vec!["code", "message", "phase", "action"];
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        error_properties["limit_kind"] = json!({"type":"string","enum":["raw_message_bytes","decoded_attachment_bytes","decoded_body_bytes","mime_structure","header_bytes","wire_response_bytes","scan_items","cache_quota"]});
        error_properties["limit"] = json!({"type":"integer","minimum":0});
        required.push("limit_kind");
    }
    let schema = json!({"type":"object","properties":{"error":{"type":"object","properties":error_properties,"required":required,"additionalProperties":false},"request_id":{"type":"string","format":"uuid"}},"required":["error","request_id"],"additionalProperties":false});
    let validator = jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&schema)
        .unwrap();
    assert!(
        validator.is_valid(&value),
        "HTTP attachment error violates its contract: {value}"
    );
    assert_eq!(value["request_id"], request_id);
    assert_eq!(value["error"]["code"], code);
    assert_eq!(value["error"]["phase"], phase);
    assert!(value["error"]["action"].is_string());
    value
}

#[tokio::test]
async fn attachment_authentication_uses_real_jwt_validation_and_private_json_errors() {
    let h = Harness::new(BTreeMap::from([(7, multipart())]), None, 64 * 1024).await;
    let credentials = Credentials::new(h.service.config.clone());
    let app = router(h.service.clone(), credentials.auth.clone());
    let secret_path = "/mcp/attachments/private-handle-value";
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let bad_tokens = [
        "private-invalid-token".into(),
        credentials.changed("sub", json!("private-other")),
        credentials.changed("iss", json!("https://private-other.test/")),
        credentials.changed("aud", json!("private-api")),
        credentials.changed("exp", json!(now - 300)),
        credentials.changed("nbf", json!(now + 300)),
        credentials.changed("scope", json!("private-scope")),
    ];
    for token in std::iter::once(None).chain(bad_tokens.iter().map(|t| Some(t.as_str()))) {
        let response = get_attachment(&app, token, secret_path).await;
        assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
        let error = assert_http_error(
            response,
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "authentication",
        )
        .await;
        let serialized = error.to_string();
        assert!(!serialized.contains("private"));
        if let Some(token) = token {
            assert!(!serialized.contains(token));
        }
    }
    let valid = get_attachment(&app, Some(&credentials.token()), secret_path).await;
    let error = assert_http_error(
        valid,
        StatusCode::NOT_FOUND,
        "ATTACHMENT_HANDLE_NOT_FOUND",
        "handle_lookup",
    )
    .await;
    assert!(!error.to_string().contains("private"));
    // Ordinary MCP/OAuth rejection retains its previous empty body and challenge.
    let response = app
        .oneshot(
            Request::get("/mcp")
                .header(header::AUTHORIZATION, "Bearer private-invalid-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
    assert!(
        axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn attachment_id_and_index_contracts_keep_legacy_calls_and_reject_conflicts() {
    let h = Harness::new(BTreeMap::from([(7, multipart())]), None, 64 * 1024).await;
    let credentials = Credentials::new(h.service.config.clone());
    let token = credentials.token();
    let app = router(h.service.clone(), credentials.auth.clone());
    let listed = call(
        &app,
        &token,
        "list_attachments",
        json!({"uid":7,"uid_validity":9}),
    )
    .await;
    let first = &listed["data"]["attachments"][0];
    let id = first["attachment_id"].as_str().unwrap();
    for tool in ["download_attachment", "prepare_attachment_download"] {
        for selection in [
            json!({"attachment_id":id}),
            json!({"attachment_index":1}),
            json!({"attachment_id":id,"attachment_index":1}),
        ] {
            let mut args = json!({"uid":7,"uid_validity":9});
            args.as_object_mut()
                .unwrap()
                .extend(selection.as_object().unwrap().clone());
            assert!(call(&app, &token, tool, args).await.get("data").is_some());
        }
        let missing = call(&app, &token, tool, json!({"uid":7,"uid_validity":9})).await;
        assert_eq!(
            missing["error"]["details"]["reason"],
            "attachment_reference_required"
        );
        let invalid = call(
            &app,
            &token,
            tool,
            json!({"uid":7,"uid_validity":9,"attachment_id":format!("att_v1_{}","A".repeat(64))}),
        )
        .await;
        assert_eq!(
            invalid["error"]["details"]["reason"],
            "invalid_attachment_id"
        );
        let conflict = call(
            &app,
            &token,
            tool,
            json!({"uid":7,"uid_validity":9,"attachment_id":id,"attachment_index":2}),
        )
        .await;
        assert_eq!(conflict["error"]["details"]["reason"], "reference_mismatch");
    }
}

#[tokio::test]
async fn attachment_partial_pages_and_output_fallback_keep_reference_contracts() {
    let fixture = fixtures();
    let h = Harness::new(BTreeMap::from([(7, fixture_message(&fixture))]), None, 8192).await;
    let mut configuration = (*h.service.config).clone();
    configuration.subjects.push("reader".into());
    let credentials = Credentials::new(Arc::new(configuration));
    let token = credentials.token();
    let app = router(h.service.clone(), credentials.auth.clone());
    let mut offset = 0;
    let mut indices = Vec::new();
    let mut pages = 0;
    loop {
        let page = call(
            &app,
            &token,
            "list_attachments",
            json!({"uid":7,"uid_validity":9,"attachment_offset":offset}),
        )
        .await;
        let items = page["data"]["attachments"].as_array().unwrap();
        assert!(
            !items.is_empty(),
            "an attachment page must advance or report a limit error"
        );
        indices.extend(
            items
                .iter()
                .map(|item| item["attachment_index"].as_u64().unwrap()),
        );
        pages += 1;
        match page["data"]["next_attachment_offset"].as_u64() {
            Some(next) => {
                assert!(next > offset);
                offset = next;
            }
            None => break,
        }
    }
    assert!(pages > 1);
    assert_eq!(indices, (1..=8).collect::<Vec<u64>>());
    let prepared = call(
        &app,
        &token,
        "prepare_attachment_download",
        json!({"uid":7,"uid_validity":9,"attachment_index":1}),
    )
    .await;
    let foreign = get_attachment(
        &app,
        Some(&credentials.changed("sub", json!("reader"))),
        prepared["data"]["download_url"].as_str().unwrap(),
    )
    .await;
    let error = assert_http_error(
        foreign,
        StatusCode::NOT_FOUND,
        "ATTACHMENT_HANDLE_NOT_FOUND",
        "handle_lookup",
    )
    .await;
    assert!(!error.to_string().contains("reader"));
    assert!(
        !error
            .to_string()
            .contains(prepared["data"]["download_url"].as_str().unwrap())
    );
    let compact = Harness::new(BTreeMap::from([(7, multipart())]), None, 1400).await;
    let credentials = Credentials::new(compact.service.config.clone());
    let app = router(compact.service.clone(), credentials.auth.clone());
    let fallback = call(
        &app,
        &credentials.token(),
        "prepare_attachment_download",
        json!({"uid":7,"uid_validity":9,"attachment_index":1}),
    )
    .await;
    assert_eq!(
        fallback["error"]["details"]["limit_kind"],
        "wire_response_bytes"
    );
}

/// Run separately with ADAPTER_NODE_BINARY set to Node.js 24. The default Rust
/// suite has no Node dependency; this acceptance executes the standalone client.
#[tokio::test]
#[ignore = "requires ADAPTER_NODE_BINARY pointing to Node.js 24 for the optional client adapter"]
async fn standalone_node_adapter_consumes_real_rust_mcp_and_authorized_get() {
    let binary =
        std::env::var_os("ADAPTER_NODE_BINARY").expect("Set ADAPTER_NODE_BINARY to Node.js 24");
    let fixture = fixtures();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let resource = format!("{origin}/mcp");
    let h = Harness::with_resource(
        BTreeMap::from([(7, fixture_message(&fixture))]),
        None,
        2 * 1024 * 1024,
        false,
        Some(resource.clone()),
    )
    .await;
    let credentials = Credentials::new(h.service.config.clone());
    let app = router(h.service.clone(), credentials.auth.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let destination = tempfile::tempdir().unwrap();
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts/attachment-client-adapter/standard-client.mjs");
    for (index, fixture) in fixture.iter().enumerate() {
        let report = destination.path().join(format!("report-{index}.json"));
        let mut child = tokio::process::Command::new(&binary);
        child
            .arg(&script)
            .env("ADAPTER_MCP_URL", &resource)
            .env("ADAPTER_BEARER_TOKEN", credentials.token())
            .env(
                "ADAPTER_REQUEST_JSON",
                json!({"uid":7,"uid_validity":9,"attachment_index":index+1}).to_string(),
            )
            .env("ADAPTER_DESTINATION", destination.path())
            .env("ADAPTER_EXPECTED_SHA256", &fixture.sha256)
            .env("ADAPTER_REPORT", &report)
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(30), child.output())
            .await
            .expect("adapter deadline")
            .unwrap();
        assert!(
            output.status.success(),
            "adapter failed with sanitized stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
        let path = report["path"]
            .as_str()
            .expect("adapter report contains its private output path");
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(bytes.len(), fixture.decoded_size);
        assert_eq!(digest(&bytes), fixture.sha256);
    }
    server.abort();
}
