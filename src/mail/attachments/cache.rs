//! Source-bound attachment handles and local, byte-exact delivery.
use super::*;
use crate::{
    accounts::AccountLease,
    cache::{
        Access, ArtifactKey, ArtifactState, BlobId, BlobLease, CacheError, DirtyStamp, Omission,
    },
    mail::cache_read::{
        CachedReader, blob_bytes, cache_context, cache_error, cache_miss, manifest,
    },
};
use std::io::{Cursor, Read};

#[derive(Clone)]
pub(super) struct Binding {
    stamp: DirtyStamp,
    attachment: ArtifactKey,
    blob: BlobId,
    derived: bool,
    info: Option<crate::cache::BlobInfo>,
}

fn attachment_key(index: usize) -> ArtifactKey {
    ArtifactKey::Attachment {
        original_index: index as u32,
        mime_version: 1,
    }
}

fn cached_entries(payload: &[u8], whole_size: Option<u32>) -> Result<Vec<Part>> {
    let value = manifest(payload)?;
    value["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| {
            let encoding = part["transfer_encoding"].as_str().ok_or_else(cache_miss)?;
            let size = part["encoded_size"]
                .as_u64()
                .and_then(|size| usize::try_from(size).ok());
            let (decoded_size, size_accuracy) = match (encoding, size) {
                ("7bit" | "8bit" | "binary", Some(size)) => (Some(size), "exact"),
                ("base64", Some(size)) => (Some(size / 4 * 3 + size % 4 * 3 / 4), "estimate"),
                _ => (None, "unknown"),
            };
            Ok(Part {
                index: part["attachment_index"].as_u64().ok_or_else(cache_miss)? as usize,
                wire_size: size.unwrap_or_default(),
                content_type: part["content_type"].as_str().ok_or_else(cache_miss)?.into(),
                filename: part["filename"]
                    .as_str()
                    .map(|value| value.chars().take(255).collect()),
                attachment: part["is_attachment"].as_bool().ok_or_else(cache_miss)?,
                decoded_size,
                size_accuracy,
                disposition: part["disposition"].as_str().map(str::to_owned),
                content_id: part["content_id"].as_str().map(str::to_owned),
                encoding_supported: part["downloadable"].as_bool().ok_or_else(cache_miss)?,
                raw_message_size: whole_size.map(|size| size as usize),
            })
        })
        .collect()
}

fn bind(
    reader: &CachedReader,
    reference: crate::cache::Ref<'_>,
    part: &Part,
    stamp: DirtyStamp,
) -> Result<Binding> {
    if !part.encoding_supported {
        return Err(unsupported_encoding());
    }
    let attachment = attachment_key(part.index);
    let (blob, derived, info) = match local_state(reader, reference, part)? {
        ArtifactState::Materialized(info) => (info.id, false, Some(info)),
        ArtifactState::Derivable { raw } => (raw, true, None),
        ArtifactState::Missing => return Err(cache_miss()),
        ArtifactState::Omitted(Omission::Quota) => {
            return Err(MailError::limit_kind(LimitKind::CacheQuota));
        }
        ArtifactState::Omitted(_) => {
            return Err(MailError::limit_kind(LimitKind::DecodedAttachmentBytes));
        }
        ArtifactState::Unsupported(_) => return Err(unsupported_encoding()),
    };
    Ok(Binding {
        stamp,
        attachment,
        blob,
        derived,
        info,
    })
}

fn local_state(
    reader: &CachedReader,
    reference: crate::cache::Ref<'_>,
    part: &Part,
) -> Result<ArtifactState> {
    if !part.encoding_supported {
        return Ok(ArtifactState::Unsupported(Omission::UnsupportedEncoding));
    }
    let state = reader
        .cache
        .artifact_state(reference, attachment_key(part.index))
        .map_err(cache_error)?;
    if state == ArtifactState::Missing
        && let ArtifactState::Materialized(info) = reader
            .cache
            .artifact_state(reference, ArtifactKey::Raw)
            .map_err(cache_error)?
    {
        return Ok(ArtifactState::Derivable { raw: info.id });
    }
    Ok(state)
}

