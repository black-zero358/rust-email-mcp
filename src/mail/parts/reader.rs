//! Completed-command metadata reads; fallback owns a fresh connection once.
use super::{
    mapping::{self, HeaderEvidence, HeaderRequest, ManifestOutcome},
    *,
};
use async_imap::{error::Error as ImapError, types::ResponseData};
use imap_proto::{AttributeValue, MessageSection, Response, SectionPath, Status};
use serde::Serialize;

const HEADER_BYTES: usize = 128 * 1024;
const TOTAL_HEADER_BYTES: usize = 1024 * 1024;
const SECTIONS_PER_COMMAND: usize = 16;
const HEADER_COMMANDS: usize = 64;

#[derive(Clone, Copy, Serialize)]
pub(in crate::mail) struct ManifestDiagnostics {
    source: &'static str,
    fallback_reason: Option<&'static str>,
    limit_kind: Option<&'static str>,
}
#[derive(Clone, Copy)]
pub(super) enum Fallback {
    StructureLimit,
    StructureRejected,
    MappingUnverified,
    MetadataConflict,
    WholeMultipart,
    HeaderBudget,
}
impl Fallback {
    pub(super) fn diagnostics(self) -> ManifestDiagnostics {
        ManifestDiagnostics {
            source: "raw",
            fallback_reason: Some(match self {
                Self::StructureLimit => "structure_limit",
                Self::StructureRejected => "structure_rejected",
                Self::MappingUnverified => "mapping_unverified",
                Self::MetadataConflict => "metadata_conflict",
                Self::WholeMultipart => "whole_multipart_attachment",
                Self::HeaderBudget => "header_budget",
            }),
            limit_kind: matches!(self, Self::StructureLimit).then_some("mime_structure"),
        }
    }
}
pub(in crate::mail) enum ManifestSource {
    Structured(PartManifest),
    Raw(Vec<u8>),
}
pub(in crate::mail) struct LoadedManifest {
    pub source: ManifestSource,
    pub validity: u32,
    pub whole_size: usize,
    pub diagnostics: ManifestDiagnostics,
}

pub(in crate::mail) fn map_read_imap_error(error: ImapError) -> MailError {
    if async_imap::error::body_structure_limit(&error).is_some() {
        MailError::limit_kind(LimitKind::MimeStructure)
    } else if let Some(limit) = async_imap::error::body_section_literal_limit(&error) {
        MailError::limit_with(LimitKind::PartFetchBytes, limit.limit, limit.actual)
    } else if matches!(&error, ImapError::Io(error) if error.get_ref().is_some_and(|cause| cause.is::<ConnectionBudgetExceeded>()))
    {
        MailError::limit_kind(LimitKind::ConnectionBytes)
    } else {
        MailError::backend()
    }
}

pub(super) enum FetchFailure {
    Imap(ImapError),
    Backend,
    Missing,
    Fallback(Fallback),
}
impl FetchFailure {
    pub(super) fn mail(self) -> MailError {
        match self {
            Self::Imap(error) => map_read_imap_error(error),
            Self::Missing => not_found(),
            Self::Fallback(Fallback::StructureLimit) => {
                MailError::limit_kind(LimitKind::MimeStructure)
            }
            Self::Fallback(Fallback::HeaderBudget) => MailError::limit_kind(LimitKind::HeaderBytes),
            _ => MailError::backend(),
        }
    }
    fn recoverable(self, structure_command: bool) -> Self {
        match self {
            Self::Imap(error) if async_imap::error::body_structure_limit(&error).is_some() => {
                Self::Fallback(Fallback::StructureLimit)
            }
            Self::Imap(ImapError::No(_) | ImapError::Bad(_)) if structure_command => {
                Self::Fallback(Fallback::StructureRejected)
            }
            other => other,
        }
    }
}
fn not_found() -> MailError {
    MailError {
        code: "NOT_FOUND",
        message: "The referenced email is no longer available.",
        retryable: false,
        details: None,
    }
}

/// Consume through the matching OK. ResponseData owners are retained only when needed.
async fn command<S>(
    session: &mut async_imap::Session<S>,
    query: String,
    mut consume: impl FnMut(ResponseData) -> std::result::Result<(), FetchFailure>,
) -> std::result::Result<(), FetchFailure>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let tag = session
        .run_command(query)
        .await
        .map_err(FetchFailure::Imap)?;
    loop {
        let response = session
            .read_response()
            .await
            .map_err(|error| FetchFailure::Imap(error.into()))?
            .ok_or(FetchFailure::Backend)?;
        match response.parsed() {
            Response::Done {
                tag: received,
                status,
                ..
            } if received == &tag => {
                return match status {
                    Status::Ok => Ok(()),
                    Status::No => Err(FetchFailure::Imap(ImapError::No(
                        "Metadata command rejected".into(),
                    ))),
                    Status::Bad => Err(FetchFailure::Imap(ImapError::Bad(
                        "Metadata command rejected".into(),
                    ))),
                    _ => Err(FetchFailure::Backend),
                };
            }
            Response::Done { .. }
            | Response::Data {
                status: Status::Bye,
                ..
            } => return Err(FetchFailure::Backend),
            _ => consume(response)?,
        }
    }
}

