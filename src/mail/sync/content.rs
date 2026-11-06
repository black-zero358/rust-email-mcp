//! One durable content step. Network reads are read-only and do not advance a scan.
use super::super::{persistence, runtime, scheduler::BackendPermit};
use super::scan::Selected;
use crate::{
    accounts::AccountKey,
    cache::{
        Access, ArtifactKey, ArtifactState, BlobId, BlobLease, BlobStage, Cache, CacheError,
        Omission, Ref as CacheRef,
        sync_state::{
            ContentLayer, ContentOutcome, ContentWork, SyncError, SyncReclaimCandidate,
            SyncReclaimPosition,
        },
    },
    error::{LimitKind, MailError},
    mail::parts::{
        self, body, cached_body, fetch, reader,
        transfer_decode::{DecodeError, Decoder, Encoding, OUTPUT_CHUNK, StepStatus},
    },
    mime,
};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    sync::Arc,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;

type ContentResult<T> = std::result::Result<T, ContentError>;

#[derive(Debug, thiserror::Error)]
pub(super) enum ContentError {
    #[error("Content step was cancelled.")]
    Cancelled,
    #[error("Content step no longer matches the selected scope.")]
    Stale,
    #[error("The selected UID was not returned.")]
    Missing,
    #[error(transparent)]
    Storage(#[from] CacheError),
    #[error(transparent)]
    Read(MailError),
    #[error("Content requires verified raw evidence.")]
    RequiresRaw,
    #[error("Content step has a terminal omission: {0:?}")]
    Terminal(Omission),
}

impl ContentError {
    fn command(error: MailError) -> Self {
        if error.code == "NOT_FOUND" {
            Self::Missing
        } else {
            Self::Read(error)
        }
    }

    fn decoded(error: MailError) -> Self {
        match error.code {
            "NOT_FOUND" => Self::Missing,
            "UNSUPPORTED_BODY" | "UNSUPPORTED_CHARSET" => {
                Self::Terminal(Omission::UnsupportedContent)
            }
            "LIMIT_EXCEEDED"
                if matches!(
                    error
                        .details
                        .as_deref()
                        .and_then(|details| details.limit_kind),
                    None | Some(
                        LimitKind::DecodedBodyBytes
                            | LimitKind::DecodedAttachmentBytes
                            | LimitKind::MimeStructure
                            | LimitKind::HeaderBytes
                    )
                ) =>
            {
                Self::Terminal(Omission::DecodedLimit)
            }
            _ => Self::Read(error),
        }
    }
}

pub(super) struct Request<'a> {
    pub account: &'a AccountKey,
    pub selected: &'a Selected,
    pub work: &'a ContentWork,
    pub max_message: usize,
}

#[derive(Clone)]
struct Target {
    account: AccountKey,
    mailbox: Box<str>,
    validity: u32,
    uid: u32,
}

impl Target {
    fn reference(&self) -> CacheRef<'_> {
        CacheRef {
            account: &self.account,
            mailbox: &self.mailbox,
            uid_validity: self.validity,
            uid: self.uid,
        }
    }
}

struct Job<'a> {
    cache: &'a Arc<Cache>,
    target: Target,
    cancellation: CancellationToken,
}

struct Staged {
    value: Option<BlobStage>,
    admission: Option<Arc<BackendPermit>>,
    runtime: tokio::runtime::Handle,
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(stage) = self.value.take() {
            let admission = self.admission.take();
            self.runtime.spawn_blocking(move || {
                let _admission = admission;
                drop(stage);
            });
        }
    }
}

