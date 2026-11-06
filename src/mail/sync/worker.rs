//! One foreground-yielding supervisor for administrator-selected cache scopes.
use super::{content, delta, reconcile, scan};
use crate::{
    accounts::{AccountKey, AccountLease},
    cache::{Cache, CacheError, sync_state::*},
    config::{SyncPolicy, SyncProtocol},
    error::MailError,
    mail::{
        MailRuntime, MailService, PreparedCache, Session,
        persistence::{DeltaInvalidation, checked_at_ms},
        runtime,
        scheduler::WorkClass,
    },
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, Weak, atomic::AtomicUsize},
    time::Duration,
};
use tokio::{task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;

mod condstore;
mod qresync;

const PAGE: usize = 200;
const BACKOFF: Duration = Duration::from_secs(5);
const QUOTA_WAIT: Duration = Duration::from_secs(60);
const CLEAN_WAIT: Duration = Duration::from_secs(10);

pub(in crate::mail) struct Supervisor {
    cache: PreparedCache,
    stop: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
    health: HealthMap,
}

type HealthMap = Arc<Mutex<HashMap<(AccountKey, String), Health>>>;

#[derive(Clone, Copy, Default, serde::Serialize)]
struct Health {
    last_error: Option<Failure>,
    quota_paused: bool,
    #[serde(flatten)]
    negotiated: ProtocolHealth,
}

#[derive(Clone, Copy, Default, serde::Serialize)]
struct ProtocolHealth {
    protocol: ActiveProtocol,
    protocol_fallback: Option<ProtocolFallback>,
}

#[derive(Clone, Copy, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ActiveProtocol {
    #[default]
    Rev1,
    Condstore,
    Qresync,
}

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ProtocolFallback {
    CapabilityUnavailable,
    ExtensionFailed,
    Nomodseq,
    ModseqRollback,
    QresyncEnableFailed,
    QresyncFailed,
    QresyncReconciliationRequired,
}

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Failure {
    Network,
    Protocol,
    Storage,
    Quota,
}

impl Supervisor {
    pub(in crate::mail) fn new(cache: PreparedCache) -> Self {
        Self {
            cache,
            stop: CancellationToken::new(),
            task: Mutex::default(),
            health: Arc::default(),
        }
    }

    pub(in crate::mail) fn start(&self, runtime: &Arc<MailRuntime>) {
        let Some(policy) = self.cache.sync_policy().filter(|policy| policy.enabled) else {
            return;
        };
        let Ok(mut task) = self.task.lock() else {
            return;
        };
        if task.is_some() || self.stop.is_cancelled() {
            return;
        }
        let schedule = Schedule::new(runtime, &self.cache);
        *task = Some(tokio::spawn(run(
            Arc::downgrade(runtime),
            self.stop.clone(),
            self.health.clone(),
            policy,
            schedule,
        )));
    }

    pub(in crate::mail) fn stop(&self) {
        self.stop.cancel();
    }

    pub(in crate::mail) async fn finish(&self) -> bool {
        self.stop();
        let Ok(mut task) = self.task.lock().map(|mut task| task.take()) else {
            return false;
        };
        match task.as_mut() {
            Some(task) => tokio::time::timeout(CLEAN_WAIT, task)
                .await
                .is_ok_and(|result| result.is_ok()),
            None => true,
        }
    }

    pub(in crate::mail) fn add_health(&self, account: &AccountKey, value: &mut Value) {
        let Some(sync) = value.get_mut("sync") else {
            return;
        };
        sync["running"] = json!(
            !self.stop.is_cancelled()
                && self
                    .task
                    .lock()
                    .is_ok_and(|task| task.as_ref().is_some_and(|task| !task.is_finished()))
        );
        let Some(folders) = sync["folders"].as_array_mut() else {
            return;
        };
        let Ok(health) = self.health.lock() else {
            return;
        };
        for folder in folders {
            let Some(mailbox) = folder["mailbox"].as_str() else {
                continue;
            };
            folder["runtime"] = json!(
                health
                    .get(&(account.clone(), mailbox.into()))
                    .copied()
                    .unwrap_or_default()
            );
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone, Copy)]
enum Phase {
    History,
    Tail,
    Reconcile,
    Delta,
    Content,
    Maintain,
}

const PHASES: [Phase; 6] = [
    Phase::History,
    Phase::Tail,
    Phase::Reconcile,
    Phase::Delta,
    Phase::Content,
    Phase::Maintain,
];

struct Folder {
    name: String,
    due: [Instant; PHASES.len()],
    next: usize,
    health: Health,
    reclaim: Option<SyncReclaimPosition>,
    reclaim_active: bool,
    quota: QuotaMode,
    probe_after: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QuotaMode {
    Startup,
    Normal,
    Paused,
    Recovering,
}

impl Folder {
    fn may_probe(&self, policy: SyncPolicy) -> bool {
        policy.protocol == SyncProtocol::Auto && Instant::now() >= self.probe_after
    }