fn matches_uid(attrs: &[AttributeValue<'_>], uid: u32) -> std::result::Result<bool, FetchFailure> {
    let mut ids = attrs.iter().filter_map(|attr| {
        if let AttributeValue::Uid(uid) = attr {
            Some(*uid)
        } else {
            None
        }
    });
    let first = ids.next();
    if let Some(second) = ids.next()
        && (first == Some(uid) || second == uid || ids.any(|value| value == uid))
    {
        return Err(FetchFailure::Backend);
    }
    Ok(first == Some(uid))
}

pub(super) async fn size<S>(
    session: &mut async_imap::Session<S>,
    uid: u32,
) -> std::result::Result<usize, FetchFailure>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let mut found = None;
    let mut target = false;
    command(
        session,
        format!("UID FETCH {uid} (UID RFC822.SIZE)"),
        |response| {
            if let Response::Fetch(_, attrs) = response.parsed()
                && matches_uid(attrs, uid)?
            {
                target = true;
                for attr in attrs {
                    if let AttributeValue::Rfc822Size(value) = attr
                        && found.replace(*value as usize).is_some()
                    {
                        return Err(FetchFailure::Backend);
                    }
                }
            }
            Ok(())
        },
    )
    .await?;
    found.ok_or(if target {
        FetchFailure::Backend
    } else {
        FetchFailure::Missing
    })
}

async fn structure<S>(
    session: &mut async_imap::Session<S>,
    uid: u32,
) -> std::result::Result<ResponseData, FetchFailure>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let mut found = None;
    let mut target = false;
    command(
        session,
        format!("UID FETCH {uid} (UID BODYSTRUCTURE)"),
        |response| {
            if let Response::Fetch(_, attrs) = response.parsed()
                && matches_uid(attrs, uid)?
            {
                target = true;
                let count = attrs
                    .iter()
                    .filter(|attr| matches!(attr, AttributeValue::BodyStructure(_)))
                    .count();
                if count > 1 || count == 1 && found.is_some() {
                    return Err(FetchFailure::Backend);
                }
                if count == 1 {
                    found = Some(response);
                }
            }
            Ok(())
        },
    )
    .await
    .map_err(|error| error.recoverable(true))?;
    found.ok_or(if target {
        FetchFailure::Backend
    } else {
        FetchFailure::Missing
    })
}

