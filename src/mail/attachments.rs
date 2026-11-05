//! Authenticated attachment delivery through the shared bounded MIME reader. Handles contain bounded metadata, never email bytes.
use super::*;
use crate::auth::{Principal, RequestId};
use axum::{
    Extension,
    body::{Body, Bytes},
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
#[cfg(test)]
use std::convert::Infallible;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Semaphore};

const HANDLE_TTL: Duration = Duration::from_secs(600);
const MAX_HANDLES: usize = 64;
const MAX_TRANSFERS: usize = 4;
const MIME_REFERENCE_VERSION: u32 = 1;
pub(crate) const MAX_ATTACHMENT_INDEX: usize = mime::MAX_PARTS - 1;

mod cache;
mod metadata;
#[cfg(test)]
mod regression;
pub(in crate::mail) mod staging;
use metadata::{AttachmentReference, descriptor, select_part};

fn download_url(resource: &str, token: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(resource).map_err(|_| MailError::input())?;
    url.set_query(None);
    url.set_fragment(None);
    url.path_segments_mut()
        .map_err(|_| MailError::input())?
        .pop_if_empty()
        .push("attachments")
        .push(token);
    Ok(url.into())
}

#[derive(Clone, Default)]
struct Part {
    index: usize,
    wire_size: usize,
    content_type: String,
    filename: Option<String>,
    attachment: bool,
    decoded_size: Option<usize>,
    size_accuracy: &'static str,
    disposition: Option<String>,
    content_id: Option<String>,
    encoding_supported: bool,
    raw_message_size: Option<usize>,
}
#[derive(Clone)]
struct Transfer {
    principal: String,
    account: AccountKey,
    mailbox: String,
    uid: u32,
    validity: u32,
    part: Part,
    cache: Option<cache::Binding>,
    expires: Instant,
}
pub struct AttachmentStore {
    handles: Mutex<HashMap<String, Transfer>>,
    transfers: Arc<Semaphore>,
    staging: staging::Store,
}
impl AttachmentStore {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            staging: staging::Store::from_config(&config),
            handles: Mutex::new(HashMap::new()),
            transfers: Arc::new(Semaphore::new(MAX_TRANSFERS)),
        }
    }
    pub(super) fn stop_transfers(&self) -> bool {
        self.transfers.close();
        self.transfers.available_permits() == MAX_TRANSFERS
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
    #[cfg(test)]
    async fn lookup(&self, token: &str, principal: &str, account: &AccountKey) -> Option<Transfer> {
        self.lookup_principal(token, principal)
            .await
            .filter(|transfer| &transfer.account == account)
    }
    async fn lookup_principal(&self, token: &str, principal: &str) -> Option<Transfer> {
        let mut handles = self.handles.lock().await;
        handles.retain(|_, t| t.expires > Instant::now());
        handles
            .get(token)
            .filter(|t| t.principal == principal)
            .cloned()
    }
}