    fn new(name: &str, now: Instant) -> Self {
        Self {
            name: name.into(),
            due: [now; PHASES.len()],
            next: 0,
            health: Health::default(),
            reclaim: None,
            reclaim_active: false,
            quota: QuotaMode::Startup,
            probe_after: now,
        }
    }

    fn quota_blocked(&mut self, now: Instant) {
        if self.quota != QuotaMode::Paused {
            self.reclaim_active = true;
            self.reclaim = None;
            self.due[Phase::Maintain as usize] = now;
        }
        self.quota = QuotaMode::Paused;
    }
}

struct Account {
    key: AccountKey,
    alias: String,
    folders: Vec<Folder>,
    next: usize,
    gc: Option<SyncGcPosition>,
}

struct Schedule(VecDeque<Account>);

impl Schedule {
    fn new(runtime: &MailRuntime, cache: &PreparedCache) -> Self {
        let mut accounts = VecDeque::new();
        for lease in runtime.catalog.registered_accounts() {
            let mut names = cache.configured_folders(&lease.key).collect::<Vec<_>>();
            names.sort_unstable();
            if names.is_empty() {
                continue;
            }
            let now = Instant::now();
            let folders = names
                .into_iter()
                .map(|name| Folder::new(name, now))
                .collect();
            accounts.push_back(Account {
                key: lease.key,
                alias: lease.alias,
                folders,
                next: 0,
                gc: None,
            });
        }
        Self(accounts)
    }

    fn ready(&mut self, now: Instant) -> Option<(usize, usize)> {
        for _ in 0..self.0.len() {
            self.0.rotate_left(1);
            let account = self.0.back_mut()?;
            for _ in 0..account.folders.len() {
                let index = account.next;
                account.next = (account.next + 1) % account.folders.len();
                let folder = &mut account.folders[index];
                for _ in PHASES {
                    let phase = folder.next;
                    folder.next = (phase + 1) % PHASES.len();
                    if folder.due[phase] <= now {
                        return Some((index, phase));
                    }
                }
            }
        }
        None
    }

    fn next_due(&self) -> Option<Instant> {
        self.0
            .iter()
            .flat_map(|account| &account.folders)
            .flat_map(|folder| folder.due)
            .min()
    }