fn describe(mut value: Value, state: ArtifactState, part: &Part, config: &Config) -> Value {
    let (state, bytes, reason) = match state {
        ArtifactState::Materialized(info) => {
            value["decoded_size"] = json!(info.bytes);
            value["size_accuracy"] = json!("exact");
            let digest = info
                .sha256
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            value["checksum"] = json!({"algorithm":"sha256","value":digest});
            (
                "materialized",
                usize::try_from(info.bytes).ok(),
                (info.bytes > config.max_message as u64).then_some("decoded_attachment_bytes"),
            )
        }
        ArtifactState::Derivable { .. } => ("derivable_from_raw", part.decoded_size, None),
        ArtifactState::Missing => ("missing", None, Some("cache_miss")),
        ArtifactState::Omitted(_) => ("omitted", None, Some("cache_omitted")),
        ArtifactState::Unsupported(_) => {
            ("unsupported", None, Some("unsupported_transfer_encoding"))
        }
    };
    let mut modes = Vec::new();
    if reason.is_none() {
        if bytes.is_some_and(|size| size <= config.max_output / 4) {
            modes.push("inline_base64");
        }
        modes.push("authenticated_http");
    } else {
        value["delivery"]["next_action"] = json!("use_supported_attachment");
    }
    value["cache_state"] = json!(state);
    value["downloadable"] = json!(reason.is_none());
    value["unavailable_reason"] = json!(reason);
    value["delivery"]["available_modes"] = json!(modes);
    value
}

enum AttachmentRead {
    Materialized(BlobLease),
    Derived {
        raw: BlobLease,
        data: Cursor<Vec<u8>>,
    },
}

impl AttachmentRead {
    fn lease(&self) -> &BlobLease {
        match self {
            Self::Materialized(lease) => lease,
            Self::Derived { raw, .. } => raw,
        }
    }

    fn metadata(&self) -> Result<staging::ReadyMetadata> {
        Ok(match self {
            Self::Materialized(lease) => {
                let info = lease.info();
                staging::ReadyMetadata {
                    decoded_size: usize::try_from(info.bytes)
                        .map_err(|_| MailError::limit_kind(LimitKind::DecodedAttachmentBytes))?,
                    sha256: info.sha256,
                }
            }
            Self::Derived { data, .. } => staging::ReadyMetadata {
                decoded_size: data.get_ref().len(),
                sha256: Sha256::digest(data.get_ref()).into(),
            },
        })
    }

    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Materialized(lease) => lease.read(bytes),
            Self::Derived { data, .. } => data.read(bytes),
        }
    }
}

fn open(
    reader: &CachedReader,
    reference: crate::cache::Ref<'_>,
    binding: &Binding,
    index: usize,
    limit: usize,
) -> Result<AttachmentRead> {
    if !reader.persistent.offline_current(
        reference.account,
        reference.mailbox,
        reference.uid_validity,
        Some(reference.uid),
    ) {
        return Err(cache_error(CacheError::NeedsVerification));
    }
    let key = if binding.derived {
        ArtifactKey::Raw
    } else {
        binding.attachment
    };
    let mut lease = reader
        .cache
        .open_blob(reference, key, Access::Offline, Some(binding.blob))
        .map_err(cache_error)?;
    if lease.offline_stamp() != Some(&binding.stamp) {
        return Err(cache_error(CacheError::NeedsVerification));
    }
    let result = if binding.derived {
        let raw = blob_bytes(&mut lease, reader.config.max_message)?;
        let parsed = mime::parse(&raw)?;
        let manifest = parts::from_raw(&parsed)?;
        let entity = manifest
            .entities
            .get(index)
            .ok_or_else(attachment_missing)?;
        let bytes = decode_part(entity, limit)?;
        AttachmentRead::Derived {
            raw: lease,
            data: Cursor::new(bytes),
        }
    } else {
        if lease.info().bytes > limit as u64 {
            return Err(MailError::limit_kind(LimitKind::DecodedAttachmentBytes));
        }
        AttachmentRead::Materialized(lease)
    };
    result.lease().revalidate().map_err(cache_error)?;
    verify_binding(reader, reference, binding)?;
    Ok(result)
}

