mod graph;
mod identity;
mod offline;
mod state;
mod store;

use super::*;
use graph::*;
use identity::{ContentFingerprint, ThreadFacts};
use parts::{body::BodyReadSession, fetch, reader};
use state::{Coverage, FolderCoverage, MAX_BODY_BYTES, MAX_PAGE_SECONDS, Options, State};
use std::time::Duration;
pub(super) use store::CursorStore;
use store::{Binding, Checkout, Cursor, Lease, StoreError};
use tokio::time::{Instant as Deadline, timeout_at};
use tokio_util::sync::CancellationToken;

const MAX_WINDOWS_PER_PAGE: usize = 256;
const MAX_PAGE_CONNECTIONS: usize = 8;
const MAX_FINGERPRINT_BYTES: usize = 256 * 1024;

pub(super) fn public_limits(config: &Config) -> Value {
    json!({
        "conversation_mailboxes":MAX_FOLDERS,
        "conversation_candidate_headers":state::MAX_CANDIDATES,
        "conversation_result_locations":MAX_RESULTS,
        "conversation_display_body_bytes":MAX_BODY_BYTES,
        "conversation_fingerprint_raw_bytes":MAX_FINGERPRINT_BYTES,
        "conversation_search_windows":MAX_WINDOWS_PER_PAGE,
        "conversation_search_window_uids":WINDOW_UIDS,
        "conversation_search_response_bytes":async_imap::types::CommandReadLimits::MAX_SEARCH_RESPONSE_BYTES,
        "conversation_ordinary_connections":MAX_PAGE_CONNECTIONS,
        "conversation_timeout_seconds":config.timeout.as_secs().min(MAX_PAGE_SECONDS),
    })
}

pub(crate) fn cross_requested(args: &Value) -> bool {
    ["cursor", "scope", "mailboxes"]
        .iter()
        .any(|key| args.get(key).is_some())
}

pub(crate) fn cached_request(args: &Value) -> Result<bool> {
    use super::read_source::Source;
    let requested = Source::parse("read_conversation", args)? == Source::Cache;
    let Some(cursor) = args["cursor"].as_str() else {
        return Ok(requested);
    };
    let cached = cursor.parse::<Cursor>().map_err(store_error)?.cached();
    if args.get("read_source").is_some() && requested != cached {
        return Err(MailError::input_field(crate::error::InputViolation::new(
            "/read_source",
            "conflicting_field",
            "remove_conflicting_field",
        )));
    }
    Ok(cached)
}

pub(super) const THREAD_INDEX_VERSION: u16 = 1;

pub(super) fn normalized_message_id(input: &str) -> Result<String> {
    identity::MessageId::parse(input.as_bytes())
        .map_err(|_| MailError::input())?
        .map(|id| id.into_boxed_str().into_string())
        .ok_or_else(MailError::input)
}

