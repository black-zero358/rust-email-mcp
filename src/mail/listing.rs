//! Bounded summary pages with optional attachment filtering and body previews.
use super::{
    classification::{self, Coverage},
    parts::{fetch, preview, reader},
    *,
};
use async_imap::types::{CommandReadLimits, Flag};
use imap_proto::{AttributeValue, MessageSection, Response, SectionPath};
use tokio::time::{Duration, Instant};

const SUMMARY_HEADER_BYTES: usize = 8192;
const PREVIEW_TIME: Duration = Duration::from_secs(5);

pub(super) struct ListScan {
    start: u32,
    end: u32,
    limit: usize,
    ids: Vec<u32>,
}

impl MailService {
    pub(super) async fn list_operation(
        &self,
        name: &str,
        args: &Value,
        deadline: Instant,
    ) -> Result<Value> {
        let mailbox = folder(args, InputLimits::Read)?;
        let wire_mailbox = mailbox_name(mailbox, InputLimits::Read)?;
        let with_preview = match args.get("preview") {
            None => false,
            Some(Value::Bool(value)) => *value,
            _ => return Err(MailError::input()),
        };
        let remaining = Arc::new(AtomicUsize::new(self.config.max_message * 4));
        let mut session = self.connect_for(true, Some(remaining.clone())).await?;
        let state = select_list_mailbox(&mut session, &wire_mailbox).await?;
        self.observe_mailbox(mailbox, &state);
        check_validity(args, state.uid_validity)?;
        let Some(scan) = scan_list(&mut session, args, name == "search_emails", &state).await?
        else {
            return Ok(
                json!({"emails":[],"next_before_uid":null,"uid_validity":state.uid_validity}),
            );
        };

        let mut raw_preview = None;
        let (ids, next) = if let Some(wanted) = args.get("has_attachment") {
            let wanted = wanted.as_bool().ok_or_else(MailError::input)?;
            let mut matches = Vec::new();
            let mut last_examined = None;
            let mut resume = None;
            for (index, &uid) in scan.ids.iter().enumerate() {
                let known_size = reader::size(&mut session, uid)
                    .await
                    .map_err(reader::FetchFailure::mail)?;
                let (attached, used_fallback) = match reader::manifest(&mut session, uid).await {
                    Ok(manifest) => {
                        self.persist_manifest(mailbox, state.uid_validity, uid, &manifest);
                        (manifest.parts.iter().any(|part| part.is_attachment), false)
                    }
                    Err(reader::FetchFailure::Fallback { .. }) => {
                        let max = self
                            .config
                            .max_message
                            .min(crate::config::DEFAULT_MAX_MESSAGE);
                        if known_size > max {
                            return Err(raw_limit(InputLimits::Read, max, known_size));
                        }
                        let validity = state
                            .uid_validity
                            .filter(|value| *value > 0)
                            .ok_or_else(MailError::backend)?;
                        drop(session);
                        session = self.connect_for(true, Some(remaining.clone())).await?;
                        let fresh = session
                            .examine(&wire_mailbox)
                            .await
                            .map_err(map_read_imap_error)?;
                        self.observe_mailbox(mailbox, &fresh);
                        check_validity(&json!({"uid_validity":validity}), fresh.uid_validity)?;
                        let size = reader::size(&mut session, uid)
                            .await
                            .map_err(reader::FetchFailure::mail)?;
                        if size != known_size {
                            return Err(MailError::backend());
                        }
                        let raw =
                            fetch::whole_message(&mut session, uid, known_size + 1, known_size)
                                .await?;
                        let attached = attachments::has_attachment(&raw)?;
                        if with_preview && attached == wanted {
                            raw_preview = Some((uid, raw));
                        }
                        (attached, true)
                    }
                    Err(fault) => return Err(fault.mail()),
                };
                last_examined = Some(uid);
                if attached == wanted {
                    matches.push(uid);
                }
                if used_fallback {
                    // The fresh connection can finish this page. Resume scanning
                    // on the next page so each page has at most one raw strategy.
                    if index + 1 < scan.ids.len() {
                        resume = Some(uid);
                    }
                    break;
                }
                if matches.len() == scan.limit {
                    break;
                }
            }
            let next = resume.or_else(|| {
                page_cursor(scan.start, &matches, scan.limit, false, true, last_examined)
            });
            (matches, next)
        } else {
            scan.unfiltered()
        };
        let mut emails = self
            .fetch_summaries(&mut session, mailbox, state.uid_validity, &ids)
            .await?;
        if with_preview && !emails.is_empty() {
            let now = Instant::now();
            let preview_deadline =
                now + PREVIEW_TIME.min(deadline.saturating_duration_since(now) / 2);
            attach_previews(&mut session, &mut emails, preview_deadline, raw_preview).await;
        }
        let mut result =
            json!({"emails":emails,"next_before_uid":next,"uid_validity":state.uid_validity});
        if !ids.is_empty() {
            result["scanned_uid_range"] = json!([scan.start, scan.end]);
        }
        Ok(result)
    }