fn verify_binding(
    reader: &CachedReader,
    reference: crate::cache::Ref<'_>,
    binding: &Binding,
) -> Result<()> {
    let state = reader
        .cache
        .artifact_state(reference, binding.attachment)
        .map_err(cache_error)?;
    let same = match state {
        ArtifactState::Materialized(info) => !binding.derived && info.id == binding.blob,
        ArtifactState::Derivable { raw } => binding.derived && raw == binding.blob,
        ArtifactState::Missing => binding.derived,
        ArtifactState::Unsupported(_) | ArtifactState::Omitted(_) => false,
    };
    if same { Ok(()) } else { Err(cache_miss()) }
}

enum CachedResult {
    Output(Value),
    Prepare {
        mailbox: String,
        uid: u32,
        validity: u32,
        part: Box<Part>,
        binding: Box<Binding>,
        coverage: Value,
    },
}

impl MailService {
    pub(in crate::mail) async fn cached_attachment(
        &self,
        name: &str,
        args: &Value,
        principal: &str,
    ) -> Result<Value> {
        let (tool, args) = (name.to_owned(), args.clone());
        let result = self.cache_job(move |reader| {
            let mailbox = folder(&args, InputLimits::Read)?;
            let uid = uid(&args)?;
            let scope = reader.freeze(mailbox, &args)?;
            let reference = reader.reference(mailbox, scope.uid_validity, uid);
            let whole_size = reader.cache.cached_message(reference).map_err(cache_error)?.and_then(|row| row.size);
            let (parts, stamp) = reader.cache.cached_parts(reference).map_err(cache_error)?.ok_or_else(cache_miss)?;
            let parts = cached_entries(&parts, whole_size)?;
            let reference_id = AttachmentReference {
                alias:&reader.alias, account:&reader.account, mailbox, uid, validity:scope.uid_validity,
            };
            let coverage = cache_context(vec![reader.scope_info_with_layers(&scope)?]);
            let value = if tool == "list_attachments" {
                attachment_list(&args, &parts, &reference_id, &reader.config, |part, item| {
                    Ok(describe(item, local_state(&reader, reference, part)?, part, &reader.config))
                })?
            } else {
                let index = select_part(&parts, &args, &reference_id)?;
                let part = parts[index].clone();
                let binding = bind(&reader, reference, &part, stamp)?;
                if tool == "prepare_attachment_download" {
                    reader.verify(&scope, Some(uid))?;
                    return Ok(CachedResult::Prepare {
                        mailbox:mailbox.into(), uid, validity:scope.uid_validity, part:Box::new(part), binding:Box::new(binding), coverage,
                    });
                }
                let mut data = open(&reader, reference, &binding, index, reader.config.max_output / 4)?;
                let info = data.metadata()?;
                let mut bytes = vec![0; info.decoded_size];
                data.read_exact(&mut bytes).map_err(|_| cache_error(CacheError::Storage))?;
                data.lease().revalidate().map_err(cache_error)?;
                use base64::Engine;
                json!({
                    "account_id":reader.alias,"mailbox":mailbox,"uid":uid,"uid_validity":scope.uid_validity,
                    "attachment_id":reference_id.id(index),"attachment_index":index,"content_type":part.content_type,
                    "size":info.decoded_size,"base64":base64::engine::general_purpose::STANDARD.encode(&bytes),
                    "checksum":{"algorithm":"sha256","value":format!("{:x}",Sha256::digest(&bytes))}
                })
            };
            reader.verify(&scope, Some(uid))?;
            Ok(CachedResult::Output(with_context(value, coverage)))
        }).await?;
        match result {
            CachedResult::Output(value) => Ok(value),
            CachedResult::Prepare {
                mailbox,
                uid,
                validity,
                part,
                binding,
                coverage,
            } => {
                let reference = AttachmentReference {
                    alias: &self.alias,
                    account: &self.account,
                    mailbox: &mailbox,
                    uid,
                    validity,
                };
                let state = binding.info.map_or(
                    ArtifactState::Derivable { raw: binding.blob },
                    ArtifactState::Materialized,
                );
                let mut value = self
                    .issue_attachment(principal, reference, (*part).clone(), Some(*binding))
                    .await?;
                value["metadata"] = describe(value["metadata"].take(), state, &part, &self.config);
                value["delivery"] = value["metadata"]["delivery"].clone();
                Ok(with_context(value, coverage))
            }
        }
    }
}