pub(super) fn thread_index_ids(
    headers: &[mailparse::MailHeader<'_>],
) -> Result<Vec<(crate::cache::ThreadRole, Box<str>)>> {
    use crate::cache::ThreadRole;
    let facts =
        ThreadFacts::parse(headers).map_err(|_| MailError::limit_kind(LimitKind::HeaderBytes))?;
    Ok(facts
        .id
        .into_iter()
        .map(|id| (ThreadRole::MessageId, id.into_boxed_str()))
        .chain(
            facts
                .references
                .into_iter()
                .map(|id| (ThreadRole::References, id.into_boxed_str())),
        )
        .chain(
            facts
                .in_reply_to
                .into_iter()
                .map(|id| (ThreadRole::InReplyTo, id.into_boxed_str())),
        )
        .collect())
}

impl MailService {
    pub(super) async fn cross_conversation(
        &self,
        store: &CursorStore,
        args: &Value,
        principal: &str,
        cancellation: &CancellationToken,
        fits: impl Fn(&Value) -> bool,
    ) -> Result<Value> {
        let cached = cached_request(args)?;
        let budget = self
            .config
            .timeout
            .min(Duration::from_secs(MAX_PAGE_SECONDS));
        let deadline = Deadline::now() + budget;
        let durable = cached || self.persistent.durable_account(&self.account);
        // Leave time for the durable commit after assembling the exact page.
        let network_deadline = if durable {
            deadline - (budget / 8).min(Duration::from_secs(1))
        } else {
            deadline
        };
        let namespace = self.account.reference_namespace();
        let binding = Binding::new(namespace, principal);
        let mut backend = if cached {
            PageBackend::Cache(Box::new(offline::CachedBackend::new(
                self,
                network_deadline,
            )?))
        } else {
            PageBackend::Live(Box::new(Backend::new(self, network_deadline)))
        };
        let (mut lease, replay) = if let Some(cursor) = args["cursor"].as_str() {
            match timed(
                deadline,
                store.checkout_async(
                    binding,
                    cursor.parse().map_err(store_error)?,
                    Deadline::now().into_std(),
                ),
            )
            .await?
            .map_err(store_error)?
            {
                Checkout::Advance(lease) => (lease, false),
                Checkout::Replay(lease) => (lease, true),
            }
        } else {
            let options = options(args)?;
            let (folders, seed, facts, stamps) = Box::pin(backend.freeze(args)).await?;
            let durable_scope = store.durable_scope(&namespace, &folders);
            if !durable_scope
                && (cached
                    || folders.iter().all(|folder| {
                        self.persistent
                            .configured_folder(&self.account, &folder.name)
                    }))
            {
                return Err(MailError::cursor_unavailable());
            }
            let mut capacity =
                unavailable_start(&self.alias, &folders, seed, cursor_storage(durable_scope))?;
            if let Some(stamps) = &stamps {
                offline::annotate(&mut capacity, &folders, stamps);
            } else {
                capacity["read_source"] = json!("live");
            }
            let lease = match store.open_lease(binding, Deadline::now().into_std(), |limit| {
                let mut state = State::new(namespace, folders, seed, options, facts, limit)?;
                if let Some(stamps) = stamps {
                    state.set_cache_stamps(stamps, limit)?;
                }
                Ok(state)
            }) {
                Ok(lease) => lease,
                Err(StoreError::Capacity) if fits(&capacity) => return Ok(capacity),
                Err(StoreError::Capacity) => return Err(wire_limit()),
                Err(error) => return Err(store_error(error)),
            };
            (lease, false)
        };
        if lease.state().cache_stamps().is_some() != cached {
            return Err(MailError::cursor_unavailable());
        }

        if args.get("cursor").is_some() {
            if !replay {
                lease
                    .update(|state, _| {
                        state.begin_page();
                        Ok(())
                    })
                    .map_err(store_error)?;
            }
            let folder_count = lease.state().folders().len();
            let start = if replay {
                lease.state().folder_probe_start()
            } else {
                lease
                    .update(|state, _| Ok(state.rotate_folder_probe()))
                    .map_err(store_error)?
            } as usize;
            let mut probe_order = (0..folder_count)
                .map(|offset| (start + offset) % folder_count)
                .collect::<Vec<_>>();
            if let Some(view) = lease.replay() {
                let mut required = [false; MAX_FOLDERS];
                for item in view.delivery.items() {
                    let event = lease.state().event(*item).ok_or_else(MailError::backend)?;
                    required[event.location.folder as usize] = true;
                    required[event.representative.folder as usize] = true;
                }
                probe_order.sort_by_key(|index| !required[*index]);
            }
            let per_folder = network_deadline.saturating_duration_since(Deadline::now())
                / (2 * folder_count as u32);
            let mut replay_failure = None;
            for index in probe_order {
                let folder = &lease.state().folders()[index];
                let Boundary::Frozen { uid_validity, .. } = folder.boundary else {
                    continue;
                };
                let checked = Box::pin(backend.probe(
                    lease.state(),
                    index as u8,
                    &folder.name,
                    uid_validity,
                    per_folder,
                ))
                .await;
                if let Err(error) = &checked
                    && invalidates_source(error, cached)
                {
                    let error = error.clone();
                    let _ = timeout_at(deadline, lease.invalidate_async()).await;
                    return Err(error);
                }
                if replay {
                    if let Err(error) = checked {
                        replay_failure.get_or_insert(error);
                    }
                    continue;
                }
                let index = index as u8;
                lease
                    .update(|state, _| match checked {
                        Ok(_) => state.folder_checked(index, uid_validity),
                        Err(ref error) => state.folder_failed(index, failure(error, false)),
                    })
                    .map_err(store_error)?;
            }
            if let Some(mut error) = replay_failure {
                error.retryable = true;
                return Err(error);
            }
        }

        let storage = cursor_storage(lease.durable_mode());
        if !lease.persistence_available() {
            return Err(MailError::cursor_unavailable());
        }
        let capacity_reached = if replay {
            false
        } else {
            match Box::pin(backend.scan(&mut lease)).await {
                Ok(capacity) => capacity,
                Err(error) => {
                    if invalidates_source(&error, cached) {
                        let _ = timeout_at(deadline, lease.invalidate_async()).await;
                    }
                    return Err(error);
                }
            }
        };
        let delivery = if let Some(view) = lease.replay() {
            if view
                .delivery
                .items()
                .iter()
                .any(|item| !backend.verified_event(lease.state(), *item))
            {
                return Err(MailError::backend());
            }
            view.delivery.clone()
        } else {
            lease.state().plan_delta_for_folders(backend.verified())
        };
        let original_wire_omitted = lease.replay().is_some_and(|view| view.wire_omitted);
        let bodies = if original_wire_omitted {
            Vec::new()
        } else {
            match Box::pin(backend.bodies(lease.state(), &delivery)).await {
                Ok(bodies) => bodies,
                Err(error) => {
                    let _ = timeout_at(deadline, lease.invalidate_async()).await;
                    return Err(error);
                }
            }
        };
        if let Some(view) = lease.replay() {
            if !lease.generation_matches() {
                let _ = timeout_at(deadline, lease.invalidate_async()).await;
                return Err(stale_uid());
            }
            if !lease.persistence_available() {
                return Err(MailError::cursor_unavailable());
            }
            let value = {
                let snapshot = Snapshot {
                    next: view.next,
                    coverage: view.coverage,
                    folder: &|index| view.folder_coverage(index),
                    errors: view.errors,
                    capacity_reached: view.capacity_reached,
                    wire_omitted: view.wire_omitted,
                    cursor_storage: storage,
                };
                let full = render(
                    &self.alias,
                    lease.state(),
                    &delivery,
                    &snapshot,
                    &bodies,
                    original_wire_omitted,
                )?;
                if fits(&full) {
                    full
                } else {
                    if original_wire_omitted || bodies.is_empty() {
                        return Err(wire_limit());
                    }
                    let lean = render(
                        &self.alias,
                        lease.state(),
                        &delivery,
                        &snapshot,
                        &bodies,
                        true,
                    )?;
                    if !fits(&lean) {
                        return Err(wire_limit());
                    }
                    lean
                }
            };
            if let PageBackend::Cache(backend) = &backend
                && let Err(error) = backend.verify_page(lease.state()).await
            {
                if invalidates_source(&error, true) {
                    let _ = timeout_at(deadline, lease.invalidate_async()).await;
                }
                return Err(error);
            }
            if cancellation.is_cancelled() {
                return Err(MailError::account_unavailable());
            }
            return Ok(value);
        }

        let mut delivery = delivery;
        let mut drop_bodies = false;
        loop {
            let prepared = lease
                .prepare_with_outcome(delivery.clone(), capacity_reached, drop_bodies)
                .map_err(store_error)?;
            let value = {
                let snapshot = Snapshot {
                    next: prepared.next(),
                    coverage: prepared.coverage(),
                    folder: &|index| prepared.folder_coverage(index),
                    errors: prepared.errors(),
                    capacity_reached: prepared.capacity_reached(),
                    wire_omitted: prepared.wire_omitted(),
                    cursor_storage: storage,
                };
                render(
                    &self.alias,
                    lease.state(),
                    &delivery,
                    &snapshot,
                    &bodies,
                    drop_bodies,
                )?
            };
            if fits(&value) {
                if let PageBackend::Cache(backend) = &backend
                    && let Err(error) = backend.verify_page(lease.state()).await
                {
                    if invalidates_source(&error, true) {
                        let _ = timeout_at(deadline, lease.invalidate_async()).await;
                    }
                    return Err(error);
                }
                if cancellation.is_cancelled() {
                    return Err(MailError::account_unavailable());
                }
                if !lease.persistence_available() {
                    return Err(MailError::cursor_unavailable());
                }
                let cache = self.persistent.clone();
                let account = self.account.clone();
                let commit = Box::pin(lease.commit_async(
                    prepared,
                    Deadline::now().into_std(),
                    cancellation.clone(),
                    move |state| offline::scopes_current(&cache, &account, state),
                ));
                let committed = if durable {
                    timed(deadline, commit).await?
                } else {
                    commit.await
                }
                .map_err(store_error)?;
                debug_assert_eq!(
                    committed.next.map(|cursor| cursor.to_string()),
                    value["cursor"].as_str().map(str::to_owned)
                );
                debug_assert_eq!(
                    committed.coverage.graph_complete,
                    value["coverage"]["graph_complete"] == true
                );
                return Ok(value);
            }
            if !drop_bodies && !bodies.is_empty() {
                drop_bodies = true;
            } else if delivery.items().len() > 1 {
                drop_bodies = true;
                delivery.truncate(delivery.items().len() - 1);
            } else {
                return Err(wire_limit());
            }
        }
    }
}

type FrozenPage = (Vec<Folder>, RefKey, ThreadFacts, Option<state::CacheStamps>);

enum PageBackend<'a> {
    Live(Box<Backend<'a>>),
    Cache(Box<offline::CachedBackend<'a>>),
}

impl PageBackend<'_> {
    async fn freeze(&mut self, args: &Value) -> Result<FrozenPage> {
        match self {
            Self::Live(backend) => {
                let (folders, seed, facts) = Box::pin(backend.freeze(args)).await?;
                Ok((folders, seed, facts, None))
            }
            Self::Cache(backend) => {
                let (folders, seed, facts, stamps) = Box::pin(backend.freeze(args)).await?;
                Ok((folders, seed, facts, Some(stamps)))
            }
        }
    }

    async fn probe(
        &mut self,
        state: &State,
        index: u8,
        mailbox: &str,
        validity: u32,
        budget: Duration,
    ) -> Result<()> {
        match self {
            Self::Live(backend) => Box::pin(backend.probe(index, mailbox, Some(validity), budget))
                .await
                .map(|_| ()),
            Self::Cache(backend) => Box::pin(backend.probe(state, index)).await,
        }
    }

    async fn scan(&mut self, lease: &mut Lease) -> Result<bool> {
        match self {
            Self::Live(backend) => Box::pin(backend.scan(lease)).await,
            Self::Cache(backend) => Box::pin(backend.scan(lease)).await,
        }
    }

    async fn bodies(&mut self, state: &State, delivery: &Delivery) -> Result<Vec<Body>> {
        match self {
            Self::Live(backend) => Box::pin(backend.bodies(state, delivery)).await,
            Self::Cache(backend) => Box::pin(backend.bodies(state, delivery)).await,
        }
    }

    fn verified(&self) -> [bool; MAX_FOLDERS] {
        match self {
            Self::Live(backend) => backend.verified,
            Self::Cache(backend) => backend.verified,
        }
    }

    fn verified_event(&self, state: &State, item: EmitKey) -> bool {
        let verified = self.verified();
        state.event(item).is_some_and(|event| {
            verified[event.location.folder as usize]
                && verified[event.representative.folder as usize]
        })
    }
}

fn invalidates_source(error: &MailError, cached: bool) -> bool {
    error.code == "STALE_UID"
        || (cached
            && matches!(
                error.code,
                "CURSOR_UNAVAILABLE" | "CACHE_NEEDS_VERIFICATION" | "CACHE_UNAVAILABLE"
            ))
}

fn options(args: &Value) -> Result<Options> {
    Ok(Options {
        candidate_limit: number(
            args,
            "candidate_limit",
            state::MAX_CANDIDATES as u64,
            1,
            state::MAX_CANDIDATES as u64,
        )? as usize,
        result_limit: number(args, "limit", MAX_RESULTS as u64, 1, MAX_RESULTS as u64)? as usize,
        max_body_length: args["max_body_length"].as_u64().map(|n| n as usize),
        trimmed: args["body_mode"] == "trimmed",
        readable_html: args["html_display"] == "readable",
    })
}

struct Backend<'a> {
    service: &'a MailService,
    reader: Option<BodyReadSession<'a>>,
    selected: Option<u8>,
    deadline: Deadline,
    remaining: Arc<AtomicUsize>,
    connections: usize,
    fingerprint_remaining: usize,
    verified: [bool; MAX_FOLDERS],
}