impl Job<'_> {
    async fn local<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Arc<Cache>, CacheRef<'_>, &CancellationToken) -> ContentResult<T>
        + Send
        + 'static,
    ) -> ContentResult<T> {
        let cache = self.cache.clone();
        let target = self.target.clone();
        let cancellation = self.cancellation.clone();
        let admission = runtime::read_admission();
        let task = tokio::task::spawn_blocking(move || {
            let _admission = admission;
            if cancellation.is_cancelled() {
                return Err(ContentError::Cancelled);
            }
            operation(&cache, target.reference(), &cancellation)
        });
        self.wait(async { task.await.map_err(|_| CacheError::Storage)? })
            .await
    }

    async fn wait<T>(
        &self,
        future: impl std::future::Future<Output = ContentResult<T>>,
    ) -> ContentResult<T> {
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Err(ContentError::Cancelled),
            result = future => result,
        }
    }

    async fn stage(
        &self,
        key: ArtifactKey,
        expected: Option<usize>,
        limit: usize,
    ) -> ContentResult<Staged> {
        let runtime = tokio::runtime::Handle::current();
        let admission = runtime::read_admission();
        self.local(move |cache, reference, _| {
            Ok(Staged {
                value: Some(cache.begin_blob(
                    reference,
                    key,
                    expected.map(|size| size as u64),
                    limit as u64,
                )?),
                admission,
                runtime,
            })
        })
        .await
    }

    async fn append(&self, mut stage: Staged, bytes: Vec<u8>) -> ContentResult<Staged> {
        self.local(move |_, _, _| {
            stage
                .value
                .as_mut()
                .ok_or(ContentError::Stale)?
                .write_all(&bytes)
                .map_err(CacheError::from)?;
            Ok(stage)
        })
        .await
    }

    async fn publish(&self, mut stage: Staged) -> ContentResult<ContentOutcome> {
        self.local(move |_, _, cancellation| {
            let info = stage
                .value
                .take()
                .ok_or(ContentError::Stale)?
                .publish(&|| cancellation.is_cancelled())?;
            Ok(ContentOutcome::PresentBlob(info.id))
        })
        .await
    }

    async fn existing(
        &self,
        key: ArtifactKey,
        exact_len: Option<usize>,
    ) -> ContentResult<Option<BlobId>> {
        self.local(move |cache, reference, _| existing_artifact(cache, reference, key, exact_len))
            .await
    }
}

fn existing_artifact(
    cache: &Arc<Cache>,
    reference: CacheRef<'_>,
    key: ArtifactKey,
    exact_len: Option<usize>,
) -> ContentResult<Option<BlobId>> {
    let ArtifactState::Materialized(info) = cache.artifact_state(reference, key)? else {
        return Ok(None);
    };
    if exact_len.is_some_and(|size| info.bytes != size as u64) {
        return Err(ContentError::Read(MailError::backend()));
    }
    let lease = cache.open_blob(reference, key, Access::LiveVerified, Some(info.id))?;
    lease.revalidate()?;
    Ok(Some(info.id))
}

pub(super) async fn materialize<S>(
    cache: &Arc<Cache>,
    session: &mut async_imap::Session<S>,
    request: Request<'_>,
    cancellation: &CancellationToken,
) -> ContentResult<ContentOutcome>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let Request {
        account,
        selected,
        work,
        max_message,
    } = request;
    if !selected.matches_account(account) || !selected.matches_work(work) || max_message == 0 {
        return Err(ContentError::Stale);
    }
    let cancellation = cancellation.child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let job = Job {
        cache,
        target: Target {
            account: account.clone(),
            mailbox: selected.mailbox.clone(),
            validity: selected.uid_validity,
            uid: work.uid,
        },
        cancellation,
    };
    let limit = max_message.min(crate::config::DEFAULT_MAX_MESSAGE);
    let result = job.wait(step(&job, session, work.layer, limit)).await;
    match result {
        Err(ContentError::Missing) => Ok(ContentOutcome::MissingRemote),
        Err(ContentError::Terminal(reason)) => Ok(ContentOutcome::Terminal(reason)),
        Err(ContentError::RequiresRaw) if work.layer == ContentLayer::Manifest => {
            Ok(ContentOutcome::NeedsRaw)
        }
        result => result,
    }
}

