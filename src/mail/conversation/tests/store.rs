use super::super::{
    graph::{Boundary, Failure, Folder, RefKey, Work},
    identity::{MessageId, ThreadFacts},
    state::Options,
};
use super::*;

const ACCOUNT: [u8; 32] = [1; 32];

fn make_state(limit: usize) -> Result<State, StateError> {
    let raw = b"Message-ID: <seed@test>\r\nSubject: sample\r\n\r\n";
    let facts = ThreadFacts::parse(&mailparse::parse_headers(raw).unwrap().0).unwrap();
    State::new(
        ACCOUNT,
        vec![Folder {
            name: "INBOX".into(),
            boundary: Boundary::Frozen {
                uid_validity: 1,
                high_uid: 501,
            },
        }],
        RefKey { folder: 0, uid: 1 },
        Options::default(),
        facts,
        limit,
    )
}

fn publish(store: &CursorStore, binding: Binding, now: Instant) -> Cursor {
    let lease = store.open_lease(binding, now, make_state).unwrap();
    let prepared = lease.prepare(lease.state().plan_delta()).unwrap();
    lease.commit(prepared, now).unwrap().next.unwrap()
}

fn advance(store: &CursorStore, binding: Binding, cursor: Cursor, now: Instant) -> Lease {
    match store.checkout(binding, cursor, now).unwrap() {
        Checkout::Advance(lease) => lease,
        Checkout::Replay(_) => panic!("expected advance"),
    }
}

#[test]
fn cancel_restores_search_checkpoint_and_keeps_delivery_pending_until_commit() {
    let store = CursorStore::default();
    let now = Instant::now();
    let binding = Binding::new(ACCOUNT, "principal");
    let cursor = publish(&store, binding, now);
    let mut lease = advance(&store, binding, cursor, now);
    assert!(matches!(
        store.checkout(binding, cursor, now),
        Err(StoreError::Unavailable)
    ));
    let Work::Search(unit) = lease.state().next_work().unwrap() else {
        panic!("expected search")
    };
    lease
        .update(|state, limit| state.apply_search(unit, &[500], limit))
        .unwrap();
    let Work::Verify(candidate) = lease.state().next_work().unwrap() else {
        panic!("expected candidate")
    };
    let facts = ThreadFacts::parse(
        &mailparse::parse_headers(b"Message-ID: <seed@test>\r\nSubject: sample\r\n\r\n")
            .unwrap()
            .0,
    )
    .unwrap();
    lease
        .update(|state, limit| state.apply_facts(candidate, facts, limit))
        .unwrap();
    let expected = lease.state().plan_delta();
    let canceled = lease.prepare(expected.clone()).unwrap();
    assert!(lease.state().coverage().delivery_pending);
    drop(lease);

    let lease = advance(&store, binding, cursor, now);
    assert_eq!(lease.state().coverage().completed_windows, 1);
    assert_eq!(lease.state().plan_delta(), expected);
    assert!(matches!(
        lease.commit(canceled, now),
        Err(StoreError::Unavailable)
    ));

    let lease = advance(&store, binding, cursor, now);
    let prepared = lease.prepare(expected.clone()).unwrap();
    let planned = prepared.next();
    let committed = lease.commit(prepared, now).unwrap();
    assert!(committed.next == planned);
    assert!(!committed.coverage.delivery_pending);

    let mut later = advance(&store, binding, planned.unwrap(), now);
    let Work::Search(unit) = later.state().next_work().unwrap() else {
        panic!("expected search")
    };
    later
        .update(|state, limit| state.apply_search(unit, &[], limit))
        .unwrap();
    drop(later);

    let Checkout::Replay(mut replay) = store.checkout(binding, cursor, now).unwrap() else {
        panic!("expected replay")
    };
    let view = replay.replay().unwrap();
    assert!(view.next == planned);
    assert_eq!(view.coverage, committed.coverage);
    assert_eq!(view.delivery, &expected);
    assert_eq!(view.folder_coverage(0).unwrap().completed_windows, 1);
    assert_eq!(
        replay.state().folder_coverage(0).unwrap().completed_windows,
        2
    );
    let event = replay.state().event(expected.items()[0]).unwrap();
    assert_eq!(event.location.uid, 500);
    assert!(matches!(
        replay.update(|_, _| Ok(())),
        Err(StoreError::Unavailable)
    ));
    drop(replay);
    assert!(
        advance(&store, binding, planned.unwrap(), now)
            .state()
            .plan_delta()
            .items()
            .is_empty()
    );
}

