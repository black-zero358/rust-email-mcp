//! One mailbox session and one fallback allowance per final body target.
use super::{
    body_tree::{BodyKind, Node},
    charset_decode, fetch, reader, transfer_decode, *,
};
use crate::html::{DisplayBody, DisplayContext, HtmlDisplay};

impl MailService {
    pub(in crate::mail) async fn body_operation(&self, name: &str, args: &Value) -> Result<Value> {
        let mut reader = BodyReadSession::open(self, args).await?;
        match name {
            "get_email" => reader
                .read_item(
                    uid(args)?,
                    (self.config.max_output / 2).min(crate::config::MAX_DISPLAY_BODY),
                    args,
                )
                .await
                .map_err(|fault| fault.error),
            "get_emails" => {
                let ids = args
                    .get("uids")
                    .and_then(Value::as_array)
                    .filter(|ids| !ids.is_empty() && ids.len() <= BATCH_READ_ITEMS)
                    .ok_or_else(MailError::input)?;
                let mut items: Vec<Value> = Vec::with_capacity(ids.len());
                let mut stopped = false;
                let mut attempted = 0;
                for id in ids {
                    let id = id
                        .as_u64()
                        .filter(|id| *id > 0 && *id <= u32::MAX as u64)
                        .ok_or_else(MailError::input)? as u32;
                    if stopped {
                        items.push(json!({"uid":id,"ok":false,"status":"unprocessed"}));
                        continue;
                    }
                    attempted += 1;
                    // A duplicate UID keeps its own result while reusing any
                    // completed raw fallback already represented in this batch.
                    if reader.used_fallback(id)
                        && let Some(previous) = items.iter().rfind(|item| item["uid"] == id)
                    {
                        items.push(previous.clone());
                        continue;
                    }
                    match reader.read_item(id, 64 * 1024, args).await {
                        Ok(value) => items.push(json!({"uid":id,"ok":true,"email":value})),
                        Err(fault) => {
                            stopped = !fault.reusable;
                            items.push(json!({"uid":id,"ok":false,"error":fault.error}));
                        }
                    }
                }
                Ok(
                    json!({"items":items,"requested":ids.len(),"completed":attempted,"uid_validity":reader.state.uid_validity}),
                )
            }
            "read_conversation" => self.conversation_parts(&mut reader, args).await,
            _ => Err(MailError::input()),
        }
    }
}

pub(in crate::mail) struct ReadFault {
    pub error: MailError,
    pub reusable: bool,
}
impl ReadFault {
    pub(in crate::mail) fn command(error: MailError) -> Self {
        // The body tools historically report missing FETCH data as a backend fault.
        Self::stop(if error.code == "NOT_FOUND" {
            MailError::backend()
        } else {
            error
        })
    }
    pub(in crate::mail) fn stop(error: MailError) -> Self {
        Self {
            error,
            reusable: false,
        }
    }
    pub(in crate::mail) fn local(error: MailError) -> Self {
        Self {
            error,
            reusable: true,
        }
    }
}

pub(in crate::mail) struct BodyReadSession<'a> {
    service: &'a MailService,
    session: Option<Session>,
    pub state: async_imap::types::Mailbox,
    mailbox: String,
    fallback_used: std::collections::HashSet<(String, u32, u32)>,
    transport: Option<Arc<AtomicUsize>>,
}
impl<'a> BodyReadSession<'a> {
    pub(in crate::mail) async fn open(service: &'a MailService, args: &Value) -> Result<Self> {
        let mailbox = folder(args, InputLimits::Read)?.to_owned();
        let transport = Arc::new(AtomicUsize::new(service.config.max_message * 4));
        let session = service.connect_for(true, Some(transport.clone())).await?;
        let mut reader = Self::from_connection(service, session, Some(transport));
        reader.examine(&mailbox, args).await?;
        Ok(reader)
    }

    pub(in crate::mail) fn from_connection(
        service: &'a MailService,
        session: Session,
        transport: Option<Arc<AtomicUsize>>,
    ) -> Self {
        Self {
            service,
            session: Some(session),
            state: Default::default(),
            mailbox: String::new(),
            fallback_used: Default::default(),
            transport,
        }
    }

    pub(in crate::mail) async fn examine(&mut self, mailbox: &str, args: &Value) -> Result<()> {
        if self.session.is_none() {
            self.session = Some(
                self.service
                    .connect_for(true, self.transport.clone())
                    .await?,
            );
        }
        self.examine_checked(mailbox)
            .await
            .map_err(|fault| fault.error)?;
        check_validity(args, self.state.uid_validity)
    }