fn from_manifest(part: &parts::ManifestPart, hint: Option<(Option<usize>, &'static str)>) -> Part {
    let wire_size = part.encoded_size.map_or(0, |v| v as usize);
    let (decoded_size, size_accuracy) =
        hint.unwrap_or_else(|| match part.transfer_encoding.as_str() {
            "7bit" | "8bit" | "binary" => (Some(wire_size), "exact"),
            "base64" => (Some(wire_size / 4 * 3 + wire_size % 4 * 3 / 4), "estimate"),
            _ => (None, "unknown"),
        });
    Part {
        index: part.attachment_index,
        wire_size,
        content_type: part.content_type.clone(),
        filename: part
            .filename
            .as_ref()
            .map(|v| v.chars().take(255).collect()),
        attachment: part.is_attachment,
        decoded_size,
        size_accuracy,
        disposition: part.disposition.clone(),
        content_id: part.content_id.clone(),
        encoding_supported: part.downloadable,
        raw_message_size: None,
    }
}
fn entries(raw: &[u8]) -> Result<Vec<Part>> {
    let parsed = mime::parse(raw)?;
    let manifest = parts::from_raw(&parsed)?;
    Ok(manifest
        .manifest
        .parts
        .iter()
        .zip(manifest.entities)
        .map(|(metadata, entity)| from_manifest(metadata, Some(entity.attachment_size_hint())))
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
        metadata::ensure_account(a, &self.alias)?;
        if name == "prepare_attachment_download" && !self.attachments.staging.is_available() {
            return Err(MailError::attachment_storage_unavailable());
        }
        let mailbox = folder(a, InputLimits::Read)?;
        let uid = uid(a)?;
        let loaded = self.attachment_manifest(a).await?;
        let validity = loaded.validity;
        let diagnostics = loaded.diagnostics;
        let whole_size = loaded.whole_size;
        let entries = match loaded.source {
            parts::ManifestSource::Structured(manifest) => manifest
                .parts
                .iter()
                .map(|part| {
                    let mut value = from_manifest(part, None);
                    value.raw_message_size = Some(whole_size);
                    value
                })
                .collect(),
            parts::ManifestSource::Raw(raw) => {
                let entries = entries(&raw)?;
                if let Ok(part) = mime::parse(&raw) {
                    self.persist_raw(mailbox, Some(validity), uid, &raw, &part);
                }
                if let Some(permit) = self.content_capture(mailbox) {
                    self.capture_raw(mailbox, Some(validity), uid, raw, permit);
                }
                entries
            }
        };
        let reference = AttachmentReference {
            alias: &self.alias,
            account: &self.account,
            mailbox,
            uid,
            validity,
        };
        if name == "list_attachments" {
            let mut value =
                attachment_list(a, &entries, &reference, &self.config, |_, item| Ok(item))?;
            value["read_diagnostics"] = json!(diagnostics);
            return Ok(value);
        }
        let index = select_part(&entries, a, &reference)?;
        let part = entries[index].clone();
        if !part.encoding_supported {
            return Err(unsupported_encoding());
        }
        if part.size_accuracy == "exact"
            && part
                .decoded_size
                .is_some_and(|size| size > self.config.max_message)
        {
            return Err(MailError::limit_kind(LimitKind::DecodedAttachmentBytes));
        }
        let mut value = self
            .issue_attachment(principal, reference, part, None)
            .await?;
        value["read_diagnostics"] = json!(diagnostics);
        Ok(value)
    }

    async fn issue_attachment(
        &self,
        principal: &str,
        reference: AttachmentReference<'_>,
        part: Part,
        cache: Option<cache::Binding>,
    ) -> Result<Value> {
        let AttachmentReference {
            mailbox,
            uid,
            validity,
            ..
        } = reference;
        let index = part.index;
        let expires_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| MailError::backend())?
            .as_secs()
            .saturating_add(HANDLE_TTL.as_secs());
        let metadata = descriptor(&part, &reference, &self.config, Some(expires_at));
        let attachment_id = reference.id(index);
        let token = self
            .attachments
            .insert(Transfer {
                principal: principal.into(),
                account: self.account.clone(),
                mailbox: mailbox.into(),
                uid,
                validity,
                part,
                cache,
                expires: Instant::now() + HANDLE_TTL,
            })
            .await?;
        Ok(
            json!({"account_id":self.alias,"mailbox":mailbox,"uid":uid,"uid_validity":validity,"attachment_id":attachment_id,"attachment_index":index,"download_url":download_url(&self.config.resource,&token)?,"method":"GET","required_headers":{"Authorization":"Bearer <access_token>"},"expires_in_seconds":HANDLE_TTL.as_secs(),"delivery":metadata["delivery"],"metadata":metadata,"content_trust":"untrusted"}),
        )
    }
    #[cfg(test)]
    async fn fetch_attachment(&self, transfer: &Transfer) -> Result<Vec<u8>> {
        if transfer.account != self.account {
            return Err(MailError::account_unavailable());
        }
        let mut session = self.connect_read().await?;
        let state = session
            .examine(mailbox_name(&transfer.mailbox, InputLimits::Read)?)
            .await
            .map_err(map_read_imap_error)?;
        check_validity(
            &json!({"uid_validity":transfer.validity}),
            state.uid_validity,
        )?;
        let raw = self.raw_attachment(&mut session, transfer.uid).await?;
        let parsed = mime::parse(&raw)?;
        let manifest = parts::from_raw(&parsed)?;
        decode_part(
            manifest
                .entities
                .get(transfer.part.index)
                .ok_or_else(attachment_missing)?,
            self.config.max_message,
        )
    }
}