fn artifact(layer: ContentLayer) -> Option<ArtifactKey> {
    match layer {
        ContentLayer::Raw => Some(ArtifactKey::Raw),
        ContentLayer::FullBody => Some(cached_body::KEY),
        ContentLayer::Attachment(index) => Some(ArtifactKey::Attachment {
            original_index: index,
            mime_version: 1,
        }),
        _ => None,
    }
}

/// Immutable local artifacts can complete a derived work item under its
/// existing discovery receipt. Remote presence has its own reconciliation pass.
pub(super) fn reuse(
    cache: &Arc<Cache>,
    reference: CacheRef<'_>,
    cancellation: &CancellationToken,
    layer: ContentLayer,
    limit: usize,
) -> ContentResult<Option<ContentOutcome>> {
    let result = match layer {
        ContentLayer::Facts | ContentLayer::Raw => return Ok(None),
        ContentLayer::BodyIndex => {
            if !matches!(
                cache.artifact_state(reference, cached_body::KEY)?,
                ArtifactState::Materialized(_)
            ) {
                return Ok(None);
            }
            index_body(cache, reference, cancellation).map(Some)
        }
        _ => {
            if let Some(key) = artifact(layer)
                && let Some(blob) = existing_artifact(cache, reference, key, None)?
            {
                return Ok(Some(ContentOutcome::PresentBlob(blob)));
            }
            let ArtifactState::Materialized(raw) =
                cache.artifact_state(reference, ArtifactKey::Raw)?
            else {
                return Ok(None);
            };
            derive(
                cache,
                reference,
                cancellation,
                layer,
                raw.bytes as usize,
                limit,
            )
        }
    };
    match result {
        Err(ContentError::Terminal(reason)) => Ok(Some(ContentOutcome::Terminal(reason))),
        result => result,
    }
}

async fn step<S>(
    job: &Job<'_>,
    session: &mut async_imap::Session<S>,
    layer: ContentLayer,
    limit: usize,
) -> ContentResult<ContentOutcome>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let uid = job.target.uid;
    let size = reader::size(session, uid)
        .await
        .map_err(|error| ContentError::command(error.mail()))?;
    if let Some(key) = artifact(layer) {
        if let Some(blob) = job
            .existing(key, (key == ArtifactKey::Raw).then_some(size))
            .await?
        {
            return Ok(ContentOutcome::PresentBlob(blob));
        }
        if key == ArtifactKey::Raw {
            return raw(job, session, size, limit).await;
        }
    }
    if matches!(layer, ContentLayer::BodyIndex) {
        return job.local(index_body).await;
    }
    if matches!(layer, ContentLayer::Facts) {
        return Err(ContentError::Stale);
    }
    if let Some(outcome) = job
        .local(move |cache, reference, cancellation| {
            derive(cache, reference, cancellation, layer, size, limit)
        })
        .await?
    {
        return Ok(outcome);
    }
    let mut manifest = match reader::manifest(session, uid).await {
        Ok(manifest) => manifest,
        Err(reader::FetchFailure::Fallback { .. }) if size <= limit => {
            return Err(ContentError::RequiresRaw);
        }
        Err(reader::FetchFailure::Fallback { .. }) => {
            return Err(ContentError::Terminal(Omission::UnsupportedContent));
        }
        Err(error) => return Err(ContentError::command(error.mail())),
    };
    match layer {
        ContentLayer::Manifest => {
            job.local(move |cache, reference, cancellation| {
                save_manifest(cache, reference, cancellation, &manifest)
            })
            .await
        }
        ContentLayer::FullBody => {
            let record = body::canonical_structured(session, uid, &mut manifest)
                .await
                .map_err(|fault| match fault {
                    body::StructuredFault::Command(error) => ContentError::command(error),
                    body::StructuredFault::Local(error) => ContentError::decoded(error),
                    body::StructuredFault::RawFallback(reader::Fallback::BodyEofMismatch) => {
                        ContentError::command(MailError::backend())
                    }
                    body::StructuredFault::RawFallback(_) => {
                        ContentError::Terminal(Omission::UnsupportedContent)
                    }
                })?;
            job.local(move |cache, reference, cancellation| {
                save_body(cache, reference, cancellation, &record)
            })
            .await
        }
        ContentLayer::Attachment(index) => {
            let part = attachment(&manifest, index)?;
            stream_attachment(job, session, part, index, limit).await
        }
        _ => Err(ContentError::Stale),
    }
}