    pub(in crate::mail) async fn examine_checked(
        &mut self,
        mailbox: &str,
    ) -> std::result::Result<(), ReadFault> {
        let encoded = mailbox_name(mailbox, InputLimits::Read).map_err(ReadFault::local)?;
        let state = self
            .session()
            .map_err(ReadFault::stop)?
            .examine(encoded)
            .await
            .map_err(|error| {
                let reusable = matches!(
                    error,
                    async_imap::error::Error::No(_) | async_imap::error::Error::Bad(_)
                );
                ReadFault {
                    error: map_read_imap_error(error),
                    reusable,
                }
            })?;
        self.service.observe_mailbox(mailbox, &state);
        state
            .uid_validity
            .filter(|value| *value > 0)
            .ok_or_else(|| ReadFault::local(MailError::backend()))?;
        self.state = state;
        self.mailbox = mailbox.to_owned();
        Ok(())
    }

    pub(in crate::mail) fn disconnect(&mut self) {
        drop(self.session.take());
    }

    pub(in crate::mail) fn replace_connection(&mut self, session: Session) {
        self.session = Some(session);
    }

    fn fallback_key(&self, uid: u32) -> (String, u32, u32) {
        (
            self.mailbox.clone(),
            self.state.uid_validity.unwrap_or(0),
            uid,
        )
    }

    fn used_fallback(&self, uid: u32) -> bool {
        self.fallback_used.contains(&self.fallback_key(uid))
    }
    pub(in crate::mail) fn session(&mut self) -> Result<&mut Session> {
        self.session.as_mut().ok_or_else(MailError::backend)
    }
    pub(in crate::mail) async fn read_item(
        &mut self,
        uid: u32,
        hard_limit: usize,
        args: &Value,
    ) -> std::result::Result<Value, ReadFault> {
        let result = Box::pin(self.read_item_inner(uid, hard_limit, args)).await;
        if let Err(fault) = &result {
            use crate::cache::{ArtifactState, Omission};
            let evidence = match fault.error.code {
                "UNSUPPORTED_BODY" | "UNSUPPORTED_CHARSET" => {
                    Some(ArtifactState::Unsupported(Omission::UnsupportedContent))
                }
                "UNSUPPORTED_TRANSFER_ENCODING" => {
                    Some(ArtifactState::Unsupported(Omission::UnsupportedEncoding))
                }
                "LIMIT_EXCEEDED"
                    if matches!(
                        fault
                            .error
                            .details
                            .as_deref()
                            .and_then(|details| details.limit_kind),
                        Some(LimitKind::DecodedBodyBytes)
                    ) =>
                {
                    Some(ArtifactState::Omitted(Omission::DecodedLimit))
                }
                _ => None,
            };
            if let Some(state) = evidence {
                self.service.cache_artifact_state(
                    &self.mailbox,
                    self.state.uid_validity,
                    uid,
                    cached_body::KEY,
                    state,
                );
            }
        }
        result
    }