impl<'a> Backend<'a> {
    fn new(service: &'a MailService, deadline: Deadline) -> Self {
        Self {
            service,
            reader: None,
            selected: None,
            deadline,
            remaining: Arc::new(AtomicUsize::new(service.config.max_message * 4)),
            connections: 0,
            fingerprint_remaining: MAX_FINGERPRINT_BYTES,
            verified: [false; MAX_FOLDERS],
        }
    }

    async fn connect(&mut self) -> Result<&mut BodyReadSession<'a>> {
        if self
            .reader
            .as_mut()
            .is_none_or(|reader| reader.session().is_err())
        {
            if self.connections >= MAX_PAGE_CONNECTIONS {
                return Err(MailError::limit_kind(LimitKind::ScanItems));
            }
            self.connections += 1;
            let connection = timed(
                self.deadline,
                self.service.connect_for(true, Some(self.remaining.clone())),
            )
            .await??;
            if let Some(reader) = self.reader.as_mut() {
                reader.replace_connection(connection);
            } else {
                self.reader = Some(BodyReadSession::from_connection(
                    self.service,
                    connection,
                    Some(self.remaining.clone()),
                ));
            }
        }
        self.reader.as_mut().ok_or_else(MailError::backend)
    }

    fn disconnect(&mut self) {
        self.selected = None;
        if let Some(reader) = self.reader.as_mut() {
            reader.disconnect();
        }
    }

    async fn select(
        &mut self,
        index: u8,
        mailbox: &str,
        expected: Option<u32>,
    ) -> Result<(u32, u32)> {
        let deadline = self.deadline;
        let selected = self.selected == Some(index);
        let reader = self.connect().await?;
        if !selected {
            match timed(deadline, reader.examine_checked(mailbox)).await {
                Ok(Ok(())) => {}
                Ok(Err(fault)) => {
                    self.selected = None;
                    if !fault.reusable {
                        self.disconnect();
                    }
                    return Err(fault.error);
                }
                Err(error) => {
                    self.disconnect();
                    return Err(error);
                }
            }
        }
        let validity = reader.state.uid_validity.ok_or_else(MailError::backend)?;
        let high = reader.state.uid_next.and_then(|next| next.checked_sub(1));
        self.selected = Some(index);
        if expected.is_some_and(|expected| expected != validity) {
            return Err(stale_uid());
        }
        let high = high.ok_or_else(MailError::backend)?;
        self.verified[index as usize] = true;
        Ok((validity, high))
    }

    async fn probe(
        &mut self,
        index: u8,
        mailbox: &str,
        expected: Option<u32>,
        budget: Duration,
    ) -> Result<(u32, u32)> {
        let page = self.deadline;
        self.deadline = page.min(Deadline::now() + budget);
        let result = self.select(index, mailbox, expected).await;
        self.deadline = page;
        result
    }

    async fn freeze(&mut self, args: &Value) -> Result<(Vec<Folder>, RefKey, ThreadFacts)> {
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
        let expected = args["uid_validity"].as_u64().map(|v| v as u32);
        let (uid_validity, high_uid) = self.select(seed_index, seed_mailbox, expected).await?;
        if seed_uid > high_uid {
            return Err(missing());
        }
        let facts = self
            .facts(seed_mailbox, uid_validity, seed_uid)
            .await
            .map_err(|fault| fault.error)?;
        let facts = self
            .fingerprint(seed_mailbox, uid_validity, seed_uid, facts)
            .await?;
        let mut folders = Vec::with_capacity(names.len());
        let per_folder =
            self.deadline.saturating_duration_since(Deadline::now()) / (2 * names.len() as u32);
        for (index, name) in names.into_iter().enumerate() {
            let index = index as u8;
            let boundary = if index == seed_index {
                Boundary::Frozen {
                    uid_validity,
                    high_uid,
                }
            } else {
                match self.probe(index, &name, None, per_folder).await {
                    Ok((uid_validity, high_uid)) => Boundary::Frozen {
                        uid_validity,
                        high_uid,
                    },
                    Err(error) => Boundary::Unavailable(failure(&error, false)),
                }
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
        ))
    }

    async fn standard_folders(&mut self) -> Result<Vec<String>> {
        let deadline = self.deadline;
        let config = &self.service.config;
        let session = self.connect().await?.session()?;
        let listed = timed(deadline, async {
            let mut names = Vec::new();
            let mut seen = 0;
            let mut archive_excluded = false;
            let mut stream = session
                .list(Some(""), Some("*"))
                .await
                .map_err(map_read_imap_error)?;
            while let Some(item) = stream.try_next().await.map_err(map_read_imap_error)? {
                seen += 1;
                if seen > 2000 {
                    return Err(MailError::limit_kind(LimitKind::ScanItems));
                }
                let item = mailbox_metadata(&item, config)?;
                let name = item["name"].as_str().ok_or_else(MailError::backend)?;
                let roles = item["special_use"]
                    .as_array()
                    .ok_or_else(MailError::backend)?;
                let excluded = item["selectable"] == false
                    || roles.iter().any(|role| {
                        role.as_str()
                            .is_some_and(|role| matches!(role, "all" | "junk" | "trash" | "drafts"))
                    })
                    || name == config.drafts
                    || name == config.trash;
                if config.archive.as_deref() == Some(name) && excluded {
                    archive_excluded = true;
                }
                if !excluded
                    && roles.iter().any(|role| role == "sent")
                    && !name.eq_ignore_ascii_case("INBOX")
                {
                    push_mailbox(&mut names, name)?;
                }
            }
            names.sort_unstable();
            names.insert(0, "INBOX".into());
            if let Some(archive) = config.archive.as_deref().filter(|archive| {
                !archive_excluded && *archive != config.drafts && *archive != config.trash
            }) {
                push_mailbox(&mut names, archive)?;
            }
            if names.len() > MAX_FOLDERS {
                return Err(MailError::limit_kind(LimitKind::ScanItems));
            }
            Ok(names)
        })
        .await;
        listed.and_then(|result| result)
    }

    async fn facts(
        &mut self,
        mailbox: &str,
        validity: u32,
        uid: u32,
    ) -> std::result::Result<ThreadFacts, parts::body::ReadFault> {
        use parts::body::ReadFault;
        let reader = self
            .reader
            .as_mut()
            .ok_or_else(|| ReadFault::stop(MailError::backend()))?;
        let uid_next = reader.state.uid_next;
        let session = reader.session().map_err(ReadFault::stop)?;
        let result = timed(
            self.deadline,
            fetch::message_header(session, uid, insights::HEADER_LIMIT + 1),
        )
        .await;
        let raw = result.map_err(ReadFault::stop)?.map_err(|error| {
            let error =
                self.service
                    .observe_fetch_error(mailbox, Some(validity), uid_next, uid, error);
            if error.code == "NOT_FOUND" {
                ReadFault::local(error)
            } else {
                ReadFault::stop(error)
            }
        })?;
        if raw.len() > insights::HEADER_LIMIT {
            return Err(ReadFault::local(MailError::limit_with(
                LimitKind::HeaderBytes,
                insights::HEADER_LIMIT,
                raw.len(),
            )));
        }
        let headers = mime::headers(&raw).map_err(ReadFault::local)?;
        let facts = ThreadFacts::parse(&headers)
            .map_err(|_| ReadFault::local(MailError::limit_kind(LimitKind::HeaderBytes)))?;
        if self.service.persists(mailbox) {
            self.service.persist_headers(
                mailbox,
                Some(validity),
                vec![persistence::HeaderObservation::new(
                    uid, &raw, true, &headers,
                )],
            );
        }
        Ok(facts)
    }

    async fn fingerprint(
        &mut self,
        mailbox: &str,
        validity: u32,
        uid: u32,
        mut facts: ThreadFacts,
    ) -> Result<ThreadFacts> {
        if facts.id.is_none() || self.fingerprint_remaining == 0 || Deadline::now() >= self.deadline
        {
            return Ok(facts);
        }
        let now = Deadline::now();
        let evidence_deadline = now
            + (self.deadline - now)
                .div_f64(4.0)
                .min(Duration::from_secs(2));
        let reader = self.reader.as_mut().ok_or_else(MailError::backend)?;
        let uid_next = reader.state.uid_next;
        let session = reader.session()?;
        let max = self
            .fingerprint_remaining
            .min(self.service.config.max_message);
        let size = match timed(evidence_deadline, reader::size(session, uid)).await {
            Ok(Ok(size)) if size > 0 && size <= max => size,
            Ok(Ok(_)) => return Ok(facts),
            Ok(Err(error)) => {
                self.service.observe_fetch_error(
                    mailbox,
                    Some(validity),
                    uid_next,
                    uid,
                    error.mail(),
                );
                self.disconnect();
                return Ok(facts);
            }
            Err(_) => {
                self.disconnect();
                return Ok(facts);
            }
        };
        self.fingerprint_remaining -= size;
        let result = timed(
            evidence_deadline,
            fetch::whole_message(session, uid, size, size),
        )
        .await;
        let raw = match result {
            Ok(Ok(raw)) => raw,
            Ok(Err(error)) => {
                self.service
                    .observe_fetch_error(mailbox, Some(validity), uid_next, uid, error);
                self.disconnect();
                return Ok(facts);
            }
            Err(_) => {
                self.disconnect();
                return Ok(facts);
            }
        };
        let Ok(part) = mime::parse(&raw) else {
            return Ok(facts);
        };
        let observed = ThreadFacts::parse(&part.headers)
            .map_err(|_| MailError::limit_kind(LimitKind::HeaderBytes))?;
        if observed.evidence.headers != facts.evidence.headers
            || observed.id != facts.id
            || observed.references != facts.references
            || observed.in_reply_to != facts.in_reply_to
        {
            return Err(MailError::backend());
        }
        facts.evidence.content = Some(ContentFingerprint::raw_mime_body(part.encoded_body()));
        Ok(facts)
    }

    async fn scan(&mut self, lease: &mut Lease) -> Result<bool> {
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
            let index = match work {
                Work::Search(unit) => unit.folder,
                Work::Verify(candidate) => candidate.reference.folder,
            };
            let folder = &lease.state().folders()[index as usize];
            let Boundary::Frozen { uid_validity, .. } = folder.boundary else {
                return Err(MailError::backend());
            };
            let mailbox = folder.name.to_string();
            if let Err(error) = self.select(index, &mailbox, Some(uid_validity)).await {
                if error.code == "STALE_UID" {
                    return Err(error);
                }
                let error = failure(&error, false);
                lease
                    .update(|state, _| state.folder_failed(index, error))
                    .map_err(store_error)?;
                continue;
            }
            match work {
                Work::Search(unit) => {
                    windows += 1;
                    let anchor = lease
                        .state()
                        .message_id(unit.target)
                        .ok_or_else(MailError::backend)?
                        .search_anchor();
                    let query = format!(
                        "UID {}:{} HEADER {} \"{}\"",
                        unit.start,
                        unit.end,
                        unit.field.wire_name(),
                        anchor
                    );
                    let session = self
                        .reader
                        .as_mut()
                        .ok_or_else(MailError::backend)?
                        .session()?;
                    let result = timed(
                        self.deadline,
                        session.uid_search_bounded(query, WINDOW_UIDS),
                    )
                    .await;
                    let failed = match result {
                        Ok(Ok(hits)) => match lease
                            .update(|state, limit| state.apply_search(unit, &hits, limit))
                        {
                            Ok(()) => continue,
                            Err(StoreError::Capacity) => return Ok(true),
                            Err(StoreError::State(StateError::InvalidHits)) => Failure::Protocol,
                            Err(error) => return Err(store_error(error)),
                        },
                        Ok(Err(error)) if async_imap::error::search_limit(&error).is_some() => {
                            Failure::Limit
                        }
                        Ok(Err(error)) => failure(&map_read_imap_error(error), true),
                        Err(error) => failure(&error, false),
                    };
                    self.disconnect();
                    lease
                        .update(|state, _| state.search_failed(unit, failed))
                        .map_err(store_error)?;
                }
                Work::Verify(candidate) => {
                    candidates += 1;
                    let uid = candidate.reference.uid;
                    let facts = match self.facts(&mailbox, uid_validity, uid).await {
                        Ok(facts) => {
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
                                self.fingerprint(&mailbox, uid_validity, uid, facts).await
                            } else {
                                Ok(facts)
                            }
                        }
                        Err(fault) => {
                            if !fault.reusable {
                                self.disconnect();
                            }
                            Err(fault.error)
                        }
                    };
                    match facts {
                        Ok(facts) => match lease
                            .update(|state, limit| state.apply_facts(candidate, facts, limit))
                        {
                            Ok(_) => {}
                            Err(StoreError::Capacity) => return Ok(true),
                            Err(error) => return Err(store_error(error)),
                        },
                        Err(error) => {
                            let error = failure(&error, false);
                            lease
                                .update(|state, _| state.candidate_failed(candidate, error))
                                .map_err(store_error)?;
                        }
                    }
                }
            }
        }
        Ok(false)
    }

    async fn bodies(&mut self, state: &State, delivery: &Delivery) -> Result<Vec<Body>> {
        let mut bodies = Vec::new();
        let options = state.options();
        let mut args = json!({
            "body_mode":if options.trimmed {"trimmed"} else {"full"},
            "html_display":if options.readable_html {"readable"} else {"legacy"}
        });
        if let Some(limit) = options.max_body_length {
            args["max_body_length"] = json!(limit);
        }
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
            let reference = state.reference(key).ok_or_else(MailError::backend)?;
            let value = if remaining == 0 {
                Err(MailError::limit_kind(LimitKind::DecodedBodyBytes))
            } else {
                match self
                    .select(key.folder, reference.mailbox, Some(reference.uid_validity))
                    .await
                {
                    Ok(_) => {
                        let reader = self.reader.as_mut().ok_or_else(MailError::backend)?;
                        match timed(
                            self.deadline,
                            Box::pin(reader.read_item(key.uid, remaining.min(64 * 1024), &args)),
                        )
                        .await
                        {
                            Ok(Ok(mut value)) => {
                                insights::bound_conversation_metadata(&mut value);
                                remaining = remaining
                                    .saturating_sub(value["body"].as_str().map_or(0, str::len));
                                value["account_id"] = json!(self.service.alias);
                                value["mailbox"] = json!(reference.mailbox);
                                Ok(value)
                            }
                            Ok(Err(fault)) => {
                                if !fault.reusable {
                                    self.disconnect();
                                }
                                Err(fault.error)
                            }
                            Err(error) => {
                                self.disconnect();
                                Err(error)
                            }
                        }
                    }
                    Err(error) => Err(error),
                }
            };
            if value.as_ref().is_err_and(|error| error.code == "STALE_UID") {
                return Err(stale_uid());
            }
            bodies.push(Body { key, value });
        }
        Ok(bodies)
    }
}