async fn raw<S>(
    job: &Job<'_>,
    session: &mut async_imap::Session<S>,
    size: usize,
    limit: usize,
) -> ContentResult<ContentOutcome>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    if size > limit {
        return Err(ContentError::Terminal(Omission::RawLimit));
    }
    let uid = job.target.uid;
    let mut stage = job.stage(ArtifactKey::Raw, Some(size), limit).await?;
    for offset in (0..size).step_by(OUTPUT_CHUNK) {
        let count = (size - offset).min(OUTPUT_CHUNK);
        let bytes = fetch::raw_chunk(session, uid, offset, count, count)
            .await
            .map_err(ContentError::command)?;
        stage = job.append(stage, bytes).await?;
    }
    fetch::raw_chunk(session, uid, size, 1, 0)
        .await
        .map_err(ContentError::command)?;
    job.publish(stage).await
}

fn save_manifest(
    cache: &Cache,
    reference: CacheRef<'_>,
    cancellation: &CancellationToken,
    manifest: &parts::PartManifest,
) -> ContentResult<ContentOutcome> {
    let bytes = persistence::encode_manifest(manifest)
        .ok_or(ContentError::Terminal(Omission::DecodedLimit))?;
    let expected = manifest
        .parts
        .iter()
        .filter(|part| part.is_attachment)
        .map(|part| {
            (
                ArtifactKey::Attachment {
                    original_index: part.attachment_index as u32,
                    mime_version: 1,
                },
                if part.downloadable {
                    ArtifactState::Missing
                } else {
                    ArtifactState::Unsupported(Omission::UnsupportedEncoding)
                },
            )
        })
        .collect::<Vec<_>>();
    if cancellation.is_cancelled() {
        return Err(ContentError::Cancelled);
    }
    cache.remember_full_manifest(reference, &bytes, &expected)?;
    Ok(ContentOutcome::Present)
}

fn save_body(
    cache: &Arc<Cache>,
    reference: CacheRef<'_>,
    cancellation: &CancellationToken,
    record: &cached_body::Record,
) -> ContentResult<ContentOutcome> {
    let mut stage = cache.begin_blob(
        reference,
        cached_body::KEY,
        Some(record.bytes() as u64),
        cached_body::LIMIT as u64,
    )?;
    let metadata = record
        .encode(BodyWriter {
            stage: &mut stage,
            cancellation,
        })
        .map_err(|error| {
            if cancellation.is_cancelled() {
                ContentError::Cancelled
            } else {
                ContentError::Storage(error.into())
            }
        })?;
    let info = stage.publish_body(&metadata, &|| cancellation.is_cancelled())?;
    Ok(ContentOutcome::PresentBlob(info.id))
}

struct BodyWriter<'a, W> {
    stage: &'a mut W,
    cancellation: &'a CancellationToken,
}

impl<W: Write> Write for BodyWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(std::io::Error::other("Content step was cancelled."));
        }
        self.stage.write(&bytes[..bytes.len().min(OUTPUT_CHUNK)])
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stage.flush()
    }
}

