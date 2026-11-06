//! Bounded local metadata, full-text and exact-ID queries.
use super::*;
use crate::{
    cache::{BODY_INDEX_REBUILD_BATCH, BODY_INDEX_VERSION, SearchPosition, ThreadRole},
    mail::{
        classification::{self, Coverage},
        parts::preview,
    },
};
use base64::Engine;

const EXACT_WINDOW: u32 = 200;
const PREVIEW_BYTES: usize = preview::PAGE_ENCODED_BYTES;
const SEARCH_BODY: (u16, u16) = (1, 1);

impl CachedReader {
    pub(super) fn cached_query(&self, name: &str, args: &Value) -> Result<Value> {
        if name == "locate_message" {
            return self.locate(args);
        }
        let mailbox = canonical_mailbox(folder(args, InputLimits::Read)?);
        let limit = number(args, "limit", 20, 1, LIST_PAGE_ITEMS as u64)? as usize;
        let scope = self.freeze(mailbox, args)?;
        let before = args
            .get("before_uid")
            .map(|_| number(args, "before_uid", 1, 1, u32::MAX as u64))
            .transpose()?
            .map(|uid| uid as u32);
        let mut match_layers = None;
        let mut query_revision = None;
        let (rows, next_before_uid, next_cache_cursor, index_coverage) = if let Some(query) =
            args["query"].as_str()
        {
            if args.get("cache_cursor").is_none() && before.is_none() {
                self.rebuild_body_indexes(&scope)?;
            }
            let revision = self
                .cache
                .search_revision(&self.account, &scope)
                .map_err(cache_error)?;
            let key = search_key(&scope, revision, args);
            let cursor = args
                .get("cache_cursor")
                .map(|value| decode_cursor(value.as_str().ok_or_else(MailError::input)?, key))
                .transpose()?;
            if cursor.is_some() && before.is_some() {
                return Err(MailError::input());
            }
            let position =
                cursor.or_else(|| before.map(|uid| SearchPosition { uid, segment: None }));
            let page = self
                .cache
                .search_cached(
                    &self.account,
                    mailbox,
                    query,
                    position,
                    limit,
                    Some(SEARCH_BODY),
                )
                .map_err(cache_error)?;
            if page.search_revision != revision
                || page.scope.uid_validity != scope.uid_validity
                || page.scope.high_uid != scope.high_uid
                || page.scope.dirty_revision != scope.dirty_revision
                || page.scope.restore_epoch != scope.restore_epoch
            {
                return Err(cache_error(CacheError::NeedsVerification));
            }
            query_revision = Some(revision);
            let mut layers = std::collections::HashMap::with_capacity(page.items.len());
            let rows = page
                .items
                .into_iter()
                .map(|hit| {
                    layers.insert(
                        hit.message.uid,
                        json!({"metadata":hit.matched_metadata,"body":hit.matched_body}),
                    );
                    hit.message
                })
                .collect::<Vec<_>>();
            match_layers = Some(layers);
            let next_uid = page
                .next
                .as_ref()
                .filter(|cursor| cursor.segment.is_none())
                .map(|cursor| cursor.uid);
            let next_cursor = page.next.map(|next| encode_cursor(next, key));
            let rebuild_pending = self
                .cache
                .pending_body_index_rebuild(&self.account, &scope, SEARCH_BODY)
                .map_err(cache_error)?
                .is_some();
            (
                rows,
                next_uid,
                next_cursor,
                json!({"indexed_metadata":page.indexed_metadata,"indexed_body":page.indexed_body,
                    "version":BODY_INDEX_VERSION,"rebuild_pending":rebuild_pending}),
            )
        } else {
            if args.get("cache_cursor").is_some() {
                return Err(MailError::input());
            }
            let page = self
                .cache
                .list_cached(&self.account, mailbox, before, limit)
                .map_err(cache_error)?;
            if page.scope != scope {
                return Err(cache_error(CacheError::NeedsVerification));
            }
            (page.items, page.next_before_uid, None, Value::Null)
        };
        let mut unknown = 0;
        let mut headers_missing = 0;
        let mut emails = Vec::with_capacity(rows.len());
        let mut preview_left = PREVIEW_BYTES;
        for row in rows {
            if row.header.is_none() {
                headers_missing += 1;
                continue;
            }
            let reference = self.reference(mailbox, scope.uid_validity, row.uid);
            let known = match self.matches_filters(reference, &row, args)? {
                Some(known) => known,
                None => {
                    unknown += 1;
                    continue;
                }
            };
            if !known {
                continue;
            }
            let mut item = summary(&row)?;
            if let Some(layers) = match_layers
                .as_mut()
                .and_then(|layers| layers.remove(&row.uid))
            {
                item["cache_match"] = layers;
            }
            if args["preview"] == true {
                self.preview(reference, &mut item, &mut preview_left);
            }
            emails.push(item);
        }
        let mut info = self.scope_info_with_layers(&scope)?;
        self.verify(&scope, None)?;
        if let Some(revision) = query_revision
            && self
                .cache
                .search_revision(&self.account, &scope)
                .map_err(cache_error)?
                != revision
        {
            return Err(cache_error(CacheError::NeedsVerification));
        }
        info["coverage"]["filter_unknown"] = json!(unknown);
        info["coverage"]["headers_missing"] = json!(headers_missing);
        if !index_coverage.is_null() {
            info["coverage"]["search_index"] = index_coverage;
        }
        let source = if name == "list_emails" {
            "read_source"
        } else {
            "search_source"
        };
        let mut result = json!({
            "emails":emails,"next_before_uid":next_before_uid,"uid_validity":scope.uid_validity,
            "cache":cache_context(vec![info])
        });
        result[source] = json!("cache");
        if next_cache_cursor.is_some() {
            result["next_cache_cursor"] = json!(next_cache_cursor);
        }
        Ok(result)
    }