struct Body {
    key: RefKey,
    value: Result<Value>,
}

struct Snapshot<'a> {
    next: Option<Cursor>,
    coverage: Coverage,
    folder: &'a dyn Fn(u8) -> Option<FolderCoverage>,
    errors: &'a state::ErrorSnapshot,
    capacity_reached: bool,
    wire_omitted: bool,
    cursor_storage: &'static str,
}

fn render(
    alias: &str,
    state: &State,
    delivery: &Delivery,
    snapshot: &Snapshot<'_>,
    bodies: &[Body],
    drop_bodies: bool,
) -> Result<Value> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let reference = |key| -> Result<Value> {
        let key = state.reference(key).ok_or_else(MailError::backend)?;
        Ok(
            json!({"account_id":alias,"mailbox":key.mailbox,"uid_validity":key.uid_validity,"uid":key.uid}),
        )
    };
    let mut messages: Vec<Value> = Vec::new();
    let mut omitted = Vec::new();
    let mut returned = 0;
    for item in delivery.items() {
        let event = state.event(*item).ok_or_else(MailError::backend)?;
        let entity_id = URL_SAFE_NO_PAD.encode(event.entity.0);
        let entity = messages.iter().position(|value| value["entity_id"] == entity_id).unwrap_or_else(|| {
            messages.push(json!({
                "entity_id":entity_id,
                "message_id":event.message_id.and_then(|target| state.message_id(target)).map(|id| id.as_str()),
                "identity_status":match event.status {
                    IdentityStatus::IdentityUnverified => "identity_unverified",
                    IdentityStatus::Verified => "verified",
                    IdentityStatus::Collision => "collision",
                },
                "new_entity":event.new_entity,
                "variants":[]
            }));
            messages.len() - 1
        });
        let variants = messages[entity]["variants"]
            .as_array_mut()
            .ok_or_else(MailError::backend)?;
        let variant_id = URL_SAFE_NO_PAD.encode(event.variant.0);
        let location = reference(event.location)?;
        if let Some(variant) = variants
            .iter_mut()
            .find(|value| value["variant_id"] == variant_id)
        {
            variant["locations"]
                .as_array_mut()
                .ok_or_else(MailError::backend)?
                .push(location);
            continue;
        }
        let representative = reference(event.representative)?;
        let mut variant = json!({"variant_id":variant_id,"representative":representative,"locations":[location],"new_variant":event.new_variant});
        if event.new_variant {
            if drop_bodies {
                omitted.push(representative);
            } else {
                match bodies
                    .iter()
                    .find(|body| body.key == event.representative)
                    .map(|body| &body.value)
                {
                    Some(Ok(value)) => {
                        variant["message"] = value.clone();
                        returned += 1;
                    }
                    result => {
                        omitted.push(representative);
                        if let Some(Err(error)) = result {
                            variant["body_error"] = json!(error);
                        }
                    }
                }
            }
        }
        variants.push(variant);
    }

    let coverage = snapshot.coverage;
    let folders = state
        .folders()
        .iter()
        .enumerate()
        .map(|(index, folder)| {
            render_folder(
                &folder.name,
                folder.boundary,
                (snapshot.folder)(index as u8),
            )
        })
        .collect::<Vec<_>>();
    let errors = snapshot.errors.entries().map(|error| {
        use state::CheckpointError::*;
        match error {
            Folder { folder, failure } => Ok(json!({"stage":"folder","mailbox":state.folders()[folder as usize].name,"error":public_failure(failure)})),
            Search { unit, failure } => Ok(json!({"stage":"search","mailbox":state.folders()[unit.folder as usize].name,"field":unit.field.wire_name(),
                "start_uid":unit.start,"end_uid":unit.end,"error":public_failure(failure)})),
            Candidate { candidate, failure } => Ok(json!({"stage":"header","ref":reference(candidate.reference)?,"error":public_failure(failure)})),
        }
    }).collect::<Result<Vec<_>>>()?;
    let mut value = json!({
        "mode":"cross_mailbox","account_id":alias,"seed":reference(state.seed())?,"folders":folders,
        "messages":messages,"cursor":snapshot.next.map(|cursor| cursor.to_string()),"cursor_storage":snapshot.cursor_storage,
        "coverage":render_coverage(coverage, returned + omitted.len(), returned),
        "errors":errors,"errors_omitted":snapshot.errors.omitted(),"omitted_refs":omitted,
        "complete":coverage.graph_complete && !coverage.delivery_pending,
        "response_budget_omitted":snapshot.wire_omitted,"content_trust":"untrusted",
    });
    if let Some(stamps) = state.cache_stamps() {
        offline::annotate(&mut value, state.folders(), stamps);
    } else {
        value["read_source"] = json!("live");
    }
    if snapshot.capacity_reached {
        value["cursor_action"] = json!("resume_or_restart_with_narrower_scope");
    } else if coverage.remaining_windows == 0
        && coverage.pending_candidates == 0
        && !coverage.delivery_pending
        && state
            .folders()
            .iter()
            .any(|folder| matches!(folder.boundary, Boundary::Unavailable(_)))
    {
        value["cursor_action"] = json!("restart_later_or_with_narrower_scope");
    }
    Ok(value)
}