    async fn read_item_inner(
        &mut self,
        uid: u32,
        hard_limit: usize,
        args: &Value,
    ) -> std::result::Result<Value, ReadFault> {
        let format = insights::display_format(args).map_err(ReadFault::local)?;
        let command_fault = |error| {
            ReadFault::command(self.service.observe_fetch_error(
                &self.mailbox,
                self.state.uid_validity,
                self.state.uid_next,
                uid,
                error,
            ))
        };
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| ReadFault::stop(MailError::backend()))?;
        let size = reader::size(session, uid)
            .await
            .map_err(|error| command_fault(error.mail()))?;
        if self
            .service
            .persistent
            .full_folder(&self.service.account, &self.mailbox)
            .is_some()
        {
            self.service.persist_listing(
                &self.mailbox,
                self.state.uid_validity,
                vec![persistence::ListingObservation {
                    uid,
                    flags: None,
                    internal_date: None,
                    size: u32::try_from(size).ok(),
                }],
            );
        }
        if let Some(value) = self
            .service
            .cached_live_body(
                &self.mailbox,
                self.state.uid_validity,
                uid,
                hard_limit,
                args,
            )
            .await
        {
            return Ok(value);
        }
        if size
            > self
                .service
                .config
                .max_message
                .min(crate::config::DEFAULT_MAX_MESSAGE)
        {
            self.service.cache_artifact_state(
                &self.mailbox,
                self.state.uid_validity,
                uid,
                crate::cache::ArtifactKey::Raw,
                crate::cache::ArtifactState::Omitted(crate::cache::Omission::RawLimit),
            );
        }
        let mut manifest = match reader::manifest(session, uid).await {
            Ok(manifest) => manifest,
            Err(reader::FetchFailure::Fallback {
                reason,
                synchronized,
            }) => {
                return self
                    .raw_fallback(uid, size, hard_limit, args, reason, synchronized)
                    .await;
            }
            Err(error) => return Err(command_fault(error.mail())),
        };
        self.service.persist_structured_manifest(
            &self.mailbox,
            self.state.uid_validity,
            uid,
            &manifest,
        );
        let selected = selected_leaves(session, uid, &mut manifest)
            .await
            .map_err(|fault| fault.read_fault(command_fault))?;
        let mut remaining = cached_body::LIMIT;
        let mut context = DisplayContext::new(format);
        let mut text = String::new();
        let capture = self.service.content_capture(&self.mailbox);
        let mut complete = cached_body::Record::default();
        // Match the raw reader's validation order across the chosen leaves.
        // A failure discards the result without changing the selection.
        for index in selected {
            let (leaf, html, reserve) = match decoded_leaf(
                session,
                uid,
                &manifest.parts[index],
                &mut remaining,
                capture.is_some(),
            )
            .await
            {
                Ok(leaf) => leaf,
                Err(StructuredFault::RawFallback(reason)) => {
                    return self
                        .raw_fallback(uid, size, hard_limit, args, reason, true)
                        .await;
                }
                Err(fault) => return Err(fault.read_fault(command_fault)),
            };
            let displayed = if html {
                std::borrow::Cow::Owned(
                    crate::html::render_selected(&leaf, &mut context).map_err(ReadFault::local)?,
                )
            } else {
                std::borrow::Cow::Borrowed(leaf.as_str())
            };
            if displayed.len() > reserve && !(html && format == HtmlDisplay::Readable) {
                return Err(ReadFault::local(body_limit()));
            }
            text.push_str(&displayed);
            drop(displayed);
            if capture.is_some() {
                complete.push(leaf, html, reserve);
            }
        }
        let headers = mime::headers(&manifest.body.root_headers).map_err(ReadFault::local)?;
        let metadata =
            insights::manifest_metadata(&headers, &manifest, uid, self.state.uid_validity);
        let mut value = insights::with_displayed(
            metadata,
            &headers,
            DisplayBody {
                text,
                metadata: context.into_metadata(),
            },
            hard_limit,
            args,
        )
        .map_err(ReadFault::local)?;
        value["read_diagnostics"] = serde_json::to_value(reader::ManifestDiagnostics::structured())
            .map_err(|_| ReadFault::local(MailError::backend()))?;
        if let Some(permit) = capture {
            self.service.capture_body(
                &self.mailbox,
                self.state.uid_validity,
                uid,
                complete,
                permit,
            );
        }
        Ok(value)
    }
    async fn raw_fallback(
        &mut self,
        uid: u32,
        size: usize,
        hard_limit: usize,
        args: &Value,
        reason: reader::Fallback,
        synchronized: bool,
    ) -> std::result::Result<Value, ReadFault> {
        if self.used_fallback(uid) || self.fallback_used.len() >= 20 {
            return Err(ReadFault::stop(MailError::backend()));
        }
        self.fallback_used.insert(self.fallback_key(uid));
        let max = self
            .service
            .config
            .max_message
            .min(crate::config::DEFAULT_MAX_MESSAGE);
        if size > max {
            if !synchronized {
                drop(self.session.take());
            }
            return Err(ReadFault {
                error: raw_limit(InputLimits::Read, max, size),
                reusable: synchronized,
            });
        }
        // This is an explicit strategy fallback, never a command retry.
        drop(self.session.take());
        let mut fresh = self
            .service
            .connect_for(true, self.transport.clone())
            .await
            .map_err(ReadFault::stop)?;
        let state = fresh
            .examine(mailbox_name(&self.mailbox, InputLimits::Read).map_err(ReadFault::stop)?)
            .await
            .map_err(|e| ReadFault::stop(map_read_imap_error(e)))?;
        self.service.observe_mailbox(&self.mailbox, &state);
        check_validity(
            &json!({"uid_validity":self.state.uid_validity}),
            state.uid_validity,
        )
        .map_err(ReadFault::stop)?;
        let command_fault = |error| {
            ReadFault::command(self.service.observe_fetch_error(
                &self.mailbox,
                self.state.uid_validity,
                state.uid_next,
                uid,
                error,
            ))
        };
        let fresh_size = reader::size(&mut fresh, uid)
            .await
            .map_err(|error| command_fault(error.mail()))?;
        if fresh_size != size {
            return Err(ReadFault::stop(MailError::backend()));
        }
        let raw = fetch::whole_message(&mut fresh, uid, max + 1, size)
            .await
            .map_err(command_fault)?;
        self.session = Some(fresh);
        let value = if self.service.persists(&self.mailbox) {
            mime::parse(&raw).and_then(|part| {
                self.service
                    .persist_raw(&self.mailbox, self.state.uid_validity, uid, &raw, &part);
                insights::parsed_with_part(&part, uid, self.state.uid_validity, hard_limit, args)
            })
        } else {
            insights::parsed_with_options(&raw, uid, self.state.uid_validity, hard_limit, args)
        };
        if let Some(permit) = self.service.content_capture(&self.mailbox) {
            self.service
                .capture_raw(&self.mailbox, self.state.uid_validity, uid, raw, permit);
        }
        let mut value = value.map_err(ReadFault::local)?;
        value["read_diagnostics"] = serde_json::to_value(reason.diagnostics())
            .map_err(|_| ReadFault::local(MailError::backend()))?;
        Ok(value)
    }
}