    fn release_space(&mut self, now: Instant) {
        // A physical release can also help another folder or account that
        // was blocked on the shared global budget.
        for folder in self.0.iter_mut().flat_map(|account| &mut account.folders) {
            if folder.quota == QuotaMode::Paused {
                folder.quota = QuotaMode::Recovering;
                folder.reclaim_active = false;
                folder.due[Phase::Content as usize] = now;
            }
        }
    }
}

enum Turn {
    More,
    Idle,
    Deferred,
    Failed(Failure),
    Retry(Failure),
    Maintained {
        next: Option<SyncReclaimPosition>,
        gc: Option<SyncGcPosition>,
        released: bool,
    },
}

async fn run(
    runtime: Weak<MailRuntime>,
    stop: CancellationToken,
    health: HealthMap,
    policy: SyncPolicy,
    mut schedule: Schedule,
) {
    while !stop.is_cancelled() {
        let Some((folder, phase)) = schedule.ready(Instant::now()) else {
            let Some(due) = schedule.next_due() else {
                break;
            };
            tokio::select! { _ = stop.cancelled() => break, _ = tokio::time::sleep_until(due) => {} }
            continue;
        };
        let Some(runtime) = runtime.upgrade() else {
            break;
        };
        let Some(account) = schedule.0.back_mut() else {
            break;
        };
        let kind = PHASES[phase];
        let (turn, negotiated) =
            Box::pin(execute(&runtime, &stop, account, folder, kind, policy)).await;
        let folder = &mut account.folders[folder];
        let interval = Duration::from_secs(policy.interval_seconds);
        if let Some(negotiated) = negotiated {
            folder.health.negotiated = negotiated;
            if negotiated.protocol_fallback.is_some() {
                folder.probe_after = Instant::now() + interval.max(BACKOFF);
                if matches!(kind, Phase::Delta) && matches!(turn, Turn::More) {
                    folder.due[Phase::Reconcile as usize] = Instant::now();
                }
            }
        }
        let released = matches!(turn, Turn::Maintained { released: true, .. });
        if matches!(kind, Phase::Content)
            && folder.quota == QuotaMode::Recovering
            && matches!(turn, Turn::Idle)
        {
            folder.quota = QuotaMode::Normal;
        }
        let wait = match turn {
            Turn::More => Duration::ZERO,
            Turn::Idle if matches!(kind, Phase::Reconcile) => {
                Duration::from_secs(policy.reconcile_interval_seconds)
            }
            Turn::Idle => interval,
            Turn::Deferred => interval.min(BACKOFF),
            Turn::Maintained { next, gc, .. } => {
                folder.reclaim = next;
                folder.reclaim_active = next.is_some();
                account.gc = gc;
                if gc.is_some() {
                    Duration::from_millis(200)
                } else {
                    interval
                }
            }
            Turn::Retry(failure) => {
                folder.health.last_error = Some(failure);
                Duration::ZERO
            }
            Turn::Failed(failure) => {
                folder.health.last_error = Some(failure);
                if matches!(failure, Failure::Quota) {
                    folder.quota_blocked(Instant::now());
                    QUOTA_WAIT
                } else {
                    interval.max(BACKOFF)
                }
            }
        };
        folder.due[phase] = Instant::now() + wait;
        folder.health.quota_paused =
            matches!(folder.quota, QuotaMode::Paused | QuotaMode::Recovering);
        if let Ok(mut state) = health.lock() {
            state.insert((account.key.clone(), folder.name.clone()), folder.health);
        }
        if released {
            schedule.release_space(Instant::now());
        }
        drop(runtime);
        tokio::task::yield_now().await;
    }
}

async fn execute(
    runtime: &MailRuntime,
    stop: &CancellationToken,
    account: &Account,
    folder: usize,
    phase: Phase,
    policy: SyncPolicy,
) -> (Turn, Option<ProtocolHealth>) {
    let key = &account.key;
    let folder = &account.folders[folder];
    if matches!(phase, Phase::Delta) && !folder.may_probe(policy) {
        return (Turn::Idle, None);
    }
    let Some(lease) = runtime.catalog.sync_account(&account.alias, key) else {
        return (Turn::Idle, None);
    };
    let Ok(service) = runtime.service_for(key) else {
        return (Turn::Idle, None);
    };
    let Some(cache) = service.persistent.full_folder(key, &folder.name) else {
        return (Turn::Idle, None);
    };
    let cancellation = lease.cancellation.child_token();
    let permit = tokio::select! {
        biased;
        _ = stop.cancelled() => return (Turn::Deferred, None),
        permit = runtime.scheduler.acquire(key.reference_namespace(), WorkClass::Background, &cancellation) => {
            let Ok(permit) = permit else { return (Turn::Deferred, None) };
            Arc::new(permit)
        }
    };
    let batch = Batch {
        runtime,
        lease,
        service,
        cache: cache.clone(),
        mailbox: folder.name.clone(),
        cancellation,
        transport: Arc::new(AtomicUsize::new(service.config.max_message * 4)),
        connections: AtomicUsize::new(0),
        deadline: Instant::now() + service.config.timeout,
        negotiated: Mutex::default(),
    };
    let run = runtime::READ_ADMISSION.scope(permit.clone(), async {
        tokio::select! {
            biased;
            _ = stop.cancelled() => Turn::Deferred,
            _ = batch.cancellation.cancelled() => Turn::Deferred,
            step = tokio::time::timeout_at(batch.deadline, Box::pin(batch.run(folder, phase, policy, account.gc))) => {
                match step {
                    Ok(Ok(turn)) => turn,
                    Ok(Err(error)) => error.turn(),
                    Err(_) => Turn::Failed(Failure::Network),
                }
            }
        }
    }).await;
    batch.cancellation.cancel();
    // On timeout/cancellation a detached blocking operation or stage cleanup
    // still owns the same permit. Let it settle before advancing the supervisor.
    while Arc::strong_count(&permit) > 1 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    (run, batch.negotiated.into_inner().unwrap_or_default())
}

enum Error {
    Mail(MailError),
    Sync(SyncError),
    Deferred,
}
type Result<T> = std::result::Result<T, Error>;

impl From<MailError> for Error {
    fn from(error: MailError) -> Self {
        Self::Mail(error)
    }
}
impl From<SyncError> for Error {
    fn from(error: SyncError) -> Self {
        Self::Sync(error)
    }
}
impl From<CacheError> for Error {
    fn from(error: CacheError) -> Self {
        Self::Sync(error.into())
    }
}

impl Error {
    fn turn(self) -> Turn {
        match self {
            Self::Deferred
            | Self::Sync(
                SyncError::Stale
                | SyncError::Cancelled
                | SyncError::NotReady
                | SyncError::ClaimActive
                | SyncError::ReconcileRequired,
            ) => Turn::Deferred,
            Self::Mail(error) if matches!(error.code, "BUSY" | "TIMEOUT") => {
                Turn::Failed(Failure::Network)
            }
            Self::Mail(_) | Self::Sync(SyncError::Invalid) => Turn::Failed(Failure::Protocol),
            Self::Sync(SyncError::Cache(CacheError::Quota)) => Turn::Failed(Failure::Quota),
            Self::Sync(SyncError::Cache(_)) => Turn::Failed(Failure::Storage),
        }
    }
}

struct Batch<'a> {
    runtime: &'a MailRuntime,
    lease: AccountLease,
    service: &'a MailService,
    cache: Arc<Cache>,
    mailbox: String,
    cancellation: CancellationToken,
    transport: Arc<AtomicUsize>,
    connections: AtomicUsize,
    deadline: Instant,
    negotiated: Mutex<Option<ProtocolHealth>>,
}

#[derive(Clone)]
struct Scope {
    cache: Arc<Cache>,
    account: AccountKey,
    mailbox: String,
    persistent: PreparedCache,
    cancellation: CancellationToken,
}

impl Scope {
    fn current(&self, validity: u32) -> bool {
        !self.cancellation.is_cancelled()
            && self
                .persistent
                .sync_current(&self.account, &self.mailbox, validity, None)
    }