fn render_folder(name: &str, boundary: Boundary, coverage: Option<FolderCoverage>) -> Value {
    let (uid_validity, high_uid, unavailable) = match boundary {
        Boundary::Frozen {
            uid_validity,
            high_uid,
        } => (Some(uid_validity), Some(high_uid), None),
        Boundary::Unavailable(error) => (None, None, Some(error)),
    };
    let coverage = coverage.unwrap_or(FolderCoverage {
        completed_windows: 0,
        remaining_windows: 0,
        pending_candidates: 0,
        unresolved_errors: usize::from(unavailable.is_some()),
        complete: false,
        failure: unavailable,
    });
    json!({"mailbox":name,"uid_validity":uid_validity,"high_uid":high_uid,
        "completed_windows":coverage.completed_windows,"remaining_windows":coverage.remaining_windows,
        "pending_candidates":coverage.pending_candidates,"unresolved_errors":coverage.unresolved_errors,
        "complete":coverage.complete,"error":coverage.failure.map(public_failure)})
}

fn render_coverage(coverage: Coverage, requested: usize, returned: usize) -> Value {
    json!({"completed_windows":coverage.completed_windows,"remaining_windows":coverage.remaining_windows,
        "pending_candidates":coverage.pending_candidates,"unresolved_errors":coverage.unresolved_errors,
        "known_locations":coverage.known_locations,"graph_complete":coverage.graph_complete,"delivery_pending":coverage.delivery_pending,
        "body_requested":requested,"body_returned":returned})
}