impl Read for AttachmentRead {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        AttachmentRead::read(self, bytes)
    }
}

fn with_context(mut value: Value, context: Value) -> Value {
    value["read_source"] = json!("cache");
    value["cache"] = context;
    value
}

struct Stream {
    data: AttachmentRead,
    reader: CachedReader,
    transfer: Transfer,
    cancellation: tokio_util::sync::CancellationToken,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Stream {
    fn next(&mut self) -> std::io::Result<Vec<u8>> {
        let reference = self.reader.reference(
            &self.transfer.mailbox,
            self.transfer.validity,
            self.transfer.uid,
        );
        let allowed = || {
            self.transfer.expires > Instant::now()
                && !self.cancellation.is_cancelled()
                && self.reader.persistent.offline_current(
                    reference.account,
                    reference.mailbox,
                    reference.uid_validity,
                    Some(reference.uid),
                )
        };
        let denied = || std::io::Error::other("Attachment reference expired");
        let binding = self.transfer.cache.as_ref().ok_or_else(denied)?;
        if !allowed() {
            return Err(denied());
        }
        self.data.lease().revalidate().map_err(|_| denied())?;
        verify_binding(&self.reader, reference, binding).map_err(|_| denied())?;
        let mut bytes = vec![0; 64 * 1024];
        let count = self.data.read(&mut bytes)?;
        self.data.lease().revalidate().map_err(|_| denied())?;
        if !allowed() {
            return Err(denied());
        }
        bytes.truncate(count);
        Ok(bytes)
    }
}

pub(super) async fn download(
    runtime: &MailRuntime,
    lease: AccountLease,
    transfer: Transfer,
    request_id: String,
) -> Response {
    let Ok(service) = runtime.service_for(&lease.key) else {
        return handle_not_found(&request_id);
    };
    let Ok(permit) = runtime.attachments.transfers.clone().try_acquire_owned() else {
        return fetch_http_error(
            &super::runtime::admission_error(scheduler::AdmissionError::Capacity),
            &request_id,
            service.config.max_message,
        );
    };
    let filename = metadata::safe_filename(transfer.part.filename.as_deref(), transfer.part.index);
    let cancellation = lease.cancellation.clone();
    let opened = runtime
        .run_cached(
            &lease,
            service.cache_job(move |reader| {
                let binding = transfer.cache.as_ref().ok_or_else(cache_miss)?;
                let reference =
                    reader.reference(&transfer.mailbox, transfer.validity, transfer.uid);
                if transfer.expires <= Instant::now() || cancellation.is_cancelled() {
                    return Err(MailError::account_unavailable());
                }
                let data = open(
                    &reader,
                    reference,
                    binding,
                    transfer.part.index,
                    reader.config.max_message,
                )?;
                let metadata = data.metadata()?;
                Ok((
                    Stream {
                        data,
                        reader,
                        transfer,
                        cancellation,
                        _permit: permit,
                    },
                    metadata,
                ))
            }),
        )
        .await;
    let (stream, metadata) = match opened {
        Ok(value) => value,
        Err(error) => return fetch_http_error(&error, &request_id, service.config.max_message),
    };
    let stream = futures::stream::try_unfold(stream, |mut stream| async move {
        let (stream, bytes) = tokio::task::spawn_blocking(move || {
            let bytes = stream.next()?;
            Ok::<_, std::io::Error>((stream, bytes))
        })
        .await
        .map_err(|_| std::io::Error::other("Attachment read interrupted"))??;
        Ok::<_, std::io::Error>((!bytes.is_empty()).then(|| (Bytes::from(bytes), stream)))
    });
    attachment_response(Body::from_stream(stream), metadata, &filename, &request_id)
}
