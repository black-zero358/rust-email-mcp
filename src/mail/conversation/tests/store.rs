use super::super::{
    graph::{Boundary, Folder, RefKey, Work},
    identity::ThreadFacts,
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
