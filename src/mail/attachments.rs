//! Authenticated attachment delivery through the shared bounded MIME reader. Handles contain bounded metadata, never email bytes.
use super::*;
use crate::auth::Principal;
use axum::{
    Extension,
    body::{Body, Bytes},
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use std::{
    collections::HashMap,
    convert::Infallible,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

const HANDLE_TTL: Duration = Duration::from_secs(300);
const MAX_HANDLES: usize = 16;

#[derive(Clone)]
struct Part {
    index: usize,
    wire_size: usize,
    content_type: String,
    filename: Option<String>,
    attachment: bool,
}
#[derive(Clone)]
struct Transfer {
    principal: String,
    mailbox: String,
    uid: u32,
    validity: u32,
    part: Part,
    expires: Instant,
}
pub struct AttachmentStore {
    handles: Mutex<HashMap<String, Transfer>>,
    transfers: Arc<Semaphore>,
}
impl AttachmentStore {
    pub fn new(_config: Arc<Config>) -> Self {
        Self {
            handles: Mutex::new(HashMap::new()),
            transfers: Arc::new(Semaphore::new(2)),
        }
    }
    async fn insert(&self, transfer: Transfer) -> Result<String> {
        let mut handles = self.handles.lock().await;
        handles.retain(|_, t| t.expires > Instant::now());
        if handles.len() >= MAX_HANDLES {
            return Err(MailError::limit());
        }
        let token = uuid::Uuid::new_v4().simple().to_string();
        handles.insert(token.clone(), transfer);
        Ok(token)
    }
    async fn lookup(&self, token: &str, principal: &str) -> Option<Transfer> {
        let mut handles = self.handles.lock().await;
        handles.retain(|_, t| t.expires > Instant::now());
        handles
            .get(token)
            .filter(|t| t.principal == principal)
            .cloned()
    }
}

fn entries(raw: &[u8]) -> Result<Vec<Part>> {
    let parsed = mime::parse(raw)?;
    let mut all = Vec::new();
    leaves(&parsed, &mut all, 0)?;
    Ok(all
        .iter()
        .enumerate()
        .map(|(index, part)| Part {
            index,
            wire_size: part.encoded_len(),
            content_type: part.ctype.mimetype.clone(),
            filename: part.filename().map(|v| v.chars().take(255).collect()),
            attachment: part.is_attachment(),
        })
        .collect())
}

pub(super) fn has_attachment(raw: &[u8]) -> Result<bool> {
    Ok(entries(raw)?.iter().any(|part| part.attachment))
}

impl MailService {
    pub(super) async fn prepare_attachment(
        &self,
        name: &str,
        a: &Value,
        principal: &str,
    ) -> Result<Value> {
        let mut session = self.connect().await?;
        let mailbox = folder(a)?;
        let uid = uid(a)?;
        let state = session
            .examine(mailbox_name(mailbox)?)
            .await
            .map_err(|_| MailError::backend())?;
        check_validity(a, state.uid_validity)?;
        let validity = state.uid_validity.ok_or_else(MailError::backend)?;
        let raw = self.raw(&mut session, uid).await?;
        let entries = entries(&raw)?;
        if name == "list_attachments" {
            let offset = number(a, "attachment_offset", 0, 0, 999)? as usize;
            let mut metadata = Vec::new();
            let mut used = 1024;
            let mut next = None;
            for (i, p) in entries
                .iter()
                .enumerate()
                .skip(offset)
                .filter(|(_, p)| p.attachment)
            {
                let item = json!({"attachment_index":i,"filename":p.filename,"content_type":p.content_type,"encoded_size":p.wire_size,"downloadable":p.wire_size<=self.config.max_message});
                let size = serde_json::to_vec(&item)
                    .map_err(|_| MailError::backend())?
                    .len();
                if used + size > self.config.max_output / 2 {
                    next = Some(i);
                    break;
                }
                used += size;
                metadata.push(item);
            }
            return Ok(
                json!({"uid":uid,"uid_validity":validity,"attachments":metadata,"next_attachment_offset":next,"content_trust":"untrusted"}),
            );
        }
        let index = number(a, "attachment_index", 0, 0, 999)? as usize;
        let part = entries.get(index).ok_or_else(MailError::input)?.clone();
        if part.wire_size > self.config.max_message {
            return Err(MailError::limit());
        }
        let metadata = json!({"filename":part.filename,"content_type":part.content_type,"encoded_size":part.wire_size});
        let token = self
            .attachments
            .insert(Transfer {
                principal: principal.into(),
                mailbox: mailbox.into(),
                uid,
                validity,
                part,
                expires: Instant::now() + HANDLE_TTL,
            })
            .await?;
        Ok(
            json!({"download_url":format!("{}/attachments/{token}", self.config.resource.trim_end_matches('/')),"method":"GET","required_headers":{"Authorization":"Bearer <access_token>"},"expires_in_seconds":HANDLE_TTL.as_secs(),"metadata":metadata,"content_trust":"untrusted"}),
        )
    }
    async fn fetch_attachment(&self, transfer: &Transfer) -> Result<Vec<u8>> {
        let _permit = self
            .slots
            .acquire()
            .await
            .map_err(|_| MailError::backend())?;
        let mut session = self.connect().await?;
        let state = session
            .examine(mailbox_name(&transfer.mailbox)?)
            .await
            .map_err(|_| MailError::backend())?;
        check_validity(
            &json!({"uid_validity":transfer.validity}),
            state.uid_validity,
        )?;
        let raw = self.raw(&mut session, transfer.uid).await?;
        let parsed = mime::parse(&raw)?;
        let mut parts = Vec::new();
        leaves(&parsed, &mut parts, 0)?;
        parts
            .get(transfer.part.index)
            .ok_or_else(MailError::input)?
            .body_raw(self.config.max_message)
    }
}
/// Must be mounted inside the same OIDC middleware as the MCP endpoint.
pub async fn download(
    State(service): State<Arc<MailService>>,
    Extension(principal): Extension<Principal>,
    Path(token): Path<String>,
) -> Response {
    let backend = service.clone();
    download_with(service, principal, token, move |transfer| async move {
        backend.fetch_attachment(&transfer).await
    })
    .await
}
async fn download_with<F, Fut>(
    service: Arc<MailService>,
    principal: Principal,
    token: String,
    fetch: F,
) -> Response
where
    F: FnOnce(Transfer) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>>>,
{
    let Some(transfer) = service.attachments.lookup(&token, &principal.0).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(permit) = service.attachments.transfers.clone().try_acquire_owned() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    let filename = transfer
        .part
        .filename
        .clone()
        .unwrap_or_else(|| "attachment.bin".into());
    let bytes = match tokio::time::timeout(service.config.timeout, fetch(transfer)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return StatusCode::BAD_GATEWAY.into_response(),
        Err(_) => return StatusCode::GATEWAY_TIMEOUT.into_response(),
    };
    if bytes.len() > service.config.max_message {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let len = bytes.len();
    // Keep the transfer permit through response consumption/drop (including slow readers).
    let stream = futures::stream::unfold(
        (Some(Bytes::from(bytes)), permit),
        |(bytes, permit)| async move { bytes.map(|bytes| (Ok::<_, Infallible>(bytes), (None, permit))) },
    );
    let mut response = Body::from_stream(stream).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    let encoded = filename
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"._-".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect::<String>();
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"attachment.bin\"; filename*=UTF-8''{encoded}")
            .parse()
            .unwrap(),
    );
    headers.insert(header::CONTENT_LENGTH, len.into());
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    fn transfer(principal: &str, expires: Instant) -> Transfer {
        Transfer {
            principal: principal.into(),
            mailbox: "INBOX".into(),
            uid: 1,
            validity: 1,
            expires,
            part: Part {
                index: 1,
                wire_size: 4,
                content_type: "application/pdf".into(),
                filename: None,
                attachment: true,
            },
        }
    }
    #[tokio::test]
    async fn handles_are_principal_bound_expiring_and_capacity_bounded() {
        let store = AttachmentStore::new(crate::config::fixture());
        let token = store
            .insert(transfer("owner", Instant::now() + HANDLE_TTL))
            .await
            .unwrap();
        assert!(store.lookup(&token, "other").await.is_none());
        assert!(store.lookup(&token, "owner").await.is_some());
        assert!(store.lookup("bad", "owner").await.is_none());
        store.handles.lock().await.get_mut(&token).unwrap().expires =
            Instant::now() - Duration::from_secs(1);
        assert!(store.lookup(&token, "owner").await.is_none());
        for _ in 0..MAX_HANDLES {
            store
                .insert(transfer("owner", Instant::now() + HANDLE_TTL))
                .await
                .unwrap();
        }
        assert!(
            store
                .insert(transfer("owner", Instant::now() + HANDLE_TTL))
                .await
                .is_err()
        );
    }
    #[test]
    fn discovery_matches_message_indices_and_multipart_attachment_trees() {
        let raw = b"Content-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nbody\r\n--x\r\nContent-Type: multipart/mixed; boundary=y\r\nContent-Disposition: attachment; filename=group.mime\r\n\r\n--y\r\nContent-Type: text/plain\r\n\r\nattached\r\n--y--\r\n--x\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=test.pdf\r\n\r\npdf\r\n--x--\r\n";
        let entries = entries(raw).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].index, 1);
        assert_eq!(entries[1].content_type, "multipart/mixed");
        assert!(entries[1].attachment);
        assert_eq!(entries[2].index, 2);
        let message = parsed(raw, 1, Some(1), 1000).unwrap();
        assert_eq!(message["attachments"][0]["attachment_index"], 1);
        assert_eq!(message["attachments"][1]["attachment_index"], 2);
        assert_eq!(
            attachment(raw, 1, 1000).unwrap()["content_type"],
            "multipart/mixed"
        );
        assert!(has_attachment(raw).unwrap());
    }
    #[tokio::test]
    async fn http_download_authorization_binary_headers_and_transfer_capacity() {
        use axum::{Router, http::Request, routing::get};
        use tower::ServiceExt;
        let service = Arc::new(
            MailService::new(
                crate::config::fixture(),
                Arc::new(Journal::open(std::path::Path::new(":memory:")).unwrap()),
            )
            .unwrap(),
        );
        let token = service
            .attachments
            .insert(transfer("owner", Instant::now() + HANDLE_TTL))
            .await
            .unwrap();
        let route = Router::new()
            .route("/mcp/attachments/{token}", get(download))
            .with_state(service.clone());
        let request = || {
            Request::builder()
                .uri(format!("/mcp/attachments/{token}"))
                .body(Body::empty())
                .unwrap()
        };
        // The production router additionally runs OIDC middleware before this handler.
        assert_eq!(
            route.clone().oneshot(request()).await.unwrap().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        let mut wrong = request();
        wrong.extensions_mut().insert(Principal("other".into()));
        assert_eq!(
            route.clone().oneshot(wrong).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
        let mut bad = Request::builder()
            .uri("/mcp/attachments/bad")
            .body(Body::empty())
            .unwrap();
        bad.extensions_mut().insert(Principal("owner".into()));
        assert_eq!(
            route.oneshot(bad).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
        let expected = vec![0x25; 200 * 1024];
        let payload = expected.clone();
        let injected = Router::new()
            .route(
                "/mcp/attachments/{token}",
                get(
                    move |State(service): State<Arc<MailService>>,
                          Extension(principal): Extension<Principal>,
                          Path(token): Path<String>| {
                        let payload = payload.clone();
                        async move {
                            download_with(service, principal, token, |_| async move { Ok(payload) })
                                .await
                        }
                    },
                ),
            )
            .with_state(service.clone());
        let mut success = request();
        success.extensions_mut().insert(Principal("owner".into()));
        let response = injected.oneshot(success).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/octet-stream"
        );
        assert!(
            response.headers()[header::CONTENT_DISPOSITION]
                .to_str()
                .unwrap()
                .starts_with("attachment;")
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(service.attachments.transfers.available_permits(), 1);
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .as_ref(),
            expected
        );
        assert_eq!(service.attachments.transfers.available_permits(), 2);
        let _p1 = service.attachments.transfers.acquire().await.unwrap();
        let _p2 = service.attachments.transfers.acquire().await.unwrap();
        let rejected = download_with(
            service.clone(),
            Principal("owner".into()),
            token,
            |_| async { panic!("backend must not be called when at capacity") },
        )
        .await;
        assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