    fn rebuild_body_indexes(&self, scope: &ScopeSnapshot) -> Result<()> {
        let mut remaining = cached_body::LIMIT as u64;
        for attempt in 0..BODY_INDEX_REBUILD_BATCH {
            let Some((uid, blob, bytes)) = self
                .cache
                .next_body_index_rebuild(&self.account, scope, SEARCH_BODY, remaining, attempt == 0)
                .map_err(cache_error)?
            else {
                break;
            };
            remaining -= bytes.max(1);
            self.rebuild_body_index(scope, uid, blob)?;
        }
        self.verify(scope, None)
    }

    fn rebuild_body_index(
        &self,
        scope: &ScopeSnapshot,
        uid: u32,
        blob: crate::cache::BlobId,
    ) -> Result<()> {
        let reference = self.reference(&scope.mailbox, scope.uid_validity, uid);
        let result = (|| {
            let mut lease = self
                .cache
                .open_blob(reference, cached_body::KEY, Access::Offline, Some(blob))
                .map_err(cache_error)?;
            let cancelled = || {
                !self.persistent.offline_current(
                    &self.account,
                    &scope.mailbox,
                    scope.uid_validity,
                    Some(uid),
                )
            };
            let record = match cached_body::Record::decode(&mut lease) {
                Err(CacheError::Corrupt) => {
                    return self
                        .cache
                        .discard_old_body_index(reference, cached_body::KEY, blob, &cancelled)
                        .map_err(cache_error);
                }
                result => result.map_err(cache_error)?,
            };
            let displayed = match record.display(crate::html::HtmlDisplay::Legacy) {
                Err(error) if matches!(error.code, "LIMIT_EXCEEDED" | "UNSUPPORTED_BODY") => {
                    return self
                        .cache
                        .discard_old_body_index(reference, cached_body::KEY, blob, &cancelled)
                        .map_err(cache_error);
                }
                result => result?,
            };
            lease.revalidate().map_err(cache_error)?;
            self.cache
                .remember_full_body_index(
                    reference,
                    cached_body::KEY,
                    blob,
                    &displayed.text,
                    &cancelled,
                )
                .map_err(cache_error)
        })();
        match result {
            Ok(()) => Ok(()),
            // A cache with limited disk or an unreadable body still serves its
            // metadata and already indexed bodies with truthful coverage.
            Err(error)
                if matches!(
                    error.code,
                    "LIMIT_EXCEEDED" | "CACHE_MISS" | "CACHE_UNAVAILABLE" | "STORAGE_UNAVAILABLE"
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn matches_filters(
        &self,
        reference: CacheRef<'_>,
        row: &CachedMessage,
        args: &Value,
    ) -> Result<Option<bool>> {
        for (input, header) in [("from", "From"), ("to", "To"), ("subject", "Subject")] {
            if let Some(query) = args[input].as_str() {
                let Some(header_bytes) = row.header.as_ref().filter(|_| row.header_complete) else {
                    return Ok(None);
                };
                let headers = mime::headers(header_bytes)?;
                if !headers
                    .get_all_values(header)
                    .iter()
                    .any(|value| value.to_lowercase().contains(&query.to_lowercase()))
                {
                    return Ok(Some(false));
                }
            }
        }
        for (input, flag) in [
            ("seen", "Seen"),
            ("flagged", "Flagged"),
            ("answered", "Answered"),
        ] {
            if let Some(wanted) = args[input].as_bool() {
                let Some(flags) = &row.flags else {
                    return Ok(None);
                };
                if flags.iter().any(|value| {
                    // Older versions serialized a mixed-case system flag as Custom("\\...").
                    value == flag
                        || value
                            .strip_prefix(r#"Custom("\\"#)
                            .and_then(|value| value.strip_suffix(r#"")"#))
                            .is_some_and(|name| name.eq_ignore_ascii_case(flag))
                }) != wanted
                {
                    return Ok(Some(false));
                }
            }
        }
        if args.get("min_size").is_some() || args.get("max_size").is_some() {
            let Some(size) = row.size.map(u64::from) else {
                return Ok(None);
            };
            if args["min_size"].as_u64().is_some_and(|min| size < min)
                || args["max_size"].as_u64().is_some_and(|max| size > max)
            {
                return Ok(Some(false));
            }
        }
        if args.get("since").is_some() || args.get("before").is_some() {
            let Some(date) = row.internal_date.as_deref().and_then(server_day) else {
                return Ok(None);
            };
            if args["since"]
                .as_str()
                .is_some_and(|since| date.as_str() < since)
                || args["before"]
                    .as_str()
                    .is_some_and(|before| date.as_str() >= before)
            {
                return Ok(Some(false));
            }
        }
        if let Some(wanted) = args["has_attachment"].as_bool() {
            let Some((parts, _)) = self.cache.cached_parts(reference).map_err(cache_error)? else {
                return Ok(None);
            };
            let value = manifest(&parts)?;
            if value["parts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|part| part["is_attachment"] == true)
                != wanted
            {
                return Ok(Some(false));
            }
        }
        Ok(Some(true))
    }

    fn preview(&self, reference: CacheRef<'_>, item: &mut Value, remaining: &mut usize) {
        let result = (|| -> Result<Option<preview::Preview>> {
            let state = self
                .cache
                .artifact_state(reference, cached_body::KEY)
                .map_err(cache_error)?;
            let raw_source = |expected: Option<crate::cache::BlobId>| match self
                .cache
                .artifact_state(reference, ArtifactKey::Raw)
                .map_err(cache_error)?
            {
                ArtifactState::Materialized(info) if expected.is_none_or(|id| id == info.id) => {
                    Ok((ArtifactKey::Raw, info))
                }
                _ => Err(cache_miss()),
            };
            let (mut key, mut info) = match state {
                ArtifactState::Materialized(info) => (cached_body::KEY, info),
                ArtifactState::Derivable { raw } => raw_source(Some(raw))?,
                ArtifactState::Unsupported(_) => raw_source(None)?,
                _ => return Err(cache_miss()),
            };
            loop {
                let size = usize::try_from(info.bytes).map_err(|_| MailError::limit())?;
                if size > 8 * 1024 || size.max(1) > *remaining {
                    return Ok(None);
                }
                *remaining -= size.max(1);
                let mut lease = self
                    .cache
                    .open_blob(reference, key, Access::Offline, Some(info.id))
                    .map_err(cache_error)?;
                let value = if key == ArtifactKey::Raw {
                    let raw = blob_bytes(&mut lease, 8 * 1024)?;
                    preview::from_raw(&raw, &mut (8 * 1024)).map_err(|fault| fault.error)?
                } else {
                    let record = cached_body::Record::decode(&mut lease).map_err(cache_error)?;
                    match record.display(crate::html::HtmlDisplay::Legacy) {
                        Err(error) if error.code == "UNSUPPORTED_BODY" => {
                            (key, info) = raw_source(None).map_err(|_| error)?;
                            continue;
                        }
                        result => preview::from_displayed(&result?.text),
                    }
                };
                lease.revalidate().map_err(cache_error)?;
                lease.touch();
                lease.revalidate().map_err(cache_error)?;
                if !self.persistent.offline_current(
                    reference.account,
                    reference.mailbox,
                    reference.uid_validity,
                    Some(reference.uid),
                ) {
                    return Err(cache_error(CacheError::NeedsVerification));
                }
                return Ok(Some(value));
            }
        })();
        item["preview_status"] = json!(match result {
            Ok(Some(preview::Preview::Text { text, truncated })) => {
                let empty = text.is_empty() && !truncated;
                item["preview"] = json!(text);
                item["preview_truncated"] = json!(truncated);
                if empty { "empty" } else { "ok" }
            }
            Ok(_) => "budget_skipped",
            Err(error) => {
                item["preview_error"] = json!(error);
                "error"
            }
        });
    }

    fn locate(&self, args: &Value) -> Result<Value> {
        let id = conversation::normalized_message_id(text(args, "message_id")?)?;
        let limit = number(args, "limit", 20, 1, 50)? as usize;
        let candidates = number(args, "candidate_limit", 10, 1, 50)? as usize;
        let before = args
            .get("before_uid")
            .map(|_| number(args, "before_uid", 1, 1, u32::MAX as u64))
            .transpose()?
            .map(|uid| uid as u32);
        let folders = args["mailboxes"]
            .as_array()
            .filter(|folders| !folders.is_empty() && folders.len() <= 20)
            .ok_or_else(MailError::input)?;
        let mut seen = std::collections::HashSet::with_capacity(folders.len());
        let mut matches = Vec::new();
        let mut scopes = Vec::new();
        let mut snapshot = Vec::new();
        for folder in folders {
            let mailbox = canonical_mailbox(folder.as_str().ok_or_else(MailError::input)?);
            if !seen.insert(mailbox) {
                continue;
            }
            let scope = self.freeze(mailbox, args)?;
            let end = before.map_or(scope.high_uid, |before| scope.high_uid.min(before - 1));
            let start = end.saturating_sub(EXACT_WINDOW - 1).max(1);
            let mut uids = if end >= start {
                self.cache
                    .thread_uids(
                        &self.account,
                        &scope,
                        ThreadRole::MessageId,
                        &id,
                        start..=end,
                    )
                    .map_err(cache_error)?
            } else {
                Vec::new()
            };
            uids.reverse();
            let processed = uids
                .len()
                .min(candidates)
                .min(limit.saturating_sub(matches.len()));
            for uid in uids.iter().take(processed) {
                matches.push(json!({"mailbox":mailbox,"uid":uid,"uid_validity":scope.uid_validity,"message_id":id}));
            }
            let limited = processed < uids.len();
            let next = if limited {
                if processed == 0 {
                    before.or_else(|| end.checked_add(1))
                } else {
                    Some(uids[processed - 1])
                }
            } else {
                (start > 1).then_some(start)
            };
            scopes.push(json!({"mailbox":mailbox,"uid_validity":scope.uid_validity,"start_uid":start,"end_uid":end,
                "next_before_uid":next,"candidates_checked":processed,"candidate_limit_reached":limited,"older_uids_omitted":start>1}));
            snapshot.push(scope);
        }
        for scope in &snapshot {
            self.verify(scope, None)?;
        }
        Ok(json!({"matches":matches,"scopes":scopes,"complete":false,
            "scope":"Only explicitly supplied folders and reported UID windows were searched; follow each cursor.",
            "search_source":"cache","cache":cache_context(snapshot.iter().map(scope_info).collect())}))
    }
}

fn summary(row: &CachedMessage) -> Result<Value> {
    let coverage = if row.header_complete {
        Coverage::Complete
    } else {
        Coverage::Partial
    };
    let raw = row.header.as_deref().unwrap_or_default();
    let known = if row.header_complete {
        raw
    } else {
        classification::complete_header_prefix(raw)
    };
    let headers = mime::headers(known)?;
    let mut value = serde_json::to_value(classification::classify(&headers, coverage))
        .map_err(|_| MailError::backend())?;
    value["uid"] = json!(row.uid);
    value["subject"] = json!(headers.get_first_value("Subject"));
    value["from"] = json!(headers.get_first_value("From"));
    value["date"] = json!(headers.get_first_value("Date"));
    value["size"] = json!(row.size);
    value["flags"] = json!(row.flags);
    value["cache_evidence"] = json!({
        "header_complete":row.header_complete,"flags_checked_at_ms":row.flags_checked_at_ms,
        "internal_date":row.internal_date,"internal_date_checked_at_ms":row.date_checked_at_ms,
        "size_checked_at_ms":row.size_checked_at_ms
    });
    Ok(value)
}

fn server_day(date: &str) -> Option<String> {
    let day = date.split_ascii_whitespace().next()?;
    let mut parts = day.split('-');
    let (first, second, third) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }

    // Background observations store a parsed datetime's display string. Both
    // forms start with the calendar date in the message's original offset.
    let (year, month, day) = if first.len() == 4 && second.len() == 2 && third.len() == 2 {
        (
            first.parse::<u16>().ok()?,
            second.parse::<usize>().ok()?,
            third.parse::<u8>().ok()?,
        )
    } else {
        let month = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ]
        .iter()
        .position(|month| month.eq_ignore_ascii_case(second))?
            + 1;
        (third.parse().ok()?, month, first.parse().ok()?)
    };
    ((1..=9999).contains(&year) && (1..=12).contains(&month) && (1..=31).contains(&day))
        .then(|| format!("{year:04}-{month:02}-{day:02}"))
}

fn search_key(scope: &ScopeSnapshot, revision: u64, args: &Value) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"r11-cache-search\0");
    hash.update(BODY_INDEX_VERSION.to_be_bytes());
    hash.update(scope.account_namespace);
    hash.update(scope.uid_validity.to_be_bytes());
    hash.update(scope.high_uid.to_be_bytes());
    hash.update(revision.to_be_bytes());
    hash.update(scope.dirty_revision.to_be_bytes());
    hash.update(scope.restore_epoch.to_be_bytes());
    hash.update((scope.mailbox.len() as u32).to_be_bytes());
    hash.update(scope.mailbox.as_bytes());
    for field in [
        "query",
        "from",
        "to",
        "subject",
        "seen",
        "flagged",
        "answered",
        "since",
        "before",
        "has_attachment",
        "min_size",
        "max_size",
    ] {
        hash.update(args[field].to_string());
        hash.update(b"\0");
    }
    hash.finalize().into()
}

fn encode_cursor(position: SearchPosition, key: [u8; 32]) -> String {
    let mut bytes = [0; 38];
    bytes[..32].copy_from_slice(&key);
    bytes[32..36].copy_from_slice(&position.uid.to_be_bytes());
    bytes[36..].copy_from_slice(&position.segment.unwrap_or(u16::MAX).to_be_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn decode_cursor(token: &str, key: [u8; 32]) -> Result<SearchPosition> {
    let mut bytes = [0; 38];
    let valid = token.len() == 51
        && base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode_slice(token, &mut bytes)
            .is_ok_and(|size| size == bytes.len())
        && bytes[..32] == key;
    if !valid {
        return Err(MailError {
            code: "CURSOR_UNAVAILABLE",
            message: "The cached search cursor is unavailable. Restart the search.",
            retryable: false,
            details: None,
        });
    }
    let uid = u32::from_be_bytes(bytes[32..36].try_into().unwrap());
    let segment = u16::from_be_bytes(bytes[36..].try_into().unwrap());
    Ok(SearchPosition {
        uid,
        segment: (segment != u16::MAX).then_some(segment),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        accounts::{AccountCatalog, CacheRunIdentity},
        cache::ListingEvidence,
        config::{self, ValidatedAccountSet},
        mail::MailRuntime,
    };

    fn reader_fixture(directory: &std::path::Path, uid_next: u32) -> CachedReader {
        let path = directory.join("cache.json");
        std::fs::write(
            &path,
            br#"{"version":1,"mode":"full","directory":"cache","max_bytes":33554432,"accounts":[{"account_id":"default","mailboxes":["INBOX"],"max_bytes":8388608}]}"#,
        )
        .unwrap();
        let config = config::fixture();
        let settings = ValidatedAccountSet::legacy(config.clone());
        let cache_settings = config::load_cache_file(&path, &settings).unwrap();
        let catalog = AccountCatalog::from_test_accounts(settings).unwrap();
        let account = catalog.registered_accounts().pop().unwrap().key;
        let persistent = MailRuntime::prepare_cache_for_run(
            &catalog,
            cache_settings,
            Some(CacheRunIdentity {
                instance: uuid::Uuid::new_v4(),
                previous: None,
                current: uuid::Uuid::new_v4(),
            }),
        )
        .unwrap();
        let cache = persistent.full_storage(&account).unwrap().clone();
        cache
            .observe_folder(&account, "INBOX", 9, Some(uid_next), 1)
            .unwrap();
        CachedReader {
            cache,
            account,
            alias: "default".into(),
            config,
            persistent,
        }
    }

    #[tokio::test]
    async fn offline_search_matches_both_internal_date_formats_and_legacy_flags() {
        let directory = tempfile::tempdir().unwrap();
        let reader = reader_fixture(directory.path(), 5);
        let canonical = ["Seen", "Answered", "Flagged"];
        let legacy = [
            r#"Custom("\\sEeN")"#,
            r#"Custom("\\ANSWERED")"#,
            r#"Custom("\\fLaGgEd")"#,
        ];
        let custom = [r#"Custom("Seen")"#, r#"Custom("\\SeenOther")"#];
        let rows = [
            (
                1,
                Some(" 7-Feb-2025 23:30:00 -0800"),
                Some(canonical.as_slice()),
            ),
            (
                2,
                Some("2025-02-07 23:30:00 -08:00"),
                Some(legacy.as_slice()),
            ),
            (3, Some("2025-02-08 00:00:00 -08:00"), Some(&[][..])),
            (4, None, Some(custom.as_slice())),
            (5, None, None),
        ];
        for (uid, internal_date, flags) in rows {
            let reference = reader.reference("INBOX", 9, uid);
            reader
                .cache
                .remember_headers(reference, true, b"Subject: synthetic\r\n\r\n", None)
                .unwrap();
            let flags = flags.map(|flags| {
                flags
                    .iter()
                    .map(|flag| (*flag).to_owned())
                    .collect::<Vec<_>>()
            });
            reader
                .cache
                .remember_listing(
                    reference,
                    ListingEvidence {
                        flags: flags.as_deref(),
                        internal_date,
                        size: None,
                        observed_at_ms: 1,
                    },
                )
                .unwrap();
        }

        for (mut args, expected, unknown) in [
            (
                json!({"since":"2025-02-07","before":"2025-02-08"}),
                json!([2, 1]),
                2,
            ),
            (
                json!({"since":"2025-02-08","before":"2025-02-09"}),
                json!([3]),
                2,
            ),
            (
                json!({"seen":true,"answered":true,"flagged":true}),
                json!([2, 1]),
                1,
            ),
            (
                json!({"seen":false,"answered":false,"flagged":false}),
                json!([4, 3]),
                1,
            ),
        ] {
            args["uid_validity"] = json!(9);
            let page = reader.cached_query("search_emails", &args).unwrap();
            let uids = page["emails"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| &row["uid"])
                .collect::<Vec<_>>();
            assert_eq!(json!(uids), expected, "{args}: {page}");
            assert_eq!(
                page["cache"]["scopes"][0]["coverage"]["filter_unknown"], unknown,
                "{page}"
            );
        }
        reader.persistent.finish().await;
    }

    #[tokio::test]
    async fn offline_index_rebuild_rotates_past_storage_failures_and_retries() {
        let directory = tempfile::tempdir().unwrap();
        let reader = reader_fixture(directory.path(), 6);
        for uid in 1..=5 {
            let reference = reader.reference("INBOX", 9, uid);
            reader
                .cache
                .remember_headers(reference, true, b"Subject: synthetic\r\n\r\n", None)
                .unwrap();
            let mut record = cached_body::Record::default();
            record.push("searchable body".into(), false, 64);
            persistence::write_body(&reader.cache, reference, &record, None, &|| false).unwrap();
        }
        reader
            .cache
            .fixture_legacy_indexes_with_storage_fault(&reader.account, true);
        let search = || {
            reader
                .cached_query(
                    "search_emails",
                    &json!({"query":"searchable","uid_validity":9}),
                )
                .unwrap()
        };
        let pending = |page: &Value| {
            page["cache"]["scopes"][0]["coverage"]["search_index"]["rebuild_pending"] == true
        };
        let first = search();
        assert!(
            first["emails"].as_array().unwrap().is_empty() && pending(&first),
            "{first}"
        );
        let second = search();
        assert_eq!(second["emails"][0]["uid"], 1, "{second}");
        assert!(pending(&second), "{second}");
        reader
            .cache
            .fixture_legacy_indexes_with_storage_fault(&reader.account, false);
        let recovered = search();
        assert_eq!(
            recovered["emails"].as_array().unwrap().len(),
            5,
            "{recovered}"
        );
        assert!(!pending(&recovered), "{recovered}");
        reader.persistent.finish().await;
    }
}
