use super::super::identity::ContentFingerprint;
use super::*;

const QUOTA: usize = 1024 * 1024;

fn facts(raw: &str) -> ThreadFacts {
    ThreadFacts::parse(&mailparse::parse_headers(raw.as_bytes()).unwrap().0).unwrap()
}

fn folder(name: &str, validity: u32, high: u32) -> Folder {
    Folder {
        name: name.into(),
        boundary: Boundary::Frozen {
            uid_validity: validity,
            high_uid: high,
        },
    }
}

fn search(state: &State) -> SearchUnit {
    match state.next_work().unwrap() {
        Work::Search(unit) => unit,
        other => panic!("{other:?}"),
    }
}

fn candidate(state: &State) -> Candidate {
    match state.next_work().unwrap() {
        Work::Verify(candidate) => candidate,
        other => panic!("{other:?}"),
    }
}

#[test]
fn verified_copies_add_locations_while_unknown_and_conflicting_evidence_add_variants() {
    let raw = "Message-ID: <same@test>\r\nSubject: shared\r\n\r\n";
    let mut initial = facts(raw);
    let fingerprint = ContentFingerprint {
        method: 1,
        digest: [9; 32],
    };
    initial.evidence.content = Some(fingerprint);
    let mut state = State::new(
        [1; 32],
        vec![folder("INBOX", 1, 1), folder("Sent", 2, 3)],
        RefKey { folder: 0, uid: 1 },
        Options::default(),
        initial,
        QUOTA,
    )
    .unwrap();
    let first = state.plan_delta();
    let seed = state.event(first.items()[0]).unwrap();
    state.mark_emitted(&first).unwrap();
    for _ in 0..3 {
        let work = search(&state);
        state.apply_search(work, &[], QUOTA).unwrap();
    }
    let work = search(&state);
    assert_eq!((work.folder, work.field), (1, SearchField::MessageId));
    state.apply_search(work, &[1, 2, 3], QUOTA).unwrap();
    let mut copy = facts(raw);
    copy.evidence.content = Some(fingerprint);
    state.apply_facts(candidate(&state), copy, QUOTA).unwrap();
    let first_delta = state.plan_delta();
    let attached = state.event(first_delta.items()[0]).unwrap();
    assert_eq!(attached.entity, seed.entity);
    assert_eq!(attached.variant, seed.variant);
    assert!(!attached.new_entity && !attached.new_variant);
    assert_eq!(attached.status, IdentityStatus::Verified);
    state.mark_emitted(&first_delta).unwrap();

    state
        .apply_facts(candidate(&state), facts(raw), QUOTA)
        .unwrap();
    let second_delta = state.plan_delta();
    let unknown = state.event(second_delta.items()[0]).unwrap();
    assert_ne!(unknown.variant, seed.variant);
    assert!(unknown.new_variant);
    assert_eq!(unknown.status, IdentityStatus::IdentityUnverified);
    state.mark_emitted(&second_delta).unwrap();

    state
        .apply_facts(
            candidate(&state),
            facts("Message-ID: <same@test>\r\nSubject: conflict\r\n\r\n"),
            QUOTA,
        )
        .unwrap();
    let third_delta = state.plan_delta();
    let collision = state.event(third_delta.items()[0]).unwrap();
    assert_eq!(collision.entity, seed.entity);
    assert_ne!(collision.variant, seed.variant);
    assert_eq!(collision.status, IdentityStatus::Collision);
    assert_eq!(third_delta.items().len(), 1);
}
