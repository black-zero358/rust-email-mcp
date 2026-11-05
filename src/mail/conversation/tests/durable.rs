use super::*;
use crate::{
    accounts::AccountKey,
    cache::{AccountScope, Cache},
    mail::conversation::{
        graph::{Folder, RefKey, Work},
        identity::{MessageId, ThreadFacts},
        state::Options,
    },
};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

fn cache(directory: &Path, account: &AccountKey) -> Arc<Cache> {
    Cache::open(
        &directory.join("cache"),
        64 * 1024 * 1024,
        vec![AccountScope {
            account: account.clone(),
            mailboxes: vec!["INBOX".into()],
            max_bytes: 64 * 1024 * 1024,
        }],
        &[],
    )
    .unwrap()
}

fn state(namespace: [u8; 32], limit: usize) -> Result<State, StateError> {
    State::new(
        namespace,
        vec![Folder {
            name: "INBOX".into(),
            boundary: Boundary::Frozen {
                uid_validity: 7,
                high_uid: 501,
            },
        }],
        RefKey { folder: 0, uid: 1 },
        Options::default(),
        facts(),
        limit,
    )
}

fn facts() -> ThreadFacts {
    let raw = b"Message-ID: <seed@test>\r\nReferences: <parent@test>\r\n\r\n";
    ThreadFacts::parse(&mailparse::parse_headers(raw).unwrap().0).unwrap()
}

async fn publish(store: &CursorStore, binding: Binding) -> Cursor {
    let lease = store
        .open_lease(binding, Instant::now(), |limit| {
            state(binding.namespace, limit)
        })
        .unwrap();
    let prepared = lease.prepare(lease.state().plan_delta()).unwrap();
    lease
        .commit_async(prepared, Instant::now(), CancellationToken::new())
        .await
        .unwrap()
        .next
        .unwrap()
}

async fn advance(store: &CursorStore, binding: Binding, cursor: Cursor) -> Lease {
    let Checkout::Advance(lease) = store
        .checkout_async(binding, cursor, Instant::now())
        .await
        .unwrap()
    else {
        panic!("expected advance");
    };
    lease
}

#[tokio::test]
async fn restart_restores_no_progress_page_and_its_original_expiry() {
    let directory = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let binding = Binding::new(account.reference_namespace(), "owner");
    let database = cache(directory.path(), &account);
    let store = CursorStore::with_backend(
        database.cursor_backend(),
        |_, _| true,
        |_, _, _| true,
        |_, _| true,
    )
    .unwrap();
    let input = publish(&store, binding).await;
    let old = database
        .cursor_backend()
        .validate(binding.into(), *input.0.as_bytes(), utc_millis().unwrap())
        .unwrap();
    drop(store);
    drop(database);

    let database = cache(directory.path(), &account);
    let store = CursorStore::with_backend(
        database.cursor_backend(),
        |_, _| true,
        |_, _, _| true,
        |_, _| true,
    )
    .unwrap();
    assert!(matches!(
        store
            .checkout_async(
                Binding::new(binding.namespace, "other"),
                input,
                Instant::now()
            )
            .await,
        Err(StoreError::Unavailable)
    ));
    let mut lease = advance(&store, binding, input).await;
    assert!(lease.state().plan_delta().items().is_empty());
    lease
        .update(|state, _| {
            state.folder_failed(0, crate::mail::conversation::graph::Failure::Timeout)
        })
        .unwrap();
    let prepared = lease
        .prepare_with_outcome(lease.state().plan_delta(), false, true)
        .unwrap();
    let coverage = prepared.coverage();
    let next = lease
        .commit_async(prepared, Instant::now(), CancellationToken::new())
        .await
        .unwrap()
        .next
        .unwrap();
    assert!(next != input);
    let row = database
        .cursor_backend()
        .validate(binding.into(), *input.0.as_bytes(), utc_millis().unwrap())
        .unwrap();
    assert_eq!(row.last_progress_ms, old.last_progress_ms);
    assert_eq!(row.expires_ms, old.expires_ms);
    drop(store);
    drop(database);

    let database = cache(directory.path(), &account);
    let store = CursorStore::with_backend(
        database.cursor_backend(),
        |_, _| true,
        |_, _, _| true,
        |_, _| true,
    )
    .unwrap();
    let Checkout::Replay(replayed) = store
        .checkout_async(binding, input, Instant::now())
        .await
        .unwrap()
    else {
        panic!("expected replay");
    };
    let replay = replayed.replay().unwrap();
    assert!(replay.next == Some(next));
    assert_eq!(replay.coverage, coverage);
    assert!(replay.wire_omitted);
    assert_eq!(replay.errors.entries().count(), 1);
    drop(replayed);
    assert!(
        advance(&store, binding, next)
            .await
            .state()
            .plan_delta()
            .items()
            .is_empty()
    );
}