pub(in crate::mail) enum StructuredFault {
    Command(MailError),
    Local(MailError),
    RawFallback(reader::Fallback),
}

impl StructuredFault {
    fn read_fault(self, command: impl FnOnce(MailError) -> ReadFault) -> ReadFault {
        match self {
            Self::Command(error) => command(error),
            Self::Local(error) => ReadFault::local(error),
            Self::RawFallback(_) => ReadFault::local(crate::html::unsupported()),
        }
    }
}

async fn selected_leaves<S>(
    session: &mut async_imap::Session<S>,
    uid: u32,
    manifest: &mut PartManifest,
) -> std::result::Result<Vec<usize>, StructuredFault>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    loop {
        // Restart selection from the root after a probe. Fragments from an
        // incomplete alternative never enter the next attempt.
        let mut selected = Vec::new();
        match mime::selection::select(Node { manifest, index: 0 }, &mut selected, true) {
            Ok(true) => return Err(StructuredFault::Local(crate::html::unsupported())),
            Ok(false) => {
                return selected
                    .into_iter()
                    .map(|node| match manifest.body.nodes[node.index].kind {
                        BodyKind::Leaf(index) => Ok(index),
                        BodyKind::Container { .. } => {
                            Err(StructuredFault::Local(MailError::backend()))
                        }
                    })
                    .collect();
            }
            Err(node) => {
                let index = node.index;
                let section = match &manifest.body.nodes[index].kind {
                    BodyKind::Leaf(part) => &manifest.parts[*part].section,
                    BodyKind::Container { section, .. } => section,
                };
                let bytes = fetch::section_chunk(session, uid, section, 0, 1, None)
                    .await
                    .map_err(StructuredFault::Command)?;
                manifest.body.nodes[index].nonempty = Some(!bytes.is_empty());
            }
        }
    }
}

async fn decoded_leaf<S>(
    session: &mut async_imap::Session<S>,
    uid: u32,
    part: &ManifestPart,
    remaining: &mut usize,
    reserve_capacity: bool,
) -> std::result::Result<(String, bool, usize), StructuredFault>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let encoded = part
        .encoded_size
        .ok_or_else(|| StructuredFault::Local(MailError::backend()))? as usize;
    let reserve = encoded.saturating_mul(4);
    let charset =
        match charset_decode::CharsetDecoder::new(part.charset.as_bytes(), encoded != 0, reserve) {
            Ok(decoder) => Some(decoder),
            Err(charset_decode::DecodeError::RawFallbackUtf7) => None,
            Err(error) => return Err(StructuredFault::Local(charset_error(error))),
        };
    *remaining = remaining
        .checked_sub(reserve)
        .ok_or_else(|| StructuredFault::Local(body_limit()))?;
    let Some(mut charset) = charset else {
        return Err(StructuredFault::RawFallback(reader::Fallback::CharsetUtf7));
    };
    let encoding = transfer_decode::Encoding::for_label(&part.transfer_encoding)
        .ok_or_else(|| StructuredFault::Local(crate::html::unsupported()))?;
    let mut transfer = transfer_decode::Decoder::new(encoding, reserve);
    let mut leaf = if reserve_capacity {
        String::with_capacity(reserve)
    } else {
        String::new()
    };
    let mut offset = 0;
    while offset < encoded {
        let expected = (encoded - offset).min(transfer_decode::OUTPUT_CHUNK);
        let bytes = fetch::section_chunk(
            session,
            uid,
            &part.section,
            offset,
            transfer_decode::OUTPUT_CHUNK,
            Some(expected),
        )
        .await
        .map_err(StructuredFault::Command)?;
        decode_transfer(&mut transfer, &mut charset, &bytes, false, &mut leaf)
            .map_err(StructuredFault::Local)?;
        offset += expected;
    }
    // A few servers return one byte at the declared EOF. The slice reader
    // validates the target and waits for tagged OK before returning the probe.
    let eof = fetch::section_chunk(session, uid, &part.section, encoded, 1, None)
        .await
        .map_err(StructuredFault::Command)?;
    if !eof.is_empty() {
        return Err(StructuredFault::RawFallback(
            reader::Fallback::BodyEofMismatch,
        ));
    }
    decode_transfer(&mut transfer, &mut charset, &[], true, &mut leaf)
        .map_err(StructuredFault::Local)?;
    decode_charset(&mut charset, &[], true, &mut leaf).map_err(StructuredFault::Local)?;
    Ok((leaf, part.content_type == "text/html", reserve))
}

