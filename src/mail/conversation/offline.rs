//! Local-only backend for both conversation entry points.
use super::super::cache_read::{cache_context, cache_error, cache_miss, scope_info};
use super::*;
use crate::cache::{
    Access, ArtifactKey, ArtifactState, Cache, CacheError, ScopeSnapshot, ThreadRole,
};
use std::{collections::HashSet, io::Read, sync::Arc};

use super::state::{CacheStamp, CacheStamps, MAX_CANDIDATES};

pub(super) struct CachedBackend<'a> {
    service: &'a MailService,
    deadline: Deadline,
    fingerprint_remaining: usize,
    pub(super) verified: [bool; MAX_FOLDERS],
}

impl<'a> CachedBackend<'a> {
    pub(super) fn new(service: &'a MailService, deadline: Deadline) -> Result<Self> {
        service.cached_reader()?;
        Ok(Self {
            service,
            deadline,
            fingerprint_remaining: MAX_FINGERPRINT_BYTES,
            verified: [false; MAX_FOLDERS],
        })
    }

    async fn query<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Arc<Cache>, &AccountKey) -> std::result::Result<T, CacheError>
        + Send
        + 'static,
    ) -> Result<T> {
        timed(
            self.deadline,
            self.service.cache_job(move |reader| {
                operation(&reader.cache, &reader.account).map_err(cache_error)
            }),
        )
        .await?
    }

    fn current(&self, scope: &ScopeSnapshot, uid: Option<u32>) -> Result<()> {
        self.service
            .persistent
            .offline_current(
                &self.service.account,
                &scope.mailbox,
                scope.uid_validity,
                uid,
            )
            .then_some(())
            .ok_or_else(|| cache_error(CacheError::NeedsVerification))
    }

    async fn frozen(&self, mailbox: &str) -> Result<ScopeSnapshot> {
        let name = mailbox.to_owned();
        let snapshot = self
            .query(move |cache, account| cache.freeze_scope(account, &name))
            .await?;
        self.current(&snapshot, None)?;
        Ok(snapshot)
    }

    pub(super) async fn probe(&mut self, state: &State, index: u8) -> Result<()> {
        let scope = scope_snapshot(state, index).ok_or_else(MailError::cursor_unavailable)?;
        self.current(&scope, None)?;
        self.query(move |cache, account| cache.verify_scope(account, &scope))
            .await?;
        let scope = scope_snapshot(state, index).ok_or_else(MailError::cursor_unavailable)?;
        self.current(&scope, None)?;
        self.verified[index as usize] = true;
        Ok(())
    }

    pub(super) async fn verify_page(&self, state: &State) -> Result<()> {
        let scopes = scope_snapshots(state);
        for scope in &scopes {
            self.current(scope, None)?;
        }
        self.query(move |cache, account| {
            for scope in &scopes {
                cache.verify_scope(account, scope)?;
            }
            Ok(())
        })
        .await?;
        for scope in scope_snapshots(state) {
            self.current(&scope, None)?;
        }
        Ok(())
    }

    pub(super) async fn freeze(
        &mut self,
        args: &Value,
    ) -> Result<(Vec<Folder>, RefKey, ThreadFacts, CacheStamps)> {
        let seed_mailbox = canonical_inbox(folder(args, InputLimits::Read)?);
        let seed_uid = uid(args)?;
        let names = if let Some(names) = args["mailboxes"].as_array() {
            let mut folders = Vec::with_capacity(names.len());
            for name in names {
                push_mailbox(&mut folders, name.as_str().ok_or_else(MailError::input)?)?;
            }
            folders
        } else {
            self.standard_folders().await?
        };
        if names.iter().any(|name| {
            !self
                .service
                .persistent
                .configured_folder(&self.service.account, name)
        }) {
            return Err(MailError::storage());
        }
        let seed_index = names
            .iter()
            .position(|name| name == seed_mailbox)
            .ok_or_else(|| {
                MailError::input_field(crate::error::InputViolation::new(
                    "/mailbox",
                    "conflicting_field",
                    "remove_conflicting_field",
                ))
            })? as u8;
        let seed_scope = self.frozen(seed_mailbox).await?;
        check_validity(args, Some(seed_scope.uid_validity))?;
        let mut facts = self.facts(&seed_scope, seed_uid).await?;
        self.fingerprint(&seed_scope, seed_uid, &mut facts).await?;
        let mut folders = Vec::with_capacity(names.len());
        let mut stamps = [None; MAX_FOLDERS];
        for (index, name) in names.into_iter().enumerate() {
            let boundary = match if index as u8 == seed_index {
                Ok(seed_scope.clone())
            } else {
                self.frozen(&name).await
            } {
                Ok(scope) => {
                    stamps[index] = Some(stamp(&scope));
                    self.verified[index] = true;
                    Boundary::Frozen {
                        uid_validity: scope.uid_validity,
                        high_uid: scope.high_uid,
                    }
                }
                Err(error) => Boundary::Unavailable(failure(&error, false)),
            };
            folders.push(Folder {
                name: name.into_boxed_str(),
                boundary,
            });
        }
        Ok((
            folders,
            RefKey {
                folder: seed_index,
                uid: seed_uid,
            },
            facts,
            stamps,
        ))
    }

    async fn standard_folders(&self) -> Result<Vec<String>> {
        let cached = timed(self.deadline, self.service.cached_mailboxes()).await??;
        let listed = cached["mailboxes"]
            .as_array()
            .ok_or_else(MailError::storage)?;
        let mut names = Vec::new();
        if self
            .service
            .persistent
            .configured_folder(&self.service.account, "INBOX")
        {
            push_mailbox(&mut names, "INBOX")?;
        }
        let archive = self.service.config.archive.as_deref();
        let mut archive_excluded = false;
        for item in listed {
            let Some(name) = item["name"].as_str() else {
                continue;
            };
            let roles = item["special_use"].as_array();
            let excluded = item["selectable"] == false
                || roles.is_some_and(|roles| {
                    roles.iter().any(|role| {
                        role.as_str()
                            .is_some_and(|role| matches!(role, "all" | "junk" | "trash" | "drafts"))
                    })
                })
                || name == self.service.config.drafts
                || name == self.service.config.trash;
            if archive == Some(name) && excluded {
                archive_excluded = true;
            }
            if !excluded
                && roles.is_some_and(|roles| roles.iter().any(|role| role == "sent"))
                && self
                    .service
                    .persistent
                    .configured_folder(&self.service.account, name)
            {
                push_mailbox(&mut names, name)?;
            }
        }
        if let Some(archive) = archive.filter(|name| {
            !archive_excluded
                && *name != self.service.config.drafts
                && *name != self.service.config.trash
                && self
                    .service
                    .persistent
                    .configured_folder(&self.service.account, name)
        }) {
            push_mailbox(&mut names, archive)?;
        }
        if names.is_empty() {
            return Err(MailError::storage());
        }
        Ok(names)
    }

    async fn facts(&self, scope: &ScopeSnapshot, uid: u32) -> Result<ThreadFacts> {
        self.current(scope, Some(uid))?;
        if uid > scope.high_uid {
            return Err(cache_miss());
        }
        let owned = scope.clone();
        let raw = self
            .query(move |cache, account| cache.thread_header(account, &owned, uid))
            .await?
            .ok_or_else(cache_miss)?;
        self.current(scope, Some(uid))?;
        let headers = mime::headers(&raw)?;
        ThreadFacts::parse(&headers).map_err(|_| MailError::limit_kind(LimitKind::HeaderBytes))
    }

    async fn fingerprint(
        &mut self,
        scope: &ScopeSnapshot,
        uid: u32,
        facts: &mut ThreadFacts,
    ) -> Result<()> {
        if facts.id.is_none() || self.fingerprint_remaining == 0 {
            return Ok(());
        }
        self.current(scope, Some(uid))?;
        let mailbox = scope.mailbox.clone();
        let validity = scope.uid_validity;
        let limit = self
            .fingerprint_remaining
            .min(self.service.config.max_message);
        let owned = scope.clone();
        let result = self
            .query(move |cache, account| {
                cache.verify_scope(account, &owned)?;
                let reference = crate::cache::Ref {
                    account,
                    mailbox: &mailbox,
                    uid_validity: validity,
                    uid,
                };
                let ArtifactState::Materialized(info) =
                    cache.artifact_state(reference, ArtifactKey::Raw)?
                else {
                    return Ok(None);
                };
                if info.bytes > limit as u64 || info.bytes == 0 {
                    return Ok(None);
                }
                let bytes = info.bytes as usize;
                let raw = match cache.open_blob(
                    reference,
                    ArtifactKey::Raw,
                    Access::Offline,
                    Some(info.id),
                ) {
                    Ok(mut lease) => {
                        let mut raw = vec![0; bytes];
                        lease.read_exact(&mut raw).ok().map(|()| raw)
                    }
                    Err(_) => None,
                };
                cache.verify_scope(account, &owned)?;
                Ok(Some((bytes, raw)))
            })
            .await?;
        self.current(scope, Some(uid))?;
        let Some((charge, raw)) = result else {
            return Ok(());
        };
        self.fingerprint_remaining -= charge;
        let Some(raw) = raw else {
            return Ok(());
        };
        let Ok(part) = mime::parse(&raw) else {
            return Ok(());
        };
        let observed = ThreadFacts::parse(&part.headers)
            .map_err(|_| MailError::limit_kind(LimitKind::HeaderBytes))?;
        if observed.evidence.headers == facts.evidence.headers
            && observed.id == facts.id
            && observed.references == facts.references
            && observed.in_reply_to == facts.in_reply_to
        {
            facts.evidence.content = Some(ContentFingerprint::raw_mime_body(part.encoded_body()));
        }
        Ok(())
    }

    pub(super) async fn scan(&mut self, lease: &mut Lease) -> Result<bool> {
        let options = lease.state().options();
        let mut candidates = 0;
        let mut windows = 0;
        while candidates < options.candidate_limit
            && windows < MAX_WINDOWS_PER_PAGE
            && Deadline::now() < self.deadline
        {
            let Some(work) = lease.state().next_work() else {
                break;
            };
            match work {
                Work::Search(unit) => {
                    windows += 1;
                    let scope = scope_snapshot(lease.state(), unit.folder)
                        .ok_or_else(MailError::cursor_unavailable)?;
                    self.current(&scope, None)?;
                    let id = lease
                        .state()
                        .message_id(unit.target)
                        .ok_or_else(MailError::backend)?
                        .as_str()
                        .to_owned();
                    let role = role(unit.field);
                    let result = self
                        .query(move |cache, account| {
                            cache.thread_uids(account, &scope, role, &id, unit.start..=unit.end)
                        })
                        .await;
                    match result {
                        Ok(hits) => match lease
                            .update(|state, limit| state.apply_search(unit, &hits, limit))
                        {
                            Ok(()) => {}
                            Err(StoreError::Capacity) => return Ok(true),
                            Err(error) => return Err(store_error(error)),
                        },
                        Err(error) if invalidates_source(&error, true) => return Err(error),
                        Err(error) => lease
                            .update(|state, _| state.search_failed(unit, failure(&error, false)))
                            .map_err(store_error)?,
                    }
                }
                Work::Verify(candidate) => {
                    candidates += 1;
                    let uid = candidate.reference.uid;
                    let scope = scope_snapshot(lease.state(), candidate.reference.folder)
                        .ok_or_else(MailError::cursor_unavailable)?;
                    let result = match self.facts(&scope, uid).await {
                        Ok(mut facts) => {
                            let target = lease
                                .state()
                                .message_id(candidate.target)
                                .ok_or_else(MailError::backend)?;
                            let exact = match candidate.field {
                                SearchField::MessageId => facts.id.as_ref() == Some(target),
                                SearchField::References => facts.references.contains(target),
                                SearchField::InReplyTo => facts.in_reply_to.contains(target),
                            };
                            if exact {
                                self.fingerprint(&scope, uid, &mut facts)
                                    .await
                                    .map(|()| facts)
                            } else {
                                Ok(facts)
                            }
                        }
                        Err(error) => Err(error),
                    };
                    match result {
                        Ok(facts) => match lease
                            .update(|state, limit| state.apply_facts(candidate, facts, limit))
                        {
                            Ok(_) => {}
                            Err(StoreError::Capacity) => return Ok(true),
                            Err(error) => return Err(store_error(error)),
                        },
                        Err(error) if invalidates_source(&error, true) => return Err(error),
                        Err(error) => lease
                            .update(|state, _| {
                                state.candidate_failed(candidate, failure(&error, false))
                            })
                            .map_err(store_error)?,
                    }
                }
            }
        }
        Ok(false)
    }

    pub(super) async fn bodies(&self, state: &State, delivery: &Delivery) -> Result<Vec<Body>> {
        let options = state.options();
        let mut args = json!({
            "body_mode":if options.trimmed {"trimmed"} else {"full"},
            "html_display":if options.readable_html {"readable"} else {"legacy"}
        });
        if let Some(limit) = options.max_body_length {
            args["max_body_length"] = json!(limit);
        }
        let mut bodies = Vec::new();
        let mut remaining = MAX_BODY_BYTES;
        for key in delivery.items() {
            let event = state.event(*key).ok_or_else(MailError::backend)?;
            if !event.new_variant
                || bodies
                    .iter()
                    .any(|body: &Body| body.key == event.representative)
            {
                continue;
            }
            let key = event.representative;
            let scope =
                scope_snapshot(state, key.folder).ok_or_else(MailError::cursor_unavailable)?;
            let value = match self.body(&scope, key.uid, &args, &mut remaining).await {
                Err(error) if invalidates_source(&error, true) => return Err(error),
                value => value,
            };
            bodies.push(Body { key, value });
        }
        Ok(bodies)
    }

    async fn body(
        &self,
        scope: &ScopeSnapshot,
        uid: u32,
        args: &Value,
        remaining: &mut usize,
    ) -> Result<Value> {
        self.current(scope, Some(uid))?;
        if *remaining == 0 {
            return Err(MailError::limit_kind(LimitKind::DecodedBodyBytes));
        }
        let mut value = timed(
            self.deadline,
            Box::pin(self.service.cached_body_for_conversation(
                &scope.mailbox,
                scope.uid_validity,
                uid,
                args,
                (*remaining).min(64 * 1024),
            )),
        )
        .await??;
        self.current(scope, Some(uid))?;
        insights::bound_conversation_metadata(&mut value);
        *remaining = remaining.saturating_sub(value["body"].as_str().map_or(0, str::len));
        value["account_id"] = json!(self.service.alias);
        value["mailbox"] = json!(scope.mailbox);
        Ok(value)
    }
}