#[tokio::test]
async fn cancellation_at_transaction_commit_preserves_old_cursor_and_undelivered_location() {
    let directory = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let binding = Binding::new(account.reference_namespace(), "owner");
    let database = cache(directory.path(), &account);
    let backend = database.cursor_backend();
    let checks = Arc::new(AtomicUsize::new(usize::MAX));
    let gate = checks.clone();
    let store = CursorStore::with_backend(
        backend.clone(),
        |_, _| true,
        move |_, _, _| {
            if gate.load(Ordering::SeqCst) == usize::MAX {
                return true;
            }
            // The lease check and write preflight pass. Cancel only after the
            // backend has written the snapshot, just before its commit.
            gate.fetch_add(1, Ordering::SeqCst) < 2
        },
        |_, _| true,
    )
    .unwrap();
    let input = publish(&store, binding).await;
    let old = backend
        .validate(binding.into(), *input.0.as_bytes(), utc_millis().unwrap())
        .unwrap();
    let mut lease = advance(&store, binding, input).await;
    let Work::Search(unit) = lease.state().next_work().unwrap() else {
        panic!("expected search");
    };
    lease
        .update(|state, limit| state.apply_search(unit, &[500], limit))
        .unwrap();
    let Work::Verify(candidate) = lease.state().next_work().unwrap() else {
        panic!("expected candidate");
    };
    lease
        .update(|state, limit| state.apply_facts(candidate, facts(), limit))
        .unwrap();
    let delivery = lease.state().plan_delta();
    assert_eq!(delivery.items().len(), 1);
    let prepared = lease.prepare(delivery.clone()).unwrap();
    checks.store(0, Ordering::SeqCst);
    assert!(matches!(
        lease
            .commit_async(prepared, Instant::now(), CancellationToken::new())
            .await,
        Err(StoreError::Unavailable)
    ));
    assert_eq!(checks.swap(usize::MAX, Ordering::SeqCst), 3);
    assert_eq!(
        backend
            .validate(binding.into(), *input.0.as_bytes(), utc_millis().unwrap())
            .unwrap(),
        old
    );

    let lease = advance(&store, binding, input).await;
    assert_eq!(lease.state().plan_delta().items(), delivery.items());
    let prepared = lease.prepare(delivery).unwrap();
    let next = lease
        .commit_async(prepared, Instant::now(), CancellationToken::new())
        .await
        .unwrap()
        .next
        .unwrap();
    let Checkout::Replay(replayed) = store
        .checkout_async(binding, input, Instant::now())
        .await
        .unwrap()
    else {
        panic!("expected replay");
    };
    assert!(replayed.replay().unwrap().next == Some(next));
}

#[tokio::test]
async fn loaded_and_dormant_tokens_are_fenced_by_observed_uidvalidity() {
    let directory = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let binding = Binding::new(account.reference_namespace(), "owner");
    let database = cache(directory.path(), &account);
    let now_ms = utc_millis().unwrap();
    database
        .observe_folder(&account, "INBOX", 7, Some(501), now_ms)
        .unwrap();
    let store = CursorStore::with_backend(
        database.cursor_backend(),
        |_, _| true,
        |_, _, _| true,
        |_, _| true,
    )
    .unwrap();
    let resident = publish(&store, binding).await;
    let dormant = publish(&store, binding).await;
    drop(store);
    let store = CursorStore::with_backend(
        database.cursor_backend(),
        |_, _| true,
        |_, _, _| true,
        |_, _| true,
    )
    .unwrap();
    drop(advance(&store, binding, resident).await);
    database
        .observe_folder(&account, "INBOX", 8, Some(700), now_ms + 1)
        .unwrap();
    for cursor in [resident, dormant] {
        assert!(matches!(
            store.checkout_async(binding, cursor, Instant::now()).await,
            Err(StoreError::Unavailable)
        ));
    }
}

#[test]
fn restored_wall_clock_interval_rejects_expiry_and_future_or_invalid_progress() {
    let row = CursorRow {
        active: Some([2; 16]),
        replay: None,
        revision: 1,
        codec_version: persist::CODEC_VERSION,
        owned_bytes: 1024,
        encoded_len: 100,
        expires_ms: TTL_MS + 10,
        last_progress_ms: 10,
    };
    assert_eq!(remaining(row, 10).unwrap(), CURSOR_TTL);
    assert_eq!(
        remaining(row, row.expires_ms - 1).unwrap(),
        Duration::from_millis(1)
    );
    assert!(remaining(row, row.expires_ms).is_err());
    assert!(remaining(row, row.last_progress_ms - 1).is_err());
    assert!(
        remaining(
            CursorRow {
                expires_ms: row.expires_ms + 1,
                ..row
            },
            10
        )
        .is_err()
    );
}