fn cursor_storage(durable: bool) -> &'static str {
    if durable { "persistent" } else { "memory" }
}

fn unavailable_start(
    alias: &str,
    folders: &[Folder],
    seed: RefKey,
    storage: &'static str,
) -> Result<Value> {
    let seed_folder = &folders[seed.folder as usize];
    let Boundary::Frozen { uid_validity, .. } = seed_folder.boundary else {
        return Err(MailError::backend());
    };
    let seed = json!({"account_id":alias,"mailbox":seed_folder.name,"uid_validity":uid_validity,"uid":seed.uid});
    let folders = folders
        .iter()
        .map(|folder| render_folder(&folder.name, folder.boundary, None))
        .collect::<Vec<_>>();
    let unavailable = folders
        .iter()
        .filter(|folder| folder["error"] != Value::Null)
        .count();
    let coverage = Coverage {
        completed_windows: 0,
        remaining_windows: 0,
        pending_candidates: 0,
        unresolved_errors: unavailable,
        known_locations: 1,
        graph_complete: false,
        delivery_pending: true,
    };
    Ok(json!({
        "mode":"cross_mailbox","account_id":alias,"seed":seed,"folders":folders,
        "messages":[],"cursor":null,"cursor_storage":storage,"coverage":render_coverage(coverage, 0, 0),
        "errors":[{"stage":"cursor","error":MailError::limit_kind(LimitKind::CacheQuota)}],
        "errors_omitted":0,"omitted_refs":[seed],"complete":false,"response_budget_omitted":false,
        "content_trust":"untrusted","cursor_action":"restart_later_or_with_narrower_scope",
    }))
}

