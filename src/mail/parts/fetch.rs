//! Complete, checked target-part reads into provisional private attachment files.
use super::{
    mapping::ManifestPart,
    reader,
    transfer_decode::{DecodeError, Decoder, Encoding, OUTPUT_CHUNK, StepStatus},
    *,
};
use crate::mail::attachments::staging::{self, ExpectedFile, Ready, Stage, StageId};
use async_imap::types::CommandReadLimits;
use imap_proto::{AttributeValue, Response, SectionPath, Status};

const FETCH_BYTES: usize = 64 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    #[tokio::test]
    async fn connection_budget_classification_requires_the_typed_source() {
        let (mut stream, _) = BudgetStream::new(std::io::Cursor::new(b"abc"), 2);
        stream.read_exact(&mut [0; 2]).await.unwrap();
        assert_eq!(stream.read(&mut []).await.unwrap(), 0);
        let error = stream.read(&mut [0]).await.unwrap_err();
        let mapped = map_read_imap_error(error.into());
        assert_eq!(
            serde_json::to_value(mapped).unwrap()["details"]["limit_kind"],
            "connection_bytes"
        );
        let ordinary = map_read_imap_error(
            std::io::Error::other("IMAP connection byte budget exhausted").into(),
        );
        assert_eq!(ordinary.code, "MAIL_BACKEND");
    }
}

#[derive(Default)]
pub(in crate::mail) struct FetchActivity(std::sync::atomic::AtomicBool);
impl FetchActivity {
    pub(in crate::mail) fn is_network_active(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
    async fn network<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.0.store(true, std::sync::atomic::Ordering::Release);
        let result = future.await;
        self.0.store(false, std::sync::atomic::Ordering::Release);
        result
    }
}

pub(in crate::mail) struct CompletionEvidence {
    stage: StageId,
}
impl CompletionEvidence {
    pub(in crate::mail) fn matches(&self, stage: StageId) -> bool {
        self.stage == stage
    }
}
#[cfg(test)]
pub(in crate::mail) fn completion_for_test(stage: StageId) -> CompletionEvidence {
    CompletionEvidence { stage }
}

pub(in crate::mail) struct PartReference<'a> {
    pub mailbox: &'a str,
    pub uid: u32,
    pub validity: u32,
    pub index: usize,
}

pub(in crate::mail) fn storage_error(error: staging::Error) -> MailError {
    match error {
        staging::Error::SizeLimit => MailError::limit_kind(LimitKind::DecodedAttachmentBytes),
        staging::Error::Capacity => MailError {
            code: "BUSY",
            message: "Attachment download capacity is full.",
            retryable: true,
            details: None,
        },
        staging::Error::SizeMismatch
        | staging::Error::ChecksumMismatch
        | staging::Error::CompletionMismatch => MailError::backend(),
        _ => MailError::attachment_storage_unavailable(),
    }
}
fn decode_error(error: DecodeError) -> MailError {
    match error {
        DecodeError::DecodedLimit { limit } => MailError::limit_with(
            LimitKind::DecodedAttachmentBytes,
            limit,
            limit.saturating_add(1),
        ),
        DecodeError::PendingAllocation => MailError::limit_kind(LimitKind::DecodedAttachmentBytes),
        _ => MailError {
            code: "ATTACHMENT_DECODE_FAILED",
            message: "The attachment encoding could not be decoded completely.",
            retryable: false,
            details: None,
        },
    }
}

async fn chunk<S>(
    session: &mut async_imap::Session<S>,
    uid: u32,
    part: &ManifestPart,
    offset: usize,
    request: usize,
    expected: usize,
) -> Result<Vec<u8>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let selector = part.section.body_selector();
    if selector.is_empty() {
        return Err(MailError::backend());
    }
    let path = SectionPath::Part(part.section.numbers().to_vec(), None);
    let tag = session
        .run_command_with_limits(
            format!("UID FETCH {uid} (UID BODY.PEEK[{selector}]<{offset}.{request}>)"),
            CommandReadLimits::body_section_literal_bytes(request),
        )
        .await
        .map_err(map_read_imap_error)?;
    let mut found = None;
    let mut target = false;
    loop {
        let response = session
            .read_response()
            .await
            .map_err(|error| map_read_imap_error(error.into()))?
            .ok_or_else(MailError::backend)?;
        match response.parsed() {
            Response::Done {
                tag: received,
                status: Status::Ok,
                ..
            } if received == &tag => break,
            Response::Done { .. }
            | Response::Data {
                status: Status::Bye,
                ..
            } => return Err(MailError::backend()),
            Response::Fetch(_, attrs) => {
                let mut ids = attrs.iter().filter_map(|attr| {
                    if let AttributeValue::Uid(value) = attr {
                        Some(*value)
                    } else {
                        None
                    }
                });
                let first = ids.next();
                if let Some(second) = ids.next()
                    && (first == Some(uid) || second == uid || ids.any(|value| value == uid))
                {
                    return Err(MailError::backend());
                }
                if first != Some(uid) {
                    continue;
                }
                target = true;
                for attr in attrs {
                    if let AttributeValue::BodySection {
                        section,
                        index,
                        data,
                    } = attr
                    {
                        // A body for this target must be the exact requested slice.
                        if section.as_ref() != Some(&path) {
                            continue;
                        }
                        if *index != u32::try_from(offset).ok() || found.is_some() {
                            return Err(MailError::backend());
                        }
                        let data = data.as_deref().ok_or_else(MailError::backend)?;
                        if data.len() != expected {
                            return Err(MailError::backend());
                        }
                        found = Some(data.to_vec());
                    }
                }
            }
            _ => {}
        }
    }
    found.ok_or_else(|| {
        if target {
            MailError::backend()
        } else {
            MailError {
                code: "NOT_FOUND",
                message: "The referenced email is no longer available.",
                retryable: false,
                details: None,
            }
        }
    })
}