fn attachment_list(
    args: &Value,
    entries: &[Part],
    reference: &AttachmentReference<'_>,
    config: &Config,
    mut map: impl FnMut(&Part, Value) -> Result<Value>,
) -> Result<Value> {
    let offset = number(args, "attachment_offset", 0, 0, MAX_ATTACHMENT_INDEX as u64)? as usize;
    let mut metadata = Vec::new();
    let mut used = 1024;
    let mut next = None;
    for (index, part) in entries
        .iter()
        .enumerate()
        .skip(offset)
        .filter(|(_, part)| part.attachment)
    {
        let item = map(part, descriptor(part, reference, config, None))?;
        let size = serde_json::to_vec(&item)
            .map_err(|_| MailError::backend())?
            .len();
        if used + size > config.max_output / 2 && !metadata.is_empty() {
            next = Some(index);
            break;
        }
        used += size;
        metadata.push(item);
    }
    Ok(
        json!({"account_id":reference.alias,"mailbox":reference.mailbox,"uid":reference.uid,"uid_validity":reference.validity,
        "attachments":metadata,"next_attachment_offset":next,"content_trust":"untrusted"}),
    )
}

pub(super) fn attachment_missing() -> MailError {
    MailError {
        code: "ATTACHMENT_NOT_FOUND",
        message: "The referenced attachment is no longer available.",
        retryable: false,
        details: None,
    }
}

pub(super) fn unsupported_encoding() -> MailError {
    MailError {
        code: "UNSUPPORTED_TRANSFER_ENCODING",
        message: "The attachment transfer encoding is unsupported.",
        retryable: false,
        details: None,
    }
}

pub(super) fn decode_part(part: &mime::Part<'_>, max: usize) -> Result<Vec<u8>> {
    if !part.attachment_encoding_supported() {
        return Err(unsupported_encoding());
    }
    part.body_raw(max).map_err(|error| {
        if error.code == "LIMIT_EXCEEDED" {
            error
        } else {
            MailError {
                code: "ATTACHMENT_DECODE_FAILED",
                message: "The attachment encoding could not be decoded completely.",
                retryable: false,
                details: None,
            }
        }
    })
}

pub(super) fn inline_download(
    raw: &[u8],
    args: &Value,
    account: &AccountKey,
    mailbox: &str,
    uid: u32,
    validity: u32,
    max: usize,
) -> Result<Value> {
    use base64::Engine;
    let entries = entries(raw)?;
    let reference = AttachmentReference {
        alias: args["account_id"].as_str().unwrap_or("default"),
        account,
        mailbox,
        uid,
        validity,
    };
    let index = select_part(&entries, args, &reference)?;
    let parsed = mime::parse(raw)?;
    let manifest = parts::from_raw(&parsed)?;
    let bytes = decode_part(
        manifest
            .entities
            .get(index)
            .ok_or_else(attachment_missing)?,
        max,
    )?;
    Ok(json!({
        "account_id":reference.alias, "mailbox":mailbox, "uid":uid, "uid_validity":validity,
        "attachment_id":reference.id(index), "attachment_index":index,
        "content_type":entries[index].content_type,"size":bytes.len(),
        "base64":base64::engine::general_purpose::STANDARD.encode(&bytes),
        "checksum":{"algorithm":"sha256","value":format!("{:x}",Sha256::digest(&bytes))}
    }))
}