fn canonical_inbox(mailbox: &str) -> &str {
    if mailbox.eq_ignore_ascii_case("INBOX") {
        "INBOX"
    } else {
        mailbox
    }
}

fn push_mailbox(names: &mut Vec<String>, name: &str) -> Result<()> {
    let name = canonical_inbox(name);
    if names.iter().any(|existing| existing == name) {
        return Ok(());
    }
    if name.is_empty() || name.chars().any(char::is_control) {
        return Err(MailError::input_field(crate::error::InputViolation::new(
            "/mailboxes",
            "forbidden_control_character",
            "remove_control_characters",
        )));
    }
    InputLimits::Read.mailbox(name, "/mailboxes")?;
    if names.len() == MAX_FOLDERS {
        return Err(MailError::limit_kind(LimitKind::ScanItems));
    }
    names.push(name.to_owned());
    Ok(())
}

async fn timed<T>(
    deadline: Deadline,
    operation: impl std::future::Future<Output = T>,
) -> Result<T> {
    if Deadline::now() >= deadline {
        return Err(MailError::timeout());
    }
    timeout_at(deadline, operation)
        .await
        .map_err(|_| MailError::timeout())
}

fn stale_uid() -> MailError {
    check_validity(&json!({"uid_validity":1}), Some(2)).expect_err("mismatched validity")
}

fn missing() -> MailError {
    MailError {
        code: "NOT_FOUND",
        message: "Message not found in the frozen mailbox.",
        retryable: false,
        details: None,
    }
}

fn wire_limit() -> MailError {
    MailError::limit_kind(LimitKind::WireResponseBytes)
}

fn store_error(error: StoreError) -> MailError {
    match error {
        StoreError::Unavailable => MailError::cursor_unavailable(),
        StoreError::Capacity => MailError::limit_kind(LimitKind::CacheQuota),
        StoreError::State(_) => MailError::backend(),
    }
}

fn failure(error: &MailError, protocol: bool) -> Failure {
    match error.code {
        "TIMEOUT" => Failure::Timeout,
        "LIMIT_EXCEEDED" => Failure::Limit,
        "NOT_FOUND" | "CACHE_MISS" => Failure::Missing,
        _ if protocol => Failure::Protocol,
        _ => Failure::Backend,
    }
}

fn public_failure(failure: Failure) -> MailError {
    match failure {
        Failure::Backend | Failure::Protocol => MailError::backend(),
        Failure::Timeout => MailError::timeout(),
        Failure::Limit => MailError::limit(),
        Failure::Missing => missing(),
    }
}