#[test]
fn idle_expiry_and_current_subject_and_generation_are_checked_on_each_checkout() {
    let store = CursorStore::default();
    let now = Instant::now();
    let binding = Binding::new(ACCOUNT, "principal");
    let first = publish(&store, binding, now);
    assert!(matches!(
        store.checkout(Binding::new(ACCOUNT, "other"), first, now),
        Err(StoreError::Unavailable)
    ));
    assert!(matches!(
        store.checkout(Binding::new([2; 32], "principal"), first, now),
        Err(StoreError::Unavailable)
    ));

    let almost_expired = now + CURSOR_TTL - Duration::from_secs(1);
    let mut lease = advance(&store, binding, first, almost_expired);
    let Work::Search(unit) = lease.state().next_work().unwrap() else {
        panic!("expected search")
    };
    lease
        .update(|state, limit| state.apply_search(unit, &[], limit))
        .unwrap();
    let prepared = lease.prepare(lease.state().plan_delta()).unwrap();
    let renewed = lease
        .commit(prepared, almost_expired)
        .unwrap()
        .next
        .unwrap();
    assert!(matches!(
        store.checkout(binding, first, now + CURSOR_TTL),
        Ok(Checkout::Replay(_))
    ));
    let mut lease = advance(&store, binding, renewed, now + CURSOR_TTL);
    let Work::Search(unit) = lease.state().next_work().unwrap() else {
        panic!("expected search")
    };
    lease
        .update(|state, _| state.search_failed(unit, Failure::Timeout))
        .unwrap();
    let prepared = lease.prepare(lease.state().plan_delta()).unwrap();
    let next = lease
        .commit(prepared, now + CURSOR_TTL)
        .unwrap()
        .next
        .unwrap();
    assert!(next != renewed);
    for cursor in [renewed, next] {
        assert!(matches!(
            store.checkout(binding, cursor, almost_expired + CURSOR_TTL),
            Err(StoreError::Unavailable)
        ));
    }
}

#[test]
fn successful_no_progress_page_replays_after_a_canceled_attempt_with_its_next_cursor() {
    let store = CursorStore::default();
    let now = Instant::now();
    let binding = Binding::new(ACCOUNT, "principal");
    let input = publish(&store, binding, now);
    let mut lease = advance(&store, binding, input, now);
    lease
        .update(|state, _| state.folder_failed(0, Failure::Timeout))
        .unwrap();
    let delivery = lease.state().plan_delta();
    assert!(delivery.items().is_empty());
    let prepared = lease.prepare(delivery.clone()).unwrap();
    let errors = *prepared.errors();
    assert_eq!(errors.count(), 1);
    let committed = lease.commit(prepared, now).unwrap();
    let next = committed.next.unwrap();
    assert!(next != input);

    let mut canceled = advance(&store, binding, next, now);
    canceled
        .update(|state, _| state.folder_checked(0, 1))
        .unwrap();
    assert_eq!(canceled.state().error_snapshot().count(), 0);
    drop(canceled);

    for _ in 0..2 {
        let Checkout::Replay(lease) = store.checkout(binding, input, now).unwrap() else {
            panic!("expected replay");
        };
        let replay = lease.replay().unwrap();
        assert!(replay.next == Some(next));
        assert_eq!(replay.delivery, &delivery);
        assert_eq!(replay.coverage, committed.coverage);
        assert_eq!(replay.errors, &errors);
    }
    drop(advance(&store, binding, next, now));
}

#[test]
fn observed_uidvalidity_change_invalidates_active_and_replay_tokens() {
    let store = CursorStore::default();
    let now = Instant::now();
    let binding = Binding::new(ACCOUNT, "principal");
    let first = publish(&store, binding, now);
    let mut lease = advance(&store, binding, first, now);
    let Work::Search(unit) = lease.state().next_work().unwrap() else {
        panic!("expected search")
    };
    lease
        .update(|state, limit| state.apply_search(unit, &[], limit))
        .unwrap();
    let prepared = lease.prepare(lease.state().plan_delta()).unwrap();
    let next = lease.commit(prepared, now).unwrap().next.unwrap();
    let mut lease = advance(&store, binding, next, now);
    assert!(matches!(
        lease.update(|state, _| state.folder_checked(0, 2)),
        Err(StoreError::State(StateError::InvalidScope))
    ));
    lease.invalidate().unwrap();
    for cursor in [first, next] {
        assert!(matches!(
            store.checkout(binding, cursor, now),
            Err(StoreError::Unavailable)
        ));
    }
}

#[test]
fn replay_retains_error_details_from_the_committed_page_after_a_canceled_retry() {
    use super::super::state::CheckpointError;
    let store = CursorStore::default();
    let now = Instant::now();
    let binding = Binding::new(ACCOUNT, "principal");
    let first = publish(&store, binding, now);
    let mut lease = advance(&store, binding, first, now);
    let Work::Search(unit) = lease.state().next_work().unwrap() else {
        panic!("expected search")
    };
    lease
        .update(|state, limit| state.apply_search(unit, &[], limit))
        .unwrap();
    lease
        .update(|state, _| state.folder_failed(0, Failure::Timeout))
        .unwrap();
    let prepared = lease
        .prepare_with_capacity(lease.state().plan_delta(), true)
        .unwrap();
    assert!(prepared.capacity_reached());
    let errors = *prepared.errors();
    let next = lease.commit(prepared, now).unwrap().next.unwrap();

    let mut lease = advance(&store, binding, next, now);
    lease.update(|state, _| state.folder_checked(0, 1)).unwrap();
    assert_eq!(lease.state().error_snapshot().count(), 0);
    drop(lease);

    let Checkout::Replay(lease) = store.checkout(binding, first, now).unwrap() else {
        panic!("expected replay")
    };
    let replay = lease.replay().unwrap();
    assert!(replay.capacity_reached);
    assert_eq!(replay.errors, &errors);
    assert_eq!(
        replay.errors.entries().collect::<Vec<_>>(),
        [CheckpointError::Folder {
            folder: 0,
            failure: Failure::Timeout
        }]
    );
}