fn header_path(request: &HeaderRequest) -> SectionPath {
    if request.root_headers {
        SectionPath::Full(MessageSection::Header)
    } else {
        SectionPath::Part(
            request.section.numbers().to_vec(),
            Some(MessageSection::Mime),
        )
    }
}
async fn headers<S>(
    session: &mut async_imap::Session<S>,
    uid: u32,
    requests: &[HeaderRequest],
    used: &mut usize,
) -> std::result::Result<Vec<HeaderEvidence>, FetchFailure>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let paths = requests.iter().map(header_path).collect::<Vec<_>>();
    let selectors = requests
        .iter()
        .map(|request| {
            let section = if request.root_headers {
                "HEADER".into()
            } else {
                format!("{}.MIME", request.section.body_selector())
            };
            format!("BODY.PEEK[{section}]<0.{}>", HEADER_BYTES + 1)
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mut received: Vec<Option<Vec<u8>>> = vec![None; requests.len()];
    let mut target = false;
    command(
        session,
        format!("UID FETCH {uid} (UID {selectors})"),
        |response| {
            if let Response::Fetch(_, attrs) = response.parsed()
                && matches_uid(attrs, uid)?
            {
                target = true;
                for attr in attrs {
                    if let AttributeValue::BodySection {
                        section: Some(path),
                        index,
                        data,
                    } = attr
                        && let Some(position) = paths.iter().position(|expected| expected == path)
                    {
                        if *index != Some(0) || received[position].is_some() {
                            return Err(FetchFailure::Backend);
                        }
                        let data = data.as_deref().ok_or(FetchFailure::Backend)?;
                        let next = used
                            .checked_add(data.len())
                            .filter(|n| *n <= TOTAL_HEADER_BYTES)
                            .ok_or(FetchFailure::Fallback(Fallback::HeaderBudget))?;
                        if data.len() > HEADER_BYTES {
                            return Err(FetchFailure::Fallback(Fallback::HeaderBudget));
                        }
                        *used = next;
                        received[position] = Some(data.to_vec());
                    }
                }
            }
            Ok(())
        },
    )
    .await
    .map_err(|error| error.recoverable(false))?;
    requests
        .iter()
        .zip(received)
        .map(|(request, bytes)| {
            Ok(HeaderEvidence {
                section: request.section.clone(),
                bytes: bytes.ok_or(if target {
                    FetchFailure::Backend
                } else {
                    FetchFailure::Missing
                })?,
            })
        })
        .collect()
}

pub(super) async fn manifest<S>(
    session: &mut async_imap::Session<S>,
    uid: u32,
) -> std::result::Result<PartManifest, FetchFailure>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let response = structure(session, uid).await?;
    let Response::Fetch(_, attrs) = response.parsed() else {
        return Err(FetchFailure::Backend);
    };
    let root = attrs
        .iter()
        .find_map(|attr| {
            if let AttributeValue::BodyStructure(node) = attr {
                Some(node)
            } else {
                None
            }
        })
        .ok_or(FetchFailure::Backend)?;
    let mut evidence = Vec::new();
    let mut used = 0;
    let mut commands = 0;
    loop {
        match mapping::map(root, &evidence).map_err(|error| {
            FetchFailure::Fallback(match error {
                mapping::MapError::StructureLimit => Fallback::StructureLimit,
                mapping::MapError::HeaderLimit => Fallback::HeaderBudget,
            })
        })? {
            ManifestOutcome::Ready(manifest) => return Ok(manifest),
            ManifestOutcome::RawFallback { reason } => {
                return Err(FetchFailure::Fallback(match reason {
                    "bodystructure_header_conflict" | "duplicate_mime_metadata" => {
                        Fallback::MetadataConflict
                    }
                    "whole_multipart_attachment" => Fallback::WholeMultipart,
                    _ => Fallback::MappingUnverified,
                }));
            }
            ManifestOutcome::NeedsHeaders(requests) => {
                for batch in requests.chunks(SECTIONS_PER_COMMAND) {
                    if commands >= HEADER_COMMANDS {
                        return Err(FetchFailure::Fallback(Fallback::HeaderBudget));
                    }
                    commands += 1;
                    evidence.extend(headers(session, uid, batch, &mut used).await?);
                }
            }
        }
    }
}

impl MailService {
    pub(in crate::mail) async fn attachment_manifest(
        &self,
        args: &Value,
    ) -> Result<LoadedManifest> {
        let mailbox = folder(args, InputLimits::Read)?;
        let uid = uid(args)?;
        let mut session = self.connect_read().await?;
        let state = session
            .examine(mailbox_name(mailbox, InputLimits::Read)?)
            .await
            .map_err(map_read_imap_error)?;
        check_validity(args, state.uid_validity)?;
        let validity = state
            .uid_validity
            .filter(|v| *v > 0)
            .ok_or_else(MailError::backend)?;
        // A separately completed size command survives a later parser failure.
        let known_size = size(&mut session, uid).await.map_err(FetchFailure::mail)?;
        let max = self
            .config
            .max_message
            .min(crate::config::DEFAULT_MAX_MESSAGE);
        match manifest(&mut session, uid).await {
            Ok(manifest) => Ok(LoadedManifest {
                source: ManifestSource::Structured(manifest),
                validity,
                whole_size: known_size,
                diagnostics: ManifestDiagnostics {
                    source: "bodystructure",
                    fallback_reason: None,
                    limit_kind: None,
                },
            }),
            Err(FetchFailure::Fallback(reason)) => {
                if known_size > max {
                    return Err(raw_limit(InputLimits::Read, max, known_size));
                }
                let diagnostics = reason.diagnostics();
                tracing::info!(
                    event = "mime_manifest_fallback",
                    reason = diagnostics.fallback_reason,
                    limit_kind = diagnostics.limit_kind
                );
                drop(session);
                let mut fresh = self.connect_read().await?;
                let state = fresh
                    .examine(mailbox_name(mailbox, InputLimits::Read)?)
                    .await
                    .map_err(map_read_imap_error)?;
                check_validity(&json!({"uid_validity":validity}), state.uid_validity)?;
                let raw = self.raw_attachment(&mut fresh, uid).await?;
                if raw.len() != known_size {
                    return Err(MailError::backend());
                }
                Ok(LoadedManifest {
                    source: ManifestSource::Raw(raw),
                    validity,
                    whole_size: known_size,
                    diagnostics,
                })
            }
            Err(error) => Err(error.mail()),
        }
    }
}

#[cfg(test)]
mod tests;