    pub(super) async fn fetch_summaries<S>(
        &self,
        session: &mut async_imap::Session<S>,
        mailbox: &str,
        validity: Option<u32>,
        ids: &[u32],
    ) -> Result<Vec<Value>>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let set = ids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        #[derive(Default)]
        struct Summary {
            header: Option<Vec<u8>>,
            size: Option<u32>,
            flags: Vec<String>,
        }
        let mut summaries = std::collections::BTreeMap::<u32, Summary>::new();
        reader::command(
            session,
            format!("UID FETCH {set} (UID FLAGS RFC822.SIZE BODY.PEEK[HEADER]<0.{SUMMARY_HEADER_BYTES}>)"),
            Some(CommandReadLimits::body_section_literal_bytes(SUMMARY_HEADER_BYTES)),
            |response| {
                use reader::FetchFailure::Backend;
                let Response::Fetch(_, attrs) = response.parsed() else {
                    return Ok(());
                };
                let mut reported = attrs.iter().filter_map(|attr| match attr {
                    AttributeValue::Uid(value) => Some(*value),
                    _ => None,
                });
                let Some(first) = reported.next() else {
                    return Ok(());
                };
                if let Some(second) = reported.next() {
                    if ids.contains(&first) || ids.contains(&second) || reported.any(|uid| ids.contains(&uid)) {
                        return Err(Backend);
                    }
                    return Ok(());
                }
                if !ids.contains(&first) {
                    return Ok(());
                }
                let summary = summaries.entry(first).or_default();
                for attr in attrs {
                    match attr {
                        AttributeValue::BodySection {
                            section: Some(SectionPath::Full(MessageSection::Header)),
                            index, data,
                        } => {
                            if *index != Some(0) || summary.header.is_some() {
                                return Err(Backend);
                            }
                            let data = data.as_deref().ok_or(Backend)?;
                            if data.len() > SUMMARY_HEADER_BYTES {
                                return Err(Backend);
                            }
                            summary.header = Some(data.to_vec());
                        }
                        AttributeValue::Rfc822Size(size) => summary.size = Some(*size),
                        AttributeValue::Flags(flags) => {
                            summary.flags = flags.iter().map(|flag| format!("{:?}", Flag::from(flag.as_ref()))).collect();
                        }
                        _ => {}
                    }
                }
                Ok(())
            },
        ).await.map_err(reader::FetchFailure::mail)?;
        let mut emails = Vec::new();
        let mut observations = Vec::new();
        let persistent = self.persists(mailbox);
        for (id, summary) in summaries.into_iter().rev() {
            // FLAGS-only updates cannot create a summary with blank headers.
            let Some(header) = summary.header.as_deref() else {
                continue;
            };
            // A short partial reply exhausts the requested HEADER, including
            // header-only messages with no blank delimiter (RFC 3501 §6.4.5).
            let complete = header.len() < SUMMARY_HEADER_BYTES
                || header.ends_with(b"\r\n\r\n")
                || header.ends_with(b"\n\n");
            let (classification, headers) = if complete {
                let headers = mime::headers(header)?;
                self.remember_headers(mailbox, validity, id, header);
                (
                    classification::classify(&headers, Coverage::Complete),
                    headers,
                )
            } else {
                let prefix = classification::complete_header_prefix(header);
                let known = mime::headers(prefix)?;
                // Preserve existing summary fields where the partial literal can
                // be parsed. Classification uses only fully observed fields.
                let classification = classification::classify(&known, Coverage::Partial);
                (classification, mime::headers(header).unwrap_or(known))
            };
            let mut email =
                serde_json::to_value(classification).map_err(|_| MailError::backend())?;
            email["uid"] = json!(id);
            email["subject"] = json!(headers.get_first_value("Subject"));
            email["from"] = json!(headers.get_first_value("From"));
            email["date"] = json!(headers.get_first_value("Date"));
            email["size"] = json!(summary.size);
            email["flags"] = json!(summary.flags);
            if persistent {
                observations.push(persistence::HeaderObservation::new(
                    id, header, complete, &headers,
                ));
            }
            emails.push(email);
        }
        self.persist_headers(mailbox, validity, observations);
        Ok(emails)
    }
}