#[tokio::test]
async fn dormant_rows_share_the_same_slot_budget_as_unpublished_leases() {
    let directory = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let binding = Binding::new(account.reference_namespace(), "owner");
    let database = cache(directory.path(), &account);
    let store = CursorStore::with_backend(
        database.cursor_backend(),
        |_, _| true,
        |_, _, _| true,
        |_, _| true,
    )
    .unwrap();
    publish(&store, binding).await;
    drop(store);
    let store = CursorStore::with_backend(
        database.cursor_backend(),
        |_, _| true,
        |_, _, _| true,
        |_, _| true,
    )
    .unwrap();
    let now = Instant::now();
    let mut active = (1..MAX_CURSORS)
        .map(|_| {
            store
                .open_lease(binding, now, |limit| state(binding.namespace, limit))
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        store.open_lease(binding, now, |limit| state(binding.namespace, limit)),
        Err(StoreError::Capacity)
    ));
    drop(active.pop());
    assert!(
        store
            .open_lease(binding, now, |limit| state(binding.namespace, limit))
            .is_ok()
    );
}

#[tokio::test]
async fn near_full_cursor_restores_with_its_existing_validation_reservation() {
    let directory = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let binding = Binding::new(account.reference_namespace(), "owner");
    let database = cache(directory.path(), &account);
    let store = CursorStore::with_backend(
        database.cursor_backend(),
        |_, _| true,
        |_, _, _| true,
        |_, _| true,
    )
    .unwrap();
    let input = publish(&store, binding).await;
    let mut lease = advance(&store, binding, input).await;
    let Work::Search(unit) = lease.state().next_work().unwrap() else {
        panic!("expected search");
    };
    let hits = (481..=500).collect::<Vec<_>>();
    lease
        .update(|state, limit| state.apply_search(unit, &hits, limit))
        .unwrap();

    let padding = "a".repeat(920);
    let mut verified = 0;
    while let Some(Work::Verify(candidate)) = lease.state().next_work() {
        let mut count = 1000;
        loop {
            let mut observed = facts();
            observed.references.extend((0..count).map(|index| {
                MessageId::parse(
                    format!("<{}-{index}-{padding}@test>", candidate.reference.uid).as_bytes(),
                )
                .unwrap()
                .unwrap()
            }));
            match lease.update(|state, limit| state.apply_facts(candidate, observed, limit)) {
                Ok(true) => {
                    verified += 1;
                    break;
                }
                Err(StoreError::Capacity) if count > 0 => count /= 2,
                other => panic!("unexpected apply result: {other:?}"),
            }
        }
        if lease.state().owned_bytes() > MAX_CURSOR_BYTES - 64 * 1024 {
            break;
        }
    }
    let charge = lease.state().owned_bytes();
    assert!(
        charge > MAX_CURSOR_BYTES - 64 * 1024,
        "cursor charged only {charge} bytes"
    );
    assert_eq!(lease.state().plan_delta().items().len(), verified);
    let prepared = lease.prepare(lease.state().plan_delta()).unwrap();
    let next = lease
        .commit_async(prepared, Instant::now(), CancellationToken::new())
        .await
        .unwrap()
        .next
        .unwrap();
    drop(store);
    drop(database);

    let database = cache(directory.path(), &account);
    let store = CursorStore::with_backend(
        database.cursor_backend(),
        |_, _| true,
        |_, _, _| true,
        |_, _| true,
    )
    .unwrap();
    let restored = advance(&store, binding, next).await;
    assert!(restored.state().owned_bytes() <= charge);
    assert!(restored.state().plan_delta().items().is_empty());
}

#[tokio::test]
async fn a_temporarily_unavailable_durable_scope_keeps_its_old_revision() {
    let directory = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let binding = Binding::new(account.reference_namespace(), "owner");
    let database = cache(directory.path(), &account);
    let backend = database.cursor_backend();
    let available = Arc::new(AtomicBool::new(true));
    let gate = available.clone();
    let store = CursorStore::with_backend(
        backend.clone(),
        |_, _| true,
        |_, _, _| true,
        move |_, _| gate.load(Ordering::SeqCst),
    )
    .unwrap();
    let input = publish(&store, binding).await;
    let original = backend
        .validate(binding.into(), *input.0.as_bytes(), utc_millis().unwrap())
        .unwrap();
    let lease = advance(&store, binding, input).await;
    let prepared = lease.prepare(lease.state().plan_delta()).unwrap();
    available.store(false, Ordering::SeqCst);
    assert!(matches!(
        lease
            .commit_async(prepared, Instant::now(), CancellationToken::new())
            .await,
        Err(StoreError::Unavailable)
    ));
    assert!(matches!(
        store.checkout_async(binding, input, Instant::now()).await,
        Err(StoreError::Unavailable)
    ));
    assert_eq!(
        backend
            .validate(binding.into(), *input.0.as_bytes(), utc_millis().unwrap())
            .unwrap(),
        original
    );
    available.store(true, Ordering::SeqCst);
    assert!(advance(&store, binding, input).await.durable_mode());
}
