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
                    if reader.fallback_used.contains(&id)
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
    fallback_used: std::collections::HashSet<u32>,
}
impl<'a> BodyReadSession<'a> {
    pub(in crate::mail) async fn open(service: &'a MailService, args: &Value) -> Result<Self> {
        let mailbox = folder(args, InputLimits::Read)?.to_owned();
        let mut session = service.connect_read().await?;
        let state = session
            .examine(mailbox_name(&mailbox, InputLimits::Read)?)
            .await
            .map_err(map_read_imap_error)?;
        state
            .uid_validity
            .filter(|value| *value > 0)
            .ok_or_else(MailError::backend)?;
        check_validity(args, state.uid_validity)?;
        Ok(Self {
            service,
            session: Some(session),
            state,
            mailbox,
            fallback_used: Default::default(),
        })
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
        let format = insights::display_format(args).map_err(ReadFault::local)?;
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| ReadFault::stop(MailError::backend()))?;
        let size = reader::size(session, uid)
            .await
            .map_err(|e| ReadFault::command(e.mail()))?;
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
            Err(error) => return Err(ReadFault::command(error.mail())),
        };
        let selected = loop {
            // A probe can interrupt an alternative after collecting fragments.
            // Every retry starts at the root with a fresh transaction.
            let mut selected = Vec::new();
            match mime::selection::select(
                Node {
                    manifest: &manifest,
                    index: 0,
                },
                &mut selected,
                true,
            ) {
                Ok(true) => return Err(ReadFault::local(crate::html::unsupported())),
                Ok(false) => {
                    break selected
                        .into_iter()
                        .map(|node| node.index)
                        .collect::<Vec<_>>();
                }
                Err(node) => {
                    let index = node.index;
                    let section = match &manifest.body.nodes[index].kind {
                        BodyKind::Leaf(part) => &manifest.parts[*part].section,
                        BodyKind::Container { section, .. } => section,
                    };
                    let bytes = fetch::section_chunk(session, uid, section, 0, 1, None)
                        .await
                        .map_err(ReadFault::command)?;
                    let nonempty = Some(!bytes.is_empty());
                    manifest.body.nodes[index].nonempty = nonempty;
                }
            }
        };
        let mut remaining = 32usize * 1024 * 1024;
        let mut context = DisplayContext::new(format);
        let mut text = String::new();
        // Match the raw reader's validation order across the chosen leaves.
        // A failure discards the result without changing the selection.
        for node in selected {
            let BodyKind::Leaf(index) = manifest.body.nodes[node].kind else {
                return Err(ReadFault::local(MailError::backend()));
            };
            let part = &manifest.parts[index];
            let encoded = part
                .encoded_size
                .ok_or_else(|| ReadFault::local(MailError::backend()))?
                as usize;
            let reserve = encoded.saturating_mul(4);
            let charset = match charset_decode::CharsetDecoder::new(
                part.charset.as_bytes(),
                encoded != 0,
                reserve,
            ) {
                Ok(decoder) => Some(decoder),
                Err(charset_decode::DecodeError::RawFallbackUtf7) => None,
                Err(error) => return Err(ReadFault::local(charset_error(error))),
            };
            remaining = remaining
                .checked_sub(reserve)
                .ok_or_else(|| ReadFault::local(body_limit()))?;
            let Some(mut charset) = charset else {
                return self
                    .raw_fallback(
                        uid,
                        size,
                        hard_limit,
                        args,
                        reader::Fallback::CharsetUtf7,
                        true,
                    )
                    .await;
            };
            let encoding = match part.transfer_encoding.as_str() {
                "7bit" | "8bit" | "binary" => transfer_decode::Encoding::Identity,
                "base64" => transfer_decode::Encoding::Base64,
                "quoted-printable" => transfer_decode::Encoding::QuotedPrintable,
                _ => return Err(ReadFault::local(crate::html::unsupported())),
            };
            let mut transfer = transfer_decode::Decoder::new(encoding, reserve);
            let mut leaf = String::new();
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
                .map_err(ReadFault::command)?;
                decode_transfer(&mut transfer, &mut charset, &bytes, false, &mut leaf)
                    .map_err(ReadFault::local)?;
                offset += expected;
            }
            fetch::section_chunk(session, uid, &part.section, encoded, 1, Some(0))
                .await
                .map_err(ReadFault::command)?;
            decode_transfer(&mut transfer, &mut charset, &[], true, &mut leaf)
                .map_err(ReadFault::local)?;
            decode_charset(&mut charset, &[], true, &mut leaf).map_err(ReadFault::local)?;
            let leaf = if part.content_type == "text/html" {
                crate::html::render_selected(&leaf, &mut context).map_err(ReadFault::local)?
            } else {
                leaf
            };
            if leaf.len() > reserve
                && !(part.content_type == "text/html" && format == HtmlDisplay::Readable)
            {
                return Err(ReadFault::local(body_limit()));
            }
            text.push_str(&leaf);
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
        if self.fallback_used.contains(&uid) || self.fallback_used.len() >= 20 {
            return Err(ReadFault::stop(MailError::backend()));
        }
        self.fallback_used.insert(uid);
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
        let mut fresh = self.service.connect_read().await.map_err(ReadFault::stop)?;
        let state = fresh
            .examine(mailbox_name(&self.mailbox, InputLimits::Read).map_err(ReadFault::stop)?)
            .await
            .map_err(|e| ReadFault::stop(map_read_imap_error(e)))?;
        check_validity(
            &json!({"uid_validity":self.state.uid_validity}),
            state.uid_validity,
        )
        .map_err(ReadFault::stop)?;
        let fresh_size = reader::size(&mut fresh, uid)
            .await
            .map_err(|error| ReadFault::command(error.mail()))?;
        if fresh_size != size {
            return Err(ReadFault::stop(MailError::backend()));
        }
        let raw = fetch::whole_message(&mut fresh, uid, max + 1, size)
            .await
            .map_err(ReadFault::command)?;
        self.session = Some(fresh);
        let mut value =
            insights::parsed_with_options(&raw, uid, self.state.uid_validity, hard_limit, args)
                .map_err(ReadFault::local)?;
        value["read_diagnostics"] = serde_json::to_value(reason.diagnostics())
            .map_err(|_| ReadFault::local(MailError::backend()))?;
        Ok(value)
    }
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
fn decode_charset(
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
fn decode_transfer(
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