pub(super) fn role(field: SearchField) -> ThreadRole {
    match field {
        SearchField::MessageId => ThreadRole::MessageId,
        SearchField::References => ThreadRole::References,
        SearchField::InReplyTo => ThreadRole::InReplyTo,
    }
}

fn stamp(scope: &ScopeSnapshot) -> CacheStamp {
    CacheStamp {
        checked_at_ms: scope.checked_at_ms,
        index_revision: scope.index_revision,
        dirty_revision: scope.dirty_revision,
        restore_epoch: scope.restore_epoch,
        complete_headers: scope.complete_headers,
        indexed_threads: scope.indexed_threads,
        known_messages: scope.known_messages,
    }
}

pub(super) fn scope_snapshot(state: &State, index: u8) -> Option<ScopeSnapshot> {
    let index = index as usize;
    let folder = state.folders().get(index)?;
    let Boundary::Frozen {
        uid_validity,
        high_uid,
    } = folder.boundary
    else {
        return None;
    };
    let stamp = state.cache_stamps()?[index]?;
    Some(ScopeSnapshot {
        mailbox: folder.name.to_string(),
        uid_validity,
        high_uid,
        account_namespace: *state.namespace(),
        checked_at_ms: stamp.checked_at_ms,
        index_revision: stamp.index_revision,
        dirty_revision: stamp.dirty_revision,
        restore_epoch: stamp.restore_epoch,
        complete_headers: stamp.complete_headers,
        indexed_threads: stamp.indexed_threads,
        known_messages: stamp.known_messages,
    })
}