    fn start(&self, kind: ScanKind, observation: MailboxObservation) -> Result<ScanState> {
        let scan = self
            .cache
            .start_sync(&self.account, &self.mailbox, kind, observation)?;
        self.persistent
            .sync_commit_epoch(&self.account, &self.mailbox, observation.uid_validity);
        Ok(scan)
    }

    fn observe_delta(&self, observation: DeltaObservation) -> Result<()> {
        if !self.current(observation.uid_validity) {
            return Err(Error::Deferred);
        }
        self.cache.observe_folder(
            &self.account,
            &self.mailbox,
            observation.uid_validity,
            Some(observation.uid_next - 1),
            observation.checked_at_ms,
        )?;
        self.persistent
            .sync_commit_epoch(&self.account, &self.mailbox, observation.uid_validity);
        Ok(())
    }

    fn apply_delta_invalidation(
        &self,
        stamp: &ScanStamp,
        fence: &ReconcileFence,
        invalidation: DeltaInvalidation,
    ) -> Result<()> {
        let validity = stamp.uid_validity;
        let cancelled = || !self.current(validity);
        match invalidation {
            DeltaInvalidation::Seed => self.cache.discard_delta_seed(stamp, &cancelled)?,
            DeltaInvalidation::All => self.cache.invalidate_delta(
                &self.account,
                &self.mailbox,
                validity,
                fence,
                &cancelled,
            )?,
        }
        self.persistent.commit_delta_invalidation(
            &self.account,
            &self.mailbox,
            validity,
            invalidation,
        );
        Ok(())
    }
}

impl Batch<'_> {
    fn fence_absence(&self, absent: bool, validity: u32) -> Result<bool> {
        if !absent {
            return Ok(false);
        }
        self.service
            .persistent
            .sync_fence_absence(&self.lease.key, &self.mailbox, validity)
            .ok_or(Error::Deferred)
    }

    fn content_deadline(&self) -> Instant {
        self.deadline - (self.service.config.timeout / 4).min(Duration::from_millis(500))
    }