pub(super) async fn select_list_mailbox<S>(
    session: &mut async_imap::Session<S>,
    wire_mailbox: &str,
) -> Result<async_imap::types::Mailbox>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    session
        .examine(wire_mailbox)
        .await
        .map_err(map_read_imap_error)
}

impl ListScan {
    pub(super) fn unfiltered(&self) -> (Vec<u32>, Option<u32>) {
        let more = self.ids.len() > self.limit;
        let ids = self.ids[..self.ids.len().min(self.limit)].to_vec();
        let next = page_cursor(self.start, &ids, self.limit, more, false, None);
        (ids, next)
    }
}

pub(super) async fn scan_list<S>(
    session: &mut async_imap::Session<S>,
    args: &Value,
    search: bool,
    state: &async_imap::types::Mailbox,
) -> Result<Option<ListScan>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let limit = number(args, "limit", 20, 1, LIST_PAGE_ITEMS as u64)? as usize;
    let before = number(
        args,
        "before_uid",
        state.uid_next.unwrap_or(u32::MAX) as u64,
        1,
        u32::MAX as u64,
    )? as u32;
    if state.exists == 0 || before <= 1 {
        return Ok(None);
    }
    let end = before - 1;
    let start = end.saturating_sub(SCAN_UID_WINDOW - 1).max(1);
    let mut filters = args.clone();
    for field in ["query", "from", "to", "subject"] {
        filters
            .as_object_mut()
            .ok_or_else(MailError::input)?
            .remove(field);
    }
    let query = search_query(&filters, start, end, search)?;
    let has_text = search
        && ["query", "from", "to", "subject"]
            .iter()
            .any(|key| args.get(key).is_some());
    let mut found = if has_text && query == format!("UID {start}:{end}") {
        None
    } else {
        Some(
            session
                .uid_search(query)
                .await
                .map_err(map_read_imap_error)?,
        )
    };
    if search {
        for (field, key) in [
            ("query", "TEXT"),
            ("from", "FROM"),
            ("to", "TO"),
            ("subject", "SUBJECT"),
        ] {
            if args.get(field).is_some() {
                let matching =
                    uid_field_search(session, start, end, key, text(args, field)?).await?;
                if let Some(ids) = &mut found {
                    ids.retain(|uid| matching.contains(uid));
                } else {
                    found = Some(matching);
                }
            }
        }
    }
    let mut ids = found
        .unwrap_or_default()
        .into_iter()
        .filter(|u| (start..=end).contains(u))
        .collect::<Vec<_>>();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    Ok(Some(ListScan {
        start,
        end,
        limit,
        ids,
    }))
}

async fn attach_previews<S>(
    session: &mut async_imap::Session<S>,
    emails: &mut [Value],
    deadline: Instant,
    mut raw_preview: Option<(u32, Vec<u8>)>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let mut remaining = preview::PAGE_ENCODED_BYTES;
    let mut stopped = false;
    for email in emails {
        let status = if stopped {
            "unprocessed"
        } else if remaining == 0 {
            "budget_skipped"
        } else {
            let uid = email["uid"].as_u64().expect("summary has a UID") as u32;
            let result = if Instant::now() < deadline {
                if let Some((_, raw)) = raw_preview.take_if(|(target, _)| *target == uid) {
                    let result = preview::from_raw(&raw, &mut remaining);
                    (Instant::now() < deadline).then_some(result)
                } else {
                    tokio::time::timeout_at(deadline, preview::read(session, uid, &mut remaining))
                        .await
                        .ok()
                }
            } else {
                None
            };
            match result {
                Some(Ok(preview::Preview::Text { text, truncated })) => {
                    let status = if text.is_empty() && !truncated {
                        "empty"
                    } else {
                        "ok"
                    };
                    email["preview"] = json!(text);
                    email["preview_truncated"] = json!(truncated);
                    status
                }
                Some(Ok(preview::Preview::BudgetSkipped)) => "budget_skipped",
                Some(Err(fault)) => {
                    stopped = !fault.reusable;
                    email["preview_error"] = json!(fault.error);
                    "error"
                }
                None => {
                    stopped = true;
                    email["preview_error"] = json!(MailError::timeout());
                    "error"
                }
            }
        };
        email["preview_status"] = json!(status);
    }
}