/// Small fixed errors are shared with the attachment authentication boundary.
pub(crate) fn http_error(
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    phase: &'static str,
    action: &'static str,
    request_id: &str,
) -> Response {
    let mut response = (status, axum::Json(json!({"error":{"code":code,"message":message,"phase":phase,"action":action},"request_id":request_id}))).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response.headers_mut().insert(
        "x-content-type-options",
        header::HeaderValue::from_static("nosniff"),
    );
    if let Ok(value) = request_id.parse() {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

fn fetch_http_error(error: &MailError, request_id: &str, limit: usize) -> Response {
    match error.code {
        "TIMEOUT" => http_error(
            StatusCode::GATEWAY_TIMEOUT,
            "TIMEOUT",
            "The attachment download timed out.",
            "backend_fetch",
            "retry_read",
            request_id,
        ),
        "ATTACHMENT_STORAGE_UNAVAILABLE" => http_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "ATTACHMENT_STORAGE_UNAVAILABLE",
            "Attachment download storage is unavailable.",
            "staging",
            "contact_operator",
            request_id,
        ),
        "ACCOUNT_UNAVAILABLE" | "CACHE_UNAVAILABLE" | "CACHE_MISS" => handle_not_found(request_id),
        "CACHE_NEEDS_VERIFICATION" => http_error(
            StatusCode::CONFLICT,
            "CACHE_NEEDS_VERIFICATION",
            "The cached attachment needs live verification.",
            "cache_reference",
            "relist_attachments",
            request_id,
        ),
        "STORAGE_UNAVAILABLE" => http_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "STORAGE_UNAVAILABLE",
            "The cached attachment is unavailable.",
            "cache_read",
            "contact_operator",
            request_id,
        ),
        "BUSY" => {
            let mut response = http_error(
                StatusCode::TOO_MANY_REQUESTS,
                "BUSY",
                "Attachment download capacity is full.",
                "transfer_admission",
                "retry_with_backoff",
                request_id,
            );
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
            response
        }
        "STALE_UID" => http_error(
            StatusCode::CONFLICT,
            "STALE_UID",
            "The mailbox reference has changed.",
            "mailbox_reference",
            "refresh_message_reference",
            request_id,
        ),
        "NOT_FOUND" => http_error(
            StatusCode::NOT_FOUND,
            "MESSAGE_NOT_FOUND",
            "The referenced email is no longer available.",
            "message_fetch",
            "locate_message",
            request_id,
        ),
        "ATTACHMENT_NOT_FOUND" => http_error(
            StatusCode::NOT_FOUND,
            "ATTACHMENT_NOT_FOUND",
            "The referenced attachment is no longer available.",
            "attachment_decode",
            "locate_message",
            request_id,
        ),
        "UNSUPPORTED_TRANSFER_ENCODING" => http_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "UNSUPPORTED_TRANSFER_ENCODING",
            "The attachment transfer encoding is unsupported.",
            "attachment_decode",
            "choose_supported_attachment",
            request_id,
        ),
        "ATTACHMENT_DECODE_FAILED" => http_error(
            StatusCode::BAD_GATEWAY,
            "ATTACHMENT_DECODE_FAILED",
            "The attachment encoding could not be decoded completely.",
            "attachment_decode",
            "retry_read",
            request_id,
        ),
        "LIMIT_EXCEEDED" => {
            let kind = error
                .details
                .as_deref()
                .and_then(|details| details.limit_kind)
                .unwrap_or(LimitKind::DecodedAttachmentBytes);
            let phase = match kind {
                LimitKind::DecodedAttachmentBytes => "attachment_decode",
                LimitKind::PartFetchBytes => "part_fetch",
                LimitKind::ConnectionBytes => "connection_read",
                _ => "message_fetch",
            };
            let mut response = http_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "LIMIT_EXCEEDED",
                "The attachment exceeds a resource limit.",
                phase,
                "use_smaller_attachment",
                request_id,
            );
            // Use only bounded numeric/classification details, never backend messages.
            let mut body = json!({"error":{"code":"LIMIT_EXCEEDED","message":"The attachment exceeds a resource limit.","phase":phase,"action":"use_smaller_attachment","limit_kind":kind},"request_id":request_id});
            if let Some(limit) = error
                .details
                .as_deref()
                .and_then(|details| details.primary.as_ref())
                .and_then(|primary| primary.limit)
                .or_else(|| {
                    matches!(
                        kind,
                        LimitKind::DecodedAttachmentBytes | LimitKind::RawMessageBytes
                    )
                    .then_some(limit)
                })
            {
                body["error"]["limit"] = json!(limit);
            }
            *response.body_mut() = Body::from(serde_json::to_vec(&body).unwrap_or_default());
            response
        }
        _ => http_error(
            StatusCode::BAD_GATEWAY,
            "MAIL_BACKEND",
            "The mail server did not complete the download.",
            "backend_fetch",
            "retry_read",
            request_id,
        ),
    }
}
fn handle_not_found(request_id: &str) -> Response {
    http_error(
        StatusCode::NOT_FOUND,
        "ATTACHMENT_HANDLE_NOT_FOUND",
        "The download handle is unavailable.",
        "handle_lookup",
        "prepare_attachment_download",
        request_id,
    )
}
/// Must be mounted inside the same OIDC middleware as the MCP endpoint.
pub async fn download(
    State(runtime): State<Arc<MailRuntime>>,
    Extension(principal): Extension<Principal>,
    Path(token): Path<String>,
    request_id: Option<Extension<RequestId>>,
) -> Response {
    let request_id = request_id
        .map(|Extension(value)| value.0)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let Some(transfer) = runtime
        .attachments
        .lookup_principal(&token, &principal.0)
        .await
    else {
        return handle_not_found(&request_id);
    };
    let Ok(lease) = runtime.catalog.read_key(&principal.0, &transfer.account) else {
        return handle_not_found(&request_id);
    };
    if transfer.cache.is_some() {
        return Box::pin(cache::download(&runtime, lease, transfer, request_id)).await;
    }
    let config = lease.config.clone();
    let backend = runtime.clone();
    Box::pin(download_staged_selected(
        &runtime.attachments,
        &config,
        transfer,
        request_id,
        move |transfer, stage, deadline| async move {
            let service = backend.service_for(&lease.key)?;
            let activity = parts::fetch::FetchActivity::default();
            tokio::time::timeout_at(
                deadline,
                backend.run_read(
                    &lease,
                    Box::pin(parts::fetch::download(
                        service,
                        parts::fetch::PartReference {
                            mailbox: &transfer.mailbox,
                            uid: transfer.uid,
                            validity: transfer.validity,
                            index: transfer.part.index,
                        },
                        stage,
                        &activity,
                    )),
                ),
            )
            .await
            .map_err(|_| {
                if activity.is_network_active() {
                    service.backend_timeout()
                } else {
                    MailError::timeout()
                }
            })?
        },
    ))
    .await
}