fn index_body(
    cache: &Arc<Cache>,
    reference: CacheRef<'_>,
    cancellation: &CancellationToken,
) -> ContentResult<ContentOutcome> {
    let mut body = cache.open_blob(reference, cached_body::KEY, Access::LiveVerified, None)?;
    let id = body.info().id;
    let record = cached_body::Record::decode(&mut body)?;
    let displayed = record
        .display(crate::html::HtmlDisplay::Legacy)
        .map_err(ContentError::decoded)?;
    body.revalidate()?;
    cache.remember_full_body_index(reference, cached_body::KEY, id, &displayed.text, &|| {
        cancellation.is_cancelled()
    })?;
    Ok(ContentOutcome::PresentBlob(id))
}

fn derive(
    cache: &Arc<Cache>,
    reference: CacheRef<'_>,
    cancellation: &CancellationToken,
    layer: ContentLayer,
    size: usize,
    limit: usize,
) -> ContentResult<Option<ContentOutcome>> {
    let ArtifactState::Materialized(info) = cache.artifact_state(reference, ArtifactKey::Raw)?
    else {
        return Ok(None);
    };
    if info.bytes != size as u64 {
        return Err(ContentError::Read(MailError::backend()));
    }
    let mut raw = cache.open_blob(
        reference,
        ArtifactKey::Raw,
        Access::LiveVerified,
        Some(info.id),
    )?;
    let bytes = read_raw(&mut raw, size, cancellation)?;
    let parsed = mime::parse(&bytes).map_err(ContentError::decoded)?;
    let outcome = match layer {
        ContentLayer::Manifest => {
            let manifest = parts::from_raw(&parsed).map_err(ContentError::decoded)?;
            raw.revalidate()?;
            save_manifest(cache, reference, cancellation, &manifest.manifest)?
        }
        ContentLayer::FullBody => {
            let record = raw_body(&parsed)?;
            raw.revalidate()?;
            save_body(cache, reference, cancellation, &record)?
        }
        ContentLayer::Attachment(index) => {
            let manifest = parts::from_raw(&parsed).map_err(ContentError::decoded)?;
            let part = attachment(&manifest.manifest, index)?;
            let entity = manifest
                .entities
                .get(index as usize)
                .ok_or(ContentError::Stale)?;
            derive_attachment(
                cache,
                reference,
                cancellation,
                &raw,
                part,
                entity.encoded_body(),
                limit,
            )?
        }
        _ => return Err(ContentError::Stale),
    };
    Ok(Some(outcome))
}

fn raw_body(parsed: &mime::Part<'_>) -> ContentResult<cached_body::Record> {
    let mut record = cached_body::Record::default();
    let mut remaining = cached_body::LIMIT;
    parsed
        .decoded_display_leaves(&mut remaining, |text, html, reserve| {
            record.push(text, html, reserve);
            Ok(())
        })
        .map_err(ContentError::decoded)?;
    Ok(record)
}

fn read_raw(
    raw: &mut BlobLease,
    size: usize,
    cancellation: &CancellationToken,
) -> ContentResult<Vec<u8>> {
    if size > crate::config::DEFAULT_MAX_MESSAGE {
        return Err(ContentError::Terminal(Omission::RawLimit));
    }
    let mut bytes = vec![0; size];
    for block in bytes.chunks_mut(OUTPUT_CHUNK) {
        if cancellation.is_cancelled() {
            return Err(ContentError::Cancelled);
        }
        raw.read_exact(block).map_err(CacheError::from)?;
    }
    raw.revalidate()?;
    Ok(bytes)
}

fn attachment(manifest: &parts::PartManifest, index: u32) -> ContentResult<&parts::ManifestPart> {
    manifest
        .parts
        .get(index as usize)
        .filter(|part| part.is_attachment && part.attachment_index == index as usize)
        .ok_or(ContentError::Stale)
}

fn attachment_decoder(
    part: &parts::ManifestPart,
    limit: usize,
) -> ContentResult<(Decoder, Option<usize>)> {
    let encoding = Encoding::for_label(&part.transfer_encoding)
        .ok_or(ContentError::Terminal(Omission::UnsupportedEncoding))?;
    if !part.downloadable {
        return Err(ContentError::Terminal(Omission::UnsupportedEncoding));
    }
    let size = part.encoded_size.ok_or(ContentError::Stale)? as usize;
    let expected = (encoding == Encoding::Identity).then_some(size);
    if expected.is_some_and(|bytes| bytes > limit) {
        return Err(ContentError::Terminal(Omission::DecodedLimit));
    }
    Ok((Decoder::new(encoding, limit), expected))
}