pub(super) fn scope_snapshots(state: &State) -> Vec<ScopeSnapshot> {
    (0..state.folders().len() as u8)
        .filter_map(|index| scope_snapshot(state, index))
        .collect()
}

pub(super) fn scopes_current(cache: &PreparedCache, account: &AccountKey, state: &State) -> bool {
    state.folders().iter().all(|folder| match folder.boundary {
        Boundary::Frozen { uid_validity, .. } => {
            cache.offline_current(account, &folder.name, uid_validity, None)
        }
        Boundary::Unavailable(_) => true,
    })
}

pub(super) fn annotate(value: &mut Value, folders: &[Folder], stamps: &CacheStamps) {
    let scopes = folders
        .iter()
        .zip(stamps)
        .filter_map(|(folder, stamp)| {
            let Boundary::Frozen {
                uid_validity,
                high_uid,
            } = folder.boundary
            else {
                return None;
            };
            let stamp = (*stamp)?;
            Some(scope_info(&ScopeSnapshot {
                mailbox: folder.name.to_string(),
                uid_validity,
                high_uid,
                account_namespace: [0; 32],
                checked_at_ms: stamp.checked_at_ms,
                index_revision: stamp.index_revision,
                dirty_revision: stamp.dirty_revision,
                restore_epoch: stamp.restore_epoch,
                complete_headers: stamp.complete_headers,
                indexed_threads: stamp.indexed_threads,
                known_messages: stamp.known_messages,
            }))
        })
        .collect();
    value["read_source"] = json!("cache");
    value["cache"] = cache_context(scopes);
    // The local graph can be exhausted without evidence covering all remote
    // history. R12 supplies that independent scope-wide evidence.
    value["complete"] = json!(false);
}

