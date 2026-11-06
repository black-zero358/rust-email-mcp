//! Completed-command metadata reads; fallback owns a fresh connection once.
use super::{
    mapping::{self, HeaderEvidence, HeaderRequest, ManifestOutcome},
    *,
};
use async_imap::{
    error::Error as ImapError,
    types::{CommandReadLimits, ResponseData},
};
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
impl ManifestDiagnostics {
    pub(super) fn structured() -> Self {
        Self {
            source: "bodystructure",
            fallback_reason: None,
            limit_kind: None,
        }
    }
}
#[derive(Clone, Copy)]
pub(in crate::mail) enum Fallback {
    StructureLimit,
    StructureRejected,
    MappingUnverified,
    MetadataConflict,
    WholeMultipart,
    HeaderBudget,
    CharsetUtf7,
    BodyEofMismatch,
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
                Self::CharsetUtf7 => "charset_utf7",
                Self::BodyEofMismatch => "body_eof_mismatch",
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

pub(in crate::mail) enum FetchFailure {
    Imap(ImapError),
    Backend,
    Missing,
    Fallback {
        reason: Fallback,
        /// Every metadata command completed with matching OK; local errors may retain it.
        synchronized: bool,
    },
}
impl FetchFailure {
    pub(in crate::mail) fn mail(self) -> MailError {
        match self {
            Self::Imap(error) => map_read_imap_error(error),
            Self::Missing => not_found(),
            Self::Fallback {
                reason: Fallback::StructureLimit,
                ..
            } => MailError::limit_kind(LimitKind::MimeStructure),
            Self::Fallback {
                reason: Fallback::HeaderBudget,
                ..
            } => MailError::limit_kind(LimitKind::HeaderBytes),
            _ => MailError::backend(),
        }
    }
    fn structure_failure(self) -> Self {
        match self {
            Self::Imap(error) if async_imap::error::body_structure_limit(&error).is_some() => {
                Self::Fallback {
                    reason: Fallback::StructureLimit,
                    synchronized: false,
                }
            }
            Self::Imap(ImapError::No(_) | ImapError::Bad(_)) => Self::Fallback {
                reason: Fallback::StructureRejected,
                synchronized: false,
            },
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
pub(in crate::mail) async fn command<S>(
    session: &mut async_imap::Session<S>,
    query: String,
    limits: Option<CommandReadLimits>,
    mut consume: impl FnMut(ResponseData) -> std::result::Result<(), FetchFailure>,
) -> std::result::Result<(), FetchFailure>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let tag = match limits {
        Some(limits) => session.run_command_with_limits(query, limits).await,
        None => session.run_command(query).await,
    }
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

pub(in crate::mail) async fn size<S>(
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
        None,
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
        None,
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
    .map_err(FetchFailure::structure_failure)?;
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
    let mut over_budget = false;
    let mut target = false;
    command(
        session,
        format!("UID FETCH {uid} (UID {selectors})"),
        Some(CommandReadLimits::body_section_literal_bytes(
            HEADER_BYTES + 1,
        )),
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
                        if over_budget
                            || data.len() > HEADER_BYTES
                            || *used + data.len() > TOTAL_HEADER_BYTES
                        {
                            over_budget = true;
                            // Mark the required attribute without retaining its bytes.
                            received[position] = Some(Vec::new());
                            continue;
                        }
                        *used += data.len();
                        received[position] = Some(data.to_vec());
                    }
                }
            }
            Ok(())
        },
    )
    .await?;
    if received.iter().any(Option::is_none) {
        return Err(if target {
            FetchFailure::Backend
        } else {
            FetchFailure::Missing
        });
    }
    // Drain through matching OK before classifying a header budget failure as
    // local. A rejected command or a missing attribute stays terminal.
    if over_budget {
        return Err(FetchFailure::Fallback {
            reason: Fallback::HeaderBudget,
            synchronized: true,
        });
    }
    requests
        .iter()
        .zip(received)
        .map(|(request, bytes)| {
            Ok(HeaderEvidence {
                section: request.section.clone(),
                bytes: bytes.ok_or(FetchFailure::Backend)?,
            })
        })
        .collect()
}

pub(in crate::mail) async fn manifest<S>(
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
        match mapping::map(root, &evidence).map_err(|error| FetchFailure::Fallback {
            reason: match error {
                mapping::MapError::StructureLimit => Fallback::StructureLimit,
                mapping::MapError::HeaderLimit => Fallback::HeaderBudget,
            },
            synchronized: true,
        })? {
            ManifestOutcome::Ready(manifest) => return Ok(manifest),
            ManifestOutcome::RawFallback { reason } => {
                return Err(FetchFailure::Fallback {
                    reason: match reason {
                        "bodystructure_header_conflict" | "duplicate_mime_metadata" => {
                            Fallback::MetadataConflict
                        }
                        "whole_multipart_attachment" => Fallback::WholeMultipart,
                        _ => Fallback::MappingUnverified,
                    },
                    synchronized: true,
                });
            }
            ManifestOutcome::NeedsHeaders(requests) => {
                for batch in requests.chunks(SECTIONS_PER_COMMAND) {
                    if commands >= HEADER_COMMANDS {
                        return Err(FetchFailure::Fallback {
                            reason: Fallback::HeaderBudget,
                            synchronized: true,
                        });
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
        self.observe_mailbox(mailbox, &state);
        check_validity(args, state.uid_validity)?;
        let validity = state
            .uid_validity
            .filter(|v| *v > 0)
            .ok_or_else(MailError::backend)?;
        // A separately completed size command survives a later parser failure.
        let observe =
            |error| self.observe_fetch_error(mailbox, Some(validity), state.uid_next, uid, error);
        let known_size = size(&mut session, uid)
            .await
            .map_err(|error| observe(error.mail()))?;
        let max = self
            .config
            .max_message
            .min(crate::config::DEFAULT_MAX_MESSAGE);
        match manifest(&mut session, uid).await {
            Ok(manifest) => {
                self.persist_structured_manifest(mailbox, Some(validity), uid, &manifest);
                Ok(LoadedManifest {
                    source: ManifestSource::Structured(manifest),
                    validity,
                    whole_size: known_size,
                    diagnostics: ManifestDiagnostics {
                        source: "bodystructure",
                        fallback_reason: None,
                        limit_kind: None,
                    },
                })
            }
            Err(FetchFailure::Fallback { reason, .. }) => {
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
                self.observe_mailbox(mailbox, &state);
                check_validity(&json!({"uid_validity":validity}), state.uid_validity)?;
                let raw = self
                    .raw_attachment(&mut fresh, uid)
                    .await
                    .map_err(|error| {
                        self.observe_fetch_error(
                            mailbox,
                            Some(validity),
                            state.uid_next,
                            uid,
                            error,
                        )
                    })?;
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
            Err(error) => Err(observe(error.mail())),
        }
    }
}