fn decode_error(error: DecodeError) -> ContentError {
    ContentError::Terminal(match error {
        DecodeError::DecodedLimit { .. } | DecodeError::PendingAllocation => Omission::DecodedLimit,
        _ => Omission::UnsupportedContent,
    })
}

async fn stream_attachment<S>(
    job: &Job<'_>,
    session: &mut async_imap::Session<S>,
    part: &parts::ManifestPart,
    index: u32,
    limit: usize,
) -> ContentResult<ContentOutcome>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let (mut decoder, expected) = attachment_decoder(part, limit)?;
    let size = part.encoded_size.ok_or(ContentError::Stale)? as usize;
    let uid = job.target.uid;
    let key = ArtifactKey::Attachment {
        original_index: index,
        mime_version: 1,
    };
    let mut stage = job.stage(key, expected, limit).await?;
    for offset in (0..size).step_by(OUTPUT_CHUNK) {
        let count = (size - offset).min(OUTPUT_CHUNK);
        let bytes = fetch::attachment_chunk(session, uid, part, offset, count, count)
            .await
            .map_err(ContentError::command)?;
        stage = decode_to_stage(job, stage, &mut decoder, &bytes, false).await?;
    }
    fetch::attachment_chunk(session, uid, part, size, 1, 0)
        .await
        .map_err(ContentError::command)?;
    stage = decode_to_stage(job, stage, &mut decoder, &[], true).await?;
    job.publish(stage).await
}

async fn decode_to_stage(
    job: &Job<'_>,
    mut stage: Staged,
    decoder: &mut Decoder,
    mut input: &[u8],
    eof: bool,
) -> ContentResult<Staged> {
    loop {
        let mut output = vec![0; OUTPUT_CHUNK];
        let step = decoder
            .step(input, &mut output, eof)
            .map_err(decode_error)?;
        input = &input[step.consumed..];
        output.truncate(step.produced);
        if !output.is_empty() {
            stage = job.append(stage, output).await?;
        }
        match step.status {
            StepStatus::NeedOutput => {}
            StepStatus::NeedInput if !eof && input.is_empty() => return Ok(stage),
            StepStatus::Finished if eof && input.is_empty() => return Ok(stage),
            _ => return Err(ContentError::Stale),
        }
    }
}

fn derive_attachment(
    cache: &Arc<Cache>,
    reference: CacheRef<'_>,
    cancellation: &CancellationToken,
    raw: &BlobLease,
    part: &parts::ManifestPart,
    input: &[u8],
    limit: usize,
) -> ContentResult<ContentOutcome> {
    let (decoder, expected) = attachment_decoder(part, limit)?;
    let key = ArtifactKey::Attachment {
        original_index: part.attachment_index as u32,
        mime_version: 1,
    };
    let mut stage = cache.begin_blob(
        reference,
        key,
        expected.map(|size| size as u64),
        limit as u64,
    )?;
    decode_attachment(cancellation, decoder, input, &mut stage)?;
    raw.revalidate()?;
    let info = stage.publish(&|| cancellation.is_cancelled())?;
    Ok(ContentOutcome::PresentBlob(info.id))
}