async fn download_staged_selected<F, Fut>(
    store: &AttachmentStore,
    config: &Config,
    transfer: Transfer,
    request_id: String,
    fetch: F,
) -> Response
where
    F: FnOnce(Transfer, staging::Stage, tokio::time::Instant) -> Fut,
    Fut: std::future::Future<Output = Result<staging::Ready>>,
{
    if !store.staging.is_available() {
        return fetch_http_error(
            &MailError::attachment_storage_unavailable(),
            &request_id,
            config.max_message,
        );
    }
    let permit = match store.transfers.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return fetch_http_error(
                &MailError {
                    code: "BUSY",
                    message: "Attachment download capacity is full.",
                    retryable: true,
                    details: None,
                },
                &request_id,
                config.max_message,
            );
        }
    };
    let deadline = tokio::time::Instant::now() + config.timeout;
    let stage = match tokio::time::timeout_at(deadline, store.staging.begin(permit)).await {
        Ok(Ok(stage)) => stage,
        Ok(Err(error)) => {
            return fetch_http_error(
                &parts::fetch::storage_error(error),
                &request_id,
                config.max_message,
            );
        }
        Err(_) => {
            return http_error(
                StatusCode::GATEWAY_TIMEOUT,
                "TIMEOUT",
                "Attachment staging timed out.",
                "staging",
                "retry_read",
                &request_id,
            );
        }
    };
    let filename = metadata::safe_filename(transfer.part.filename.as_deref(), transfer.part.index);
    let ready = match fetch(transfer, stage, deadline).await {
        Ok(ready) => ready,
        Err(error) => return fetch_http_error(&error, &request_id, config.max_message),
    };
    let metadata = ready.metadata();
    let stream = futures::stream::try_unfold(ready, |ready| async move {
        let (ready, bytes) = ready
            .read_chunk()
            .await
            .map_err(|_| std::io::Error::other("Attachment file read failed"))?;
        Ok::<_, std::io::Error>(if bytes.is_empty() {
            None
        } else {
            Some((Bytes::from(bytes), ready))
        })
    });
    attachment_response(Body::from_stream(stream), metadata, &filename, &request_id)
}