pub(in crate::mail) async fn canonical_structured<S>(
    session: &mut async_imap::Session<S>,
    uid: u32,
    manifest: &mut PartManifest,
) -> std::result::Result<cached_body::Record, StructuredFault>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let mut record = cached_body::Record::default();
    let mut remaining = cached_body::LIMIT;
    for index in selected_leaves(session, uid, manifest).await? {
        let (text, html, reserve) =
            decoded_leaf(session, uid, &manifest.parts[index], &mut remaining, true).await?;
        record.push(text, html, reserve);
    }
    Ok(record)
}

fn body_limit() -> MailError {
    MailError::limit_kind(LimitKind::DecodedBodyBytes)
}
fn charset_error(error: charset_decode::DecodeError) -> MailError {
    match error {
        charset_decode::DecodeError::Utf8Limit { .. } => body_limit(),
        charset_decode::DecodeError::UnsupportedCharset
        | charset_decode::DecodeError::RawFallbackUtf7 => crate::html::unsupported(),
        _ => MailError::backend(),
    }
}
fn transfer_error(error: transfer_decode::DecodeError) -> MailError {
    match error {
        transfer_decode::DecodeError::DecodedLimit { .. }
        | transfer_decode::DecodeError::PendingAllocation => body_limit(),
        _ => MailError::backend(),
    }
}
pub(super) fn decode_charset(
    decoder: &mut charset_decode::CharsetDecoder,
    input: &[u8],
    eof: bool,
    text: &mut String,
) -> Result<()> {
    let mut consumed = 0;
    let mut output = vec![0; transfer_decode::OUTPUT_CHUNK];
    loop {
        let step = decoder
            .step(&input[consumed..], &mut output, eof)
            .map_err(charset_error)?;
        consumed = consumed
            .checked_add(step.consumed)
            .filter(|n| *n <= input.len())
            .ok_or_else(MailError::backend)?;
        text.push_str(
            std::str::from_utf8(&output[..step.produced]).map_err(|_| MailError::backend())?,
        );
        match step.status {
            charset_decode::StepStatus::NeedOutput => {}
            charset_decode::StepStatus::NeedInput if !eof && consumed == input.len() => {
                return Ok(());
            }
            charset_decode::StepStatus::Finished if eof && consumed == input.len() => return Ok(()),
            _ => return Err(MailError::backend()),
        }
    }
}
pub(super) fn decode_transfer(
    decoder: &mut transfer_decode::Decoder,
    charset: &mut charset_decode::CharsetDecoder,
    input: &[u8],
    eof: bool,
    text: &mut String,
) -> Result<()> {
    let mut consumed = 0;
    let mut output = vec![0; transfer_decode::OUTPUT_CHUNK];
    loop {
        let step = decoder
            .step(&input[consumed..], &mut output, eof)
            .map_err(transfer_error)?;
        consumed = consumed
            .checked_add(step.consumed)
            .filter(|n| *n <= input.len())
            .ok_or_else(MailError::backend)?;
        decode_charset(charset, &output[..step.produced], false, text)?;
        match step.status {
            transfer_decode::StepStatus::NeedOutput => {}
            transfer_decode::StepStatus::NeedInput if !eof && consumed == input.len() => {
                return Ok(());
            }
            transfer_decode::StepStatus::Finished if eof && consumed == input.len() => {
                return Ok(());
            }
            _ => return Err(MailError::backend()),
        }
    }
}

#[cfg(test)]
mod tests;