impl MailService {
    pub(in crate::mail) async fn cached_legacy_conversation(
        &self,
        args: &Value,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        let deadline = Deadline::now()
            + self
                .config
                .timeout
                .min(Duration::from_secs(MAX_PAGE_SECONDS));
        let backend = CachedBackend::new(self, deadline)?;
        let mailbox = canonical_inbox(folder(args, InputLimits::Read)?);
        let seed = uid(args)?;
        let candidate_limit =
            number(args, "candidate_limit", 100, 1, MAX_CANDIDATES as u64)? as usize;
        let limit = number(args, "limit", 10, 1, MAX_RESULTS as u64)? as usize;
        let scope = backend.frozen(mailbox).await?;
        check_validity(args, Some(scope.uid_validity))?;
        let seed_facts = backend.facts(&scope, seed).await?;
        let end = if args.get("before_uid").is_some() {
            number(args, "before_uid", 1, 1, u32::MAX as u64)? as u32 - 1
        } else {
            scope.high_uid
        };
        let start = end.saturating_sub(499).max(1);
        let known_end = end.min(scope.high_uid);
        let mut candidates = if known_end < start {
            Vec::new()
        } else {
            let snapshot = scope.clone();
            backend
                .query(move |cache, account| {
                    cache.scope_uids(account, &snapshot, start..=known_end, candidate_limit + 2)
                })
                .await?
        };
        candidates.retain(|id| *id != seed);
        let mut limited = candidates.len() > candidate_limit;
        candidates.truncate(candidate_limit);
        let mut facts = Vec::new();
        let mut skipped = Vec::new();
        let mut visited = None;
        let mut header_bytes = identifier_bytes(&seed_facts);
        for uid in candidates {
            match backend.facts(&scope, uid).await {
                Ok(value) => {
                    header_bytes = header_bytes.saturating_add(identifier_bytes(&value));
                    if header_bytes > 8 * 1024 * 1024 {
                        limited = true;
                        break;
                    }
                    facts.push((uid, value));
                }
                Err(error) if invalidates_source(&error, true) => return Err(error),
                Err(error) => skipped.push(json!({"uid":uid,"error":error})),
            }
            visited = Some(uid);
        }
        let next = if limited {
            visited
        } else {
            (start > 1).then_some(start)
        };
        let connected = connected(seed, &seed_facts, &facts);
        let total_budget = (self.config.max_output / 4).min(MAX_BODY_BYTES);
        let mut remaining = total_budget;
        let mut output_remaining = self.config.max_output.saturating_sub(4096);
        let mut messages = Vec::new();
        let mut message_errors = Vec::new();
        let mut wire_omitted = false;
        for uid in connected.iter().copied().take(limit) {
            let value = match backend.body(&scope, uid, args, &mut remaining).await {
                Ok(value) => value,
                Err(error) if invalidates_source(&error, true) => return Err(error),
                Err(error) => {
                    message_errors.push(json!({"uid":uid,"error":error}));
                    continue;
                }
            };
            let bytes = serde_json::to_vec(&value)
                .map_err(|_| MailError::backend())?
                .len()
                + 1;
            if bytes > output_remaining {
                wire_omitted = true;
                break;
            }
            output_remaining -= bytes;
            messages.push(value);
        }
        let omitted = connected
            .iter()
            .filter(|id| {
                !messages
                    .iter()
                    .any(|message| message["uid"].as_u64() == Some(u64::from(**id)))
            })
            .collect::<Vec<_>>();
        let checked = scope.clone();
        backend
            .query(move |cache, account| cache.verify_scope(account, &checked))
            .await?;
        backend.current(&scope, None)?;
        if cancellation.is_cancelled() {
            return Err(MailError::account_unavailable());
        }
        Ok(json!({
            "mailbox":mailbox,"uid_validity":scope.uid_validity,
            "messages":messages,"omitted_uids":omitted,
            "threading":"Message-ID/References/In-Reply-To exact identifier graph; no subject matching",
            "scope":{"start_uid":start,"end_uid":end,"candidates_checked":facts.len(),"seed_uid":seed},
            "next_before_uid":next,"candidate_limit_reached":limited,"messages_omitted":!omitted.is_empty(),
            "response_budget_omitted":wire_omitted,"older_uids_omitted":start>1,
            "total_body_byte_limit":total_budget,"skipped_candidates":skipped,"message_errors":message_errors,
            "complete":false,"content_trust":"untrusted","read_source":"cache",
            "cache":cache_context(vec![scope_info(&scope)]),
        }))
    }
}

fn identifiers(facts: &ThreadFacts) -> impl Iterator<Item = &identity::MessageId> {
    facts
        .id
        .iter()
        .chain(&facts.references)
        .chain(&facts.in_reply_to)
}

fn identifier_bytes(facts: &ThreadFacts) -> usize {
    identifiers(facts)
        .map(|id| id.as_str().len())
        .sum::<usize>()
        + (facts.references.capacity() + facts.in_reply_to.capacity())
            * std::mem::size_of::<identity::MessageId>()
        + std::mem::size_of::<(u32, ThreadFacts)>()
}

fn connected(seed: u32, facts: &ThreadFacts, candidates: &[(u32, ThreadFacts)]) -> Vec<u32> {
    let mut targets: HashSet<_> = identifiers(facts).collect();
    let mut selected = vec![seed];
    loop {
        let before = selected.len();
        for (uid, facts) in candidates {
            if !selected.contains(uid) && identifiers(facts).any(|id| targets.contains(id)) {
                targets.extend(identifiers(facts));
                selected.push(*uid);
            }
        }
        if selected.len() == before {
            selected[1..].sort_unstable();
            return selected;
        }
    }
}