fn decode_attachment(
    cancellation: &CancellationToken,
    mut decoder: Decoder,
    input: &[u8],
    mut output_writer: impl Write,
) -> ContentResult<()> {
    let mut output = vec![0; OUTPUT_CHUNK];
    let mut offset = 0;
    loop {
        if cancellation.is_cancelled() {
            return Err(ContentError::Cancelled);
        }
        let end = (offset + OUTPUT_CHUNK).min(input.len());
        let eof = end == input.len();
        let step = decoder
            .step(&input[offset..end], &mut output, eof)
            .map_err(decode_error)?;
        offset += step.consumed;
        output_writer
            .write_all(&output[..step.produced])
            .map_err(CacheError::from)?;
        match step.status {
            StepStatus::Finished if eof && offset == input.len() => break,
            StepStatus::NeedOutput => {}
            StepStatus::NeedInput if !eof && offset == end => {}
            _ => return Err(ContentError::Stale),
        }
    }
    Ok(())
}

pub(super) fn reclaim(
    cache: &Arc<Cache>,
    account: &AccountKey,
    mailbox: &str,
    position: Option<SyncReclaimPosition>,
    cancellation: &CancellationToken,
) -> Result<(bool, Option<SyncReclaimPosition>), SyncError> {
    let page = match cache.sync_reclaim_candidates(account, mailbox, position, 1) {
        Err(SyncError::Stale) if position.is_some() => {
            cache.sync_reclaim_candidates(account, mailbox, None, 1)?
        }
        result => result?,
    };
    let Some(candidate) = page.candidates.first() else {
        return Ok((false, page.next));
    };
    let reference = CacheRef {
        account,
        mailbox,
        uid_validity: candidate.uid_validity,
        uid: candidate.uid,
    };
    match same_derivation(cache, reference, candidate, cancellation) {
        Ok(true) if !cancellation.is_cancelled() => {
            if cache.evict_sync_candidate(reference, candidate)? {
                return Ok((true, None));
            }
        }
        Err(ContentError::Storage(CacheError::Storage)) => return Err(CacheError::Storage.into()),
        Err(ContentError::Cancelled) => return Err(SyncError::Cancelled),
        _ => {}
    }
    Ok((false, page.next))
}

#[derive(Default)]
struct DigestWriter {
    digest: Sha256,
    bytes: u64,
}

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.digest.update(bytes);
        self.bytes += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn same_derivation(
    cache: &Arc<Cache>,
    reference: CacheRef<'_>,
    candidate: &SyncReclaimCandidate,
    cancellation: &CancellationToken,
) -> ContentResult<bool> {
    let mut raw = cache.open_blob(
        reference,
        ArtifactKey::Raw,
        Access::LiveVerified,
        Some(candidate.raw.id),
    )?;
    let bytes = read_raw(&mut raw, candidate.raw.bytes as usize, cancellation)?;
    let parsed = mime::parse(&bytes).map_err(ContentError::decoded)?;
    let mut output = DigestWriter::default();
    match candidate.kind {
        cached_body::KEY => {
            let materialized = cache.open_blob(
                reference,
                candidate.kind,
                Access::LiveVerified,
                Some(candidate.materialized.id),
            )?;
            let body = raw_body(&parsed)?;
            let metadata = body
                .encode(BodyWriter {
                    stage: &mut output,
                    cancellation,
                })
                .map_err(CacheError::from)?;
            if materialized.body_metadata() != Some(metadata.as_slice()) {
                return Ok(false);
            }
            materialized.revalidate()?;
        }
        ArtifactKey::Attachment {
            original_index,
            mime_version: 1,
        } => {
            let manifest = parts::from_raw(&parsed).map_err(ContentError::decoded)?;
            let part = attachment(&manifest.manifest, original_index)?;
            let entity = manifest
                .entities
                .get(original_index as usize)
                .ok_or(ContentError::Stale)?;
            let (decoder, _) = attachment_decoder(part, crate::config::DEFAULT_MAX_MESSAGE)?;
            decode_attachment(cancellation, decoder, entity.encoded_body(), &mut output)?;
        }
        _ => return Ok(false),
    }
    raw.revalidate()?;
    Ok(output.bytes == candidate.materialized.bytes
        && <[u8; 32]>::from(output.digest.finalize()) == candidate.materialized.sha256)
}