    async fn local<T: Send + 'static>(
        &self,
        operation: impl FnOnce(Scope) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let scope = Scope {
            cache: self.cache.clone(),
            account: self.lease.key.clone(),
            mailbox: self.mailbox.clone(),
            persistent: self.service.persistent.clone(),
            cancellation: self.cancellation.clone(),
        };
        let admission = runtime::read_admission();
        tokio::task::spawn_blocking(move || {
            let _admission = admission;
            if scope.cancellation.is_cancelled() {
                return Err(Error::Deferred);
            }
            operation(scope)
        })
        .await
        .map_err(|_| CacheError::Storage)?
    }

    async fn login(&self) -> crate::error::Result<Session> {
        if self.cancellation.is_cancelled()
            || self
                .runtime
                .catalog
                .sync_account(&self.lease.alias, &self.lease.key)
                .is_none()
        {
            return Err(MailError::account_unavailable());
        }
        self.service.backend_ready()?;
        // An extension fallback and SEARCH narrowing use the same turn.
        if self
            .connections
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            >= 2
        {
            return Err(MailError::backend());
        }
        self.service
            .connect_inner(true, Some(self.transport.clone()))
            .await
    }

    async fn connect(&self) -> crate::error::Result<(Session, scan::Selected)> {
        self.select_plain(self.login().await?).await
    }

    async fn select_plain(
        &self,
        mut session: Session,
    ) -> crate::error::Result<(Session, scan::Selected)> {
        let (selected, _) =
            scan::Selected::examine(&mut session, &self.lease.key, &self.mailbox).await?;
        self.observe_selected(&selected)?;
        Ok((session, selected))
    }

    fn observe_selected(&self, selected: &scan::Selected) -> crate::error::Result<()> {
        if !self.service.persistent.sync_observe(
            &self.lease.key,
            &self.mailbox,
            selected.uid_validity,
        ) {
            return Err(MailError::backend());
        }
        Ok(())
    }

    async fn run(
        &self,
        folder: &Folder,
        phase: Phase,
        policy: SyncPolicy,
        gc: Option<SyncGcPosition>,
    ) -> Result<Turn> {
        match phase {
            Phase::History => Box::pin(self.discover(ScanKind::History)).await,
            Phase::Tail => Box::pin(self.discover(ScanKind::Tail)).await,
            Phase::Reconcile if folder.may_probe(policy) => Box::pin(self.reconcile_auto()).await,
            Phase::Reconcile => Box::pin(self.reconcile()).await,
            Phase::Delta => Box::pin(self.delta_turn()).await,
            Phase::Content => Box::pin(self.content(folder.quota, policy)).await,
            Phase::Maintain => self.maintain(folder, gc).await,
        }
    }

    async fn discover(&self, kind: ScanKind) -> Result<Turn> {
        let (session, selected) = self.connect().await?;
        let observation = selected.observation(now()?);
        let scan = self
            .local(move |scope| scope.start(kind, observation))
            .await?;
        let Some((low, high)) = scan.search_window() else {
            return Ok(Turn::Idle);
        };
        let stamp = scan.stamp;
        let receipt = Box::pin(scan::discover(
            session,
            selected,
            stamp.uid_validity,
            stamp.high_uid,
            scan::Window { low, high },
            || self.connect(),
        ))
        .await?;
        let updated = self
            .local(move |scope| {
                Ok(receipt.commit(&scope.cache, &stamp, &|| !scope.current(stamp.uid_validity))?)
            })
            .await?;
        Ok(if updated.complete {
            Turn::Idle
        } else {
            Turn::More
        })
    }

    async fn reconciliation_fence(&self) -> Result<Arc<ReconcileFence>> {
        // A foreground result which began executing before the deletion
        // fence must settle before the new audit freezes its known UID set.
        if self
            .service
            .persistent
            .qresync_recovery(&self.lease.key, &self.mailbox)?
            .is_some_and(|recovery| !recovery.auditing)
        {
            self.service.persistent.sync_write_barrier().await?;
        }
        self.local(|scope| {
            let mut fence = scope
                .cache
                .sync_reconcile_fence(&scope.account, &scope.mailbox)?;
            if let Some(recovery) = scope
                .persistent
                .qresync_recovery(&scope.account, &scope.mailbox)?
                && !recovery.auditing
            {
                scope.cache.require_qresync_reconcile(
                    &scope.account,
                    &scope.mailbox,
                    recovery.validity,
                    &fence,
                    &|| !scope.current(recovery.validity),
                )?;
                scope.persistent.start_qresync_recovery(
                    &scope.account,
                    &scope.mailbox,
                    recovery.validity,
                )?;
                fence = scope
                    .cache
                    .sync_reconcile_fence(&scope.account, &scope.mailbox)?;
            }
            Ok(Arc::new(fence))
        })
        .await
    }

    async fn reconcile(&self) -> Result<Turn> {
        let fence = self.reconciliation_fence().await?;
        let (session, selected) = self.connect().await?;
        self.reconcile_selected(session, selected, fence, None)
            .await
    }

    async fn reconcile_selected(
        &self,
        mut session: Session,
        selected: scan::Selected,
        fence: Arc<ReconcileFence>,
        highest_modseq: Option<u64>,
    ) -> Result<Turn> {
        let observation = selected.observation(now()?);
        if highest_modseq.is_none() {
            let pending = self
                .local(|scope| {
                    Ok(scope
                        .cache
                        .pending_delta_floor(&scope.account, &scope.mailbox)?)
                })
                .await?;
            if pending.is_some_and(|(validity, _)| validity == selected.uid_validity) {
                self.fence_invalidation(&selected, DeltaInvalidation::Seed)?;
            }
        }
        let invalidation = self.delta_invalidation_pending(&selected);
        let start_fence = fence.clone();
        let (stamp, page, floor) = self
            .local(move |scope| {
                let scan = scope.cache.start_sync_reconcile(
                    &scope.account,
                    &scope.mailbox,
                    observation,
                    &start_fence,
                )?;
                scope.persistent.sync_commit_epoch(
                    &scope.account,
                    &scope.mailbox,
                    observation.uid_validity,
                );
                let cancelled = || !scope.current(observation.uid_validity);
                if let Some(invalidation) = invalidation {
                    scope.apply_delta_invalidation(&scan.stamp, &start_fence, invalidation)?;
                }
                if let Some(highest_modseq) = highest_modseq {
                    scope.cache.prepare_delta_seed(
                        &scan.stamp,
                        DeltaObservation {
                            uid_validity: observation.uid_validity,
                            uid_next: observation.uid_next,
                            highest_modseq,
                            checked_at_ms: observation.checked_at_ms,
                        },
                        &cancelled,
                    )?;
                }
                let page = scope.cache.sync_known_page(&scan.stamp, PAGE)?;
                let floor = if highest_modseq.is_some() {
                    condstore::ScopeFloors::read(&scope)?.for_epoch(observation.uid_validity)?
                } else {
                    condstore::EpochFloors::default()
                };
                Ok((scan.stamp, page, floor))
            })
            .await?;
        let proof = if page.uids.is_empty() {
            reconcile::Proof::empty_reconciliation(&selected, &page)?
        } else if highest_modseq.is_some() {
            match Box::pin(reconcile::collect_condstore_flags(
                &mut session,
                &selected,
                &page.uids,
            ))
            .await
            {
                Ok(proof) => proof,
                Err(_) => {
                    self.report_protocol(
                        ActiveProtocol::Rev1,
                        Some(ProtocolFallback::ExtensionFailed),
                    );
                    return Box::pin(self.reconcile_fresh(session, fence)).await;
                }
            }
        } else {
            Box::pin(reconcile::collect(
                &mut session,
                &selected,
                &page.uids,
                reconcile::Mode::Flags,
            ))
            .await?
        };
        drop(session);
        let invalidation = match proof.baseline_modseq {
            Some(delta::ModSeq::Unavailable) => {
                Some((ProtocolFallback::Nomodseq, DeltaInvalidation::All))
            }
            Some(delta::ModSeq::Highest(highest)) => floor
                .invalidation(highest)
                .map(|level| (ProtocolFallback::ModseqRollback, level)),
            _ => None,
        };
        if let Some((reason, level)) = invalidation {
            self.fence_protocol_fallback(&selected, reason, level)?;
        }
        let new_fence = self.fence_absence(!proof.absent.is_empty(), stamp.uid_validity)?;
        self.local(move |scope| {
            if let Some((_, level)) = invalidation {
                scope.apply_delta_invalidation(&stamp, &fence, level)?;
            }
            let state = proof.commit_reconcile(&scope.cache, &stamp, &page, now()?, &|| {
                !scope.current(stamp.uid_validity)
            })?;
            if state.complete {
                scope.persistent.finish_qresync_recovery(
                    &scope.account,
                    &scope.mailbox,
                    stamp.uid_validity,
                    true,
                );
            }
            if state.complete || new_fence {
                scope.persistent.sync_finish_reconcile(
                    &scope.account,
                    &scope.mailbox,
                    stamp.uid_validity,
                );
            }
            Ok(if state.complete {
                Turn::Idle
            } else {
                Turn::More
            })
        })
        .await
    }

    async fn content(&self, quota: QuotaMode, policy: SyncPolicy) -> Result<Turn> {
        let work = self
            .local(move |scope| {
                let Some(scan) = work_scan(&scope)? else {
                    return Ok(None);
                };
                let now = now()?;
                if quota == QuotaMode::Recovering {
                    scope
                        .cache
                        .resume_sync_quota_after_reclaim(&scan.stamp, 1)?;
                }
                let mut work = scope.cache.sync_content_page(&scan.stamp, now, 1)?.pop();
                if work.is_none()
                    && quota == QuotaMode::Startup
                    && scope.cache.resume_sync_quota(&scan.stamp, 1)? > 0
                {
                    work = scope.cache.sync_content_page(&scan.stamp, now, 1)?.pop();
                }
                Ok(work.map(|work| (work, scan.stamp.uid_validity)))
            })
            .await?;
        let Some((work, validity)) = work else {
            return Ok(Turn::Idle);
        };
        let deadline = self.content_deadline();
        if work.layer != ContentLayer::Facts && work.layer != ContentLayer::Raw {
            let (uid, layer, limit) = (work.uid, work.layer, self.service.config.max_message);
            let local_cancel = self.cancellation.child_token();
            let cancel_on_exit = local_cancel.clone().drop_guard();
            let cached = self.local(move |scope| {
                if !scope.current(validity) {
                    return Err(Error::Deferred);
                }
                let reference = crate::cache::Ref {
                    account: &scope.account,
                    mailbox: &scope.mailbox,
                    uid_validity: validity,
                    uid,
                };
                Ok(content::reuse(
                    &scope.cache,
                    reference,
                    &local_cancel,
                    layer,
                    limit,
                ))
            });
            let cached = tokio::time::timeout_at(deadline, cached).await;
            drop(cancel_on_exit);
            match cached {
                Ok(result) => match result? {
                    Ok(None) => {}
                    Ok(Some(outcome)) => return self.commit(work, validity, outcome).await,
                    Err(error) => return self.settle(work, validity, policy, Err(error)).await,
                },
                Err(_) => {
                    return self
                        .retry(work, validity, policy, Error::Mail(MailError::timeout()))
                        .await;
                }
            }
        }
        let (mut session, selected) = match tokio::time::timeout_at(deadline, self.connect()).await
        {
            Ok(Ok(connection)) => connection,
            Ok(Err(error)) => return self.retry(work, validity, policy, Error::Mail(error)).await,
            Err(_) => {
                return self
                    .retry(work, validity, policy, Error::Mail(MailError::timeout()))
                    .await;
            }
        };
        if !selected.matches_work(&work) {
            let observation = selected.observation(now()?);
            self.local(move |scope| scope.start(ScanKind::History, observation))
                .await?;
            return Ok(Turn::Deferred);
        }
        if work.layer == ContentLayer::Facts {
            let result = tokio::time::timeout_at(
                deadline,
                Box::pin(reconcile::collect(
                    &mut session,
                    &selected,
                    &[work.uid],
                    reconcile::Mode::Facts,
                )),
            )
            .await;
            drop(session);
            let proof = match result {
                Ok(Ok(proof)) => proof,
                Ok(Err(error)) => {
                    return self.retry(work, validity, policy, Error::Mail(error)).await;
                }
                Err(_) => {
                    return self
                        .retry(work, validity, policy, Error::Mail(MailError::timeout()))
                        .await;
                }
            };
            let new_fence = self.fence_absence(!proof.absent.is_empty(), validity)?;
            return self
                .local(move |scope| {
                    proof
                        .commit_facts(&scope.cache, &work, now()?, &|| !scope.current(validity))?;
                    if new_fence {
                        scope.persistent.sync_finish_reconcile(
                            &scope.account,
                            &scope.mailbox,
                            validity,
                        );
                    }
                    Ok(Turn::More)
                })
                .await;
        }
        let result = tokio::time::timeout_at(
            deadline,
            Box::pin(content::materialize(
                &self.cache,
                &mut session,
                content::Request {
                    account: &self.lease.key,
                    selected: &selected,
                    work: &work,
                    max_message: self.service.config.max_message,
                },
                &self.cancellation,
            )),
        )
        .await;
        drop(session);
        match result {
            Ok(result) => self.settle(work, validity, policy, result).await,
            Err(_) => {
                self.retry(work, validity, policy, Error::Mail(MailError::timeout()))
                    .await
            }
        }
    }

    async fn settle(
        &self,
        work: ContentWork,
        validity: u32,
        policy: SyncPolicy,
        result: std::result::Result<ContentOutcome, content::ContentError>,
    ) -> Result<Turn> {
        match result {
            Ok(outcome) => self.commit(work, validity, outcome).await,
            Err(content::ContentError::Storage(CacheError::Quota)) => {
                let _ = self
                    .commit(work, validity, ContentOutcome::QuotaBlocked)
                    .await;
                Ok(Turn::Failed(Failure::Quota))
            }
            Err(content::ContentError::Cancelled | content::ContentError::Stale) => {
                Ok(Turn::Deferred)
            }
            Err(content::ContentError::Read(error)) => {
                self.retry(work, validity, policy, Error::Mail(error)).await
            }
            Err(content::ContentError::Storage(error)) => {
                self.retry(work, validity, policy, Error::from(error)).await
            }
            Err(_) => Err(SyncError::Invalid.into()),
        }
    }

    async fn retry(
        &self,
        work: ContentWork,
        validity: u32,
        policy: SyncPolicy,
        error: Error,
    ) -> Result<Turn> {
        let Turn::Failed(failure) = error.turn() else {
            return Ok(Turn::Deferred);
        };
        let reason = match failure {
            Failure::Network => FailureReason::Network,
            Failure::Protocol => FailureReason::Protocol,
            _ => FailureReason::Storage,
        };
        let not_before_ms =
            now()?.saturating_add(policy.interval_seconds.max(BACKOFF.as_secs()) as i64 * 1000);
        self.commit(
            work,
            validity,
            ContentOutcome::RetryAt {
                reason,
                not_before_ms,
            },
        )
        .await?;
        Ok(Turn::Retry(failure))
    }

    async fn commit(
        &self,
        work: ContentWork,
        validity: u32,
        outcome: ContentOutcome,
    ) -> Result<Turn> {
        let new_fence =
            self.fence_absence(matches!(outcome, ContentOutcome::MissingRemote), validity)?;
        self.local(move |scope| {
            scope
                .cache
                .commit_sync_content(&work, outcome, &|| !scope.current(validity))?;
            if new_fence {
                scope
                    .persistent
                    .sync_finish_reconcile(&scope.account, &scope.mailbox, validity);
            }
            Ok(Turn::More)
        })
        .await
    }

    async fn maintain(&self, folder: &Folder, gc: Option<SyncGcPosition>) -> Result<Turn> {
        let pressure = folder.reclaim_active;
        let position = folder.reclaim;
        self.local(move |scope| {
            let page = match scope.cache.collect_sync_garbage(&scope.account, gc) {
                Err(SyncError::Stale) if gc.is_some() => {
                    scope.cache.collect_sync_garbage(&scope.account, None)?
                }
                result => result?,
            };
            let (reclaimed, next) = if pressure && page.released_bytes == 0 {
                content::reclaim(
                    &scope.cache,
                    &scope.account,
                    &scope.mailbox,
                    position,
                    &scope.cancellation,
                )?
            } else {
                (false, None)
            };
            Ok(Turn::Maintained {
                next,
                gc: page.next,
                released: reclaimed || page.released_bytes > 0,
            })
        })
        .await
    }
}

fn now() -> Result<i64> {
    checked_at_ms().ok_or_else(|| CacheError::Storage.into())
}

fn work_scan(scope: &Scope) -> Result<Option<ScanState>> {
    Ok(scope
        .cache
        .sync_status(&scope.account, &scope.mailbox)?
        .into_iter()
        .filter(|scan| scan.stamp.kind != ScanKind::Reconcile)
        .max_by_key(|scan| scan.stamp.high_uid))
}