#[test]
fn wire_cut_metadata_replays_the_committed_prefix_and_preserves_the_backlog() {
    let store = CursorStore::default();
    let now = Instant::now();
    let binding = Binding::new(ACCOUNT, "principal");
    let input = publish(&store, binding, now);
    let mut lease = advance(&store, binding, input, now);
    let Work::Search(unit) = lease.state().next_work().unwrap() else {
        panic!("expected search")
    };
    lease
        .update(|state, limit| state.apply_search(unit, &[500, 501], limit))
        .unwrap();
    while let Some(Work::Verify(candidate)) = lease.state().next_work() {
        let facts = ThreadFacts::parse(
            &mailparse::parse_headers(b"Message-ID: <seed@test>\r\n\r\n")
                .unwrap()
                .0,
        )
        .unwrap();
        lease
            .update(|state, limit| state.apply_facts(candidate, facts, limit))
            .unwrap();
    }
    let mut selected = lease.state().plan_delta();
    assert_eq!(selected.items().len(), 2);
    let later = selected.items()[1];
    selected.truncate(1);
    let prepared = lease
        .prepare_with_outcome(selected.clone(), false, true)
        .unwrap();
    assert!(prepared.wire_omitted());
    let next = lease.commit(prepared, now).unwrap().next.unwrap();

    let Checkout::Replay(replayed) = store.checkout(binding, input, now).unwrap() else {
        panic!("expected replay")
    };
    let view = replayed.replay().unwrap();
    assert!(view.wire_omitted);
    assert_eq!(view.delivery, &selected);
    assert!(view.coverage.delivery_pending);
    drop(replayed);

    let advanced = advance(&store, binding, next, now);
    assert_eq!(
        advanced.state().plan_delta().items()[0].location,
        later.location
    );
}

#[test]
fn unpublished_cancellation_releases_slot_and_the_sixty_fifth_lease_is_refused() {
    let store = CursorStore::default();
    let now = Instant::now();
    let binding = Binding::new(ACCOUNT, "principal");
    let base = store.0.lock().unwrap().used;
    let mut leases = (0..MAX_CURSORS)
        .map(|_| store.open_lease(binding, now, make_state).unwrap())
        .collect::<Vec<_>>();
    assert!(matches!(
        store.open_lease(binding, now, make_state),
        Err(StoreError::Capacity)
    ));
    let old = leases.pop().unwrap();
    assert!(store.0.lock().unwrap().used > base);
    drop(old);
    drop(store.open_lease(binding, now, make_state).unwrap());
    drop(leases);
    assert_eq!(store.0.lock().unwrap().used, base);
}

#[test]
fn aggregate_owned_capacity_includes_exclusive_leases_before_more_states_are_allocated() {
    let store = CursorStore::default();
    let now = Instant::now();
    let binding = Binding::new(ACCOUNT, "principal");
    let mut leases = Vec::new();
    let construct = |limit| {
        let mut facts = ThreadFacts::parse(
            &mailparse::parse_headers(b"Message-ID: <seed@test>\r\n\r\n")
                .unwrap()
                .0,
        )
        .unwrap();
        let padding = "a".repeat(700);
        facts.references.extend((0..512).map(|index| {
            MessageId::parse(format!("<{index}-{padding}@test>").as_bytes())
                .unwrap()
                .unwrap()
        }));
        let folders = (0..20)
            .map(|index| Folder {
                name: format!("folder-{index}").into_boxed_str(),
                boundary: Boundary::Frozen {
                    uid_validity: index + 1,
                    high_uid: 1,
                },
            })
            .collect();
        State::new(
            ACCOUNT,
            folders,
            RefKey { folder: 0, uid: 1 },
            Options::default(),
            facts,
            limit,
        )
    };
    loop {
        match store.open_lease(binding, now, construct) {
            Ok(lease) => leases.push(lease),
            Err(StoreError::Capacity) => break,
            Err(error) => panic!("{error:?}"),
        }
    }
    assert!((2..MAX_CURSORS).contains(&leases.len()));
    assert!(store.0.lock().unwrap().used <= MAX_CURSOR_BYTES);
    drop(leases.pop());
    drop(store.open_lease(binding, now, construct).unwrap());
}