fn attachment_response(
    body: Body,
    metadata: staging::ReadyMetadata,
    filename: &str,
    request_id: &str,
) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    let encoded = filename
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"._-".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect::<String>();
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"attachment.bin\"; filename*=UTF-8''{encoded}")
            .parse()
            .unwrap(),
    );
    headers.insert(header::CONTENT_LENGTH, metadata.decoded_size.into());
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    if let Ok(value) = request_id.parse() {
        headers.insert("x-request-id", value);
    }
    let digest = metadata
        .sha256
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    headers.insert("x-content-sha256", digest.parse().unwrap());
    use base64::Engine;
    headers.insert(
        "content-digest",
        format!(
            "sha-256=:{}:",
            base64::engine::general_purpose::STANDARD.encode(metadata.sha256)
        )
        .parse()
        .unwrap(),
    );
    response
}
#[cfg(test)]
async fn download_with<F, Fut>(
    service: Arc<MailService>,
    principal: Principal,
    token: String,
    request_id: String,
    fetch: F,
) -> Response
where
    F: FnOnce(Transfer) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>>>,
{
    let Some(transfer) = service
        .attachments
        .lookup(&token, &principal.0, &service.account)
        .await
    else {
        return handle_not_found(&request_id);
    };
    let timeout = service.config.timeout;
    download_selected(
        &service.attachments,
        &service.config,
        transfer,
        request_id,
        move |transfer| async move {
            tokio::time::timeout(timeout, fetch(transfer))
                .await
                .map_err(|_| MailError::timeout())?
        },
    )
    .await
}
#[cfg(test)]
async fn download_selected<F, Fut>(
    store: &AttachmentStore,
    config: &Config,
    transfer: Transfer,
    request_id: String,
    fetch: F,
) -> Response
where
    F: FnOnce(Transfer) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>>>,
{
    let Ok(permit) = store.transfers.clone().try_acquire_owned() else {
        let mut response = http_error(
            StatusCode::TOO_MANY_REQUESTS,
            "BUSY",
            "Attachment download capacity is full.",
            "transfer_admission",
            "retry_with_backoff",
            &request_id,
        );
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
        return response;
    };
    let filename = metadata::safe_filename(transfer.part.filename.as_deref(), transfer.part.index);
    let bytes = match fetch(transfer).await {
        Ok(bytes) => bytes,
        Err(error) => return fetch_http_error(&error, &request_id, config.max_message),
    };
    if bytes.len() > config.max_message {
        return fetch_http_error(
            &MailError::limit_kind(LimitKind::DecodedAttachmentBytes),
            &request_id,
            config.max_message,
        );
    }
    let len = bytes.len();
    use base64::Engine;
    let digest = Sha256::digest(&bytes);
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
    if let Ok(value) = request_id.parse() {
        headers.insert("x-request-id", value);
    }
    headers.insert("x-content-sha256", format!("{digest:x}").parse().unwrap());
    headers.insert(
        "content-digest",
        format!(
            "sha-256=:{}:",
            base64::engine::general_purpose::STANDARD.encode(digest)
        )
        .parse()
        .unwrap(),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn download_legacy(
        State(service): State<Arc<MailService>>,
        Extension(principal): Extension<Principal>,
        Path(token): Path<String>,
    ) -> Response {
        let backend = service.clone();
        download_with(
            service,
            principal,
            token,
            uuid::Uuid::new_v4().to_string(),
            move |transfer| async move { backend.fetch_attachment(&transfer).await },
        )
        .await
    }
    fn transfer(principal: &str, expires: Instant, account: &AccountKey) -> Transfer {
        Transfer {
            principal: principal.into(),
            account: account.clone(),
            mailbox: "INBOX".into(),
            uid: 1,
            validity: 1,
            expires,
            cache: None,
            part: Part {
                index: 1,
                wire_size: 4,
                content_type: "application/pdf".into(),
                filename: None,
                attachment: true,
                ..Default::default()
            },
        }
    }
    #[tokio::test]
    async fn handles_are_principal_bound_expiring_and_capacity_bounded() {
        let store = AttachmentStore::new(crate::config::fixture());
        let account = AccountKey::synthetic();
        let token = store
            .insert(transfer("owner", Instant::now() + HANDLE_TTL, &account))
            .await
            .unwrap();
        assert!(store.lookup(&token, "other", &account).await.is_none());
        assert!(store.lookup(&token, "owner", &account).await.is_some());
        assert!(
            store
                .lookup(&token, "owner", &AccountKey::synthetic())
                .await
                .is_none()
        );
        assert!(store.lookup("bad", "owner", &account).await.is_none());
        store.handles.lock().await.get_mut(&token).unwrap().expires =
            Instant::now() - Duration::from_secs(1);
        assert!(store.lookup(&token, "owner", &account).await.is_none());
        for _ in 0..MAX_HANDLES {
            store
                .insert(transfer("owner", Instant::now() + HANDLE_TTL, &account))
                .await
                .unwrap();
        }
        assert!(
            store
                .insert(transfer("owner", Instant::now() + HANDLE_TTL, &account))
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
            .insert(transfer(
                "owner",
                Instant::now() + HANDLE_TTL,
                &service.account,
            ))
            .await
            .unwrap();
        let route = Router::new()
            .route("/mcp/attachments/{token}", get(download_legacy))
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
                            download_with(
                                service,
                                principal,
                                token,
                                uuid::Uuid::new_v4().to_string(),
                                |_| async move { Ok(payload) },
                            )
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
        assert_eq!(
            service.attachments.transfers.available_permits(),
            MAX_TRANSFERS - 1
        );
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .as_ref(),
            expected
        );
        assert_eq!(
            service.attachments.transfers.available_permits(),
            MAX_TRANSFERS
        );
        let _permits = service
            .attachments
            .transfers
            .acquire_many(MAX_TRANSFERS as u32)
            .await
            .unwrap();
        let rejected = download_with(
            service.clone(),
            Principal("owner".into()),
            token,
            uuid::Uuid::new_v4().to_string(),
            |_| async { panic!("backend must not be called when at capacity") },
        )
        .await;
        assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