async fn decoded_chunk(
    mut stage: Stage,
    decoder: &mut Decoder,
    input: &[u8],
    eof: bool,
) -> Result<(Stage, bool)> {
    let mut consumed = 0;
    let mut output = vec![0; OUTPUT_CHUNK];
    loop {
        let step = decoder
            .step(&input[consumed..], &mut output, eof)
            .map_err(decode_error)?;
        consumed = consumed
            .checked_add(step.consumed)
            .filter(|n| *n <= input.len())
            .ok_or_else(MailError::backend)?;
        if step.produced != 0 {
            stage = stage
                .write_chunk(output[..step.produced].to_vec())
                .await
                .map_err(storage_error)?;
        }
        match step.status {
            StepStatus::Finished if consumed == input.len() && eof => return Ok((stage, true)),
            StepStatus::NeedInput if consumed == input.len() && !eof => return Ok((stage, false)),
            StepStatus::NeedOutput => {}
            _ => return Err(MailError::backend()),
        }
    }
}

pub(in crate::mail) async fn download(
    service: &MailService,
    reference: PartReference<'_>,
    mut stage: Stage,
    activity: &FetchActivity,
) -> Result<Ready> {
    let mut session = activity.network(service.connect_read()).await?;
    let state = activity
        .network(session.examine(mailbox_name(reference.mailbox, InputLimits::Read)?))
        .await
        .map_err(map_read_imap_error)?;
    check_validity(
        &json!({"uid_validity":reference.validity}),
        state.uid_validity,
    )?;
    let known_size = activity
        .network(reader::size(&mut session, reference.uid))
        .await
        .map_err(reader::FetchFailure::mail)?;
    let manifest = match activity
        .network(reader::manifest(&mut session, reference.uid))
        .await
    {
        Ok(manifest) => manifest,
        Err(reader::FetchFailure::Fallback(reason)) => {
            let diagnostics = reason.diagnostics();
            tracing::info!(event="attachment_raw_fallback", diagnostics=?serde_json::to_value(diagnostics).ok());
            let max = service
                .config
                .max_message
                .min(crate::config::DEFAULT_MAX_MESSAGE);
            if known_size > max {
                return Err(raw_limit(InputLimits::Read, max, known_size));
            }
            drop(session);
            let mut fresh = activity.network(service.connect_read()).await?;
            let state = activity
                .network(fresh.examine(mailbox_name(reference.mailbox, InputLimits::Read)?))
                .await
                .map_err(map_read_imap_error)?;
            check_validity(
                &json!({"uid_validity":reference.validity}),
                state.uid_validity,
            )?;
            let decoded = {
                let raw = activity
                    .network(service.raw_attachment(&mut fresh, reference.uid))
                    .await?;
                if raw.len() != known_size {
                    return Err(MailError::backend());
                }
                let parsed = mime::parse(&raw)?;
                let manifest = from_raw(&parsed)?;
                attachments::decode_part(
                    manifest
                        .entities
                        .get(reference.index)
                        .ok_or_else(attachments::attachment_missing)?,
                    max,
                )?
            };
            drop(fresh);
            for bytes in decoded.chunks(FETCH_BYTES) {
                stage = stage
                    .write_chunk(bytes.to_vec())
                    .await
                    .map_err(storage_error)?;
            }
            let proof = CompletionEvidence { stage: stage.id() };
            return stage
                .finish(
                    proof,
                    ExpectedFile {
                        decoded_size: Some(decoded.len()),
                        sha256: None,
                    },
                )
                .await
                .map_err(storage_error);
        }
        Err(error) => return Err(error.mail()),
    };
    let part = manifest
        .parts
        .get(reference.index)
        .ok_or_else(attachments::attachment_missing)?;
    let encoding = match part.transfer_encoding.as_str() {
        "7bit" | "8bit" | "binary" => Encoding::Identity,
        "base64" => Encoding::Base64,
        "quoted-printable" => Encoding::QuotedPrintable,
        _ => return Err(attachments::unsupported_encoding()),
    };
    let encoded_size = part.encoded_size.ok_or_else(MailError::backend)? as usize;
    let limit = service
        .config
        .max_message
        .min(crate::config::DEFAULT_MAX_MESSAGE);
    if encoding == Encoding::Identity && encoded_size > limit {
        return Err(MailError::limit_with(
            LimitKind::DecodedAttachmentBytes,
            limit,
            encoded_size,
        ));
    }
    let mut decoder = Decoder::new(encoding, limit);
    let mut offset = 0;
    while offset < encoded_size {
        let expected = (encoded_size - offset).min(FETCH_BYTES);
        let bytes = activity
            .network(chunk(
                &mut session,
                reference.uid,
                part,
                offset,
                FETCH_BYTES,
                expected,
            ))
            .await?;
        stage = decoded_chunk(stage, &mut decoder, &bytes, false).await?.0;
        offset = offset
            .checked_add(bytes.len())
            .ok_or_else(MailError::backend)?;
    }
    // Also validates empty attachments and guards against an understated manifest size.
    activity
        .network(chunk(&mut session, reference.uid, part, encoded_size, 1, 0))
        .await?;
    let (stage, finished) = decoded_chunk(stage, &mut decoder, &[], true).await?;
    if !finished {
        return Err(MailError::backend());
    }
    drop(session);
    let proof = CompletionEvidence { stage: stage.id() };
    stage
        .finish(
            proof,
            ExpectedFile {
                decoded_size: (encoding == Encoding::Identity).then_some(encoded_size),
                sha256: None,
            },
        )
        .await
        .map_err(storage_error)
}
