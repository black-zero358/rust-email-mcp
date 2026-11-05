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

fn state(folders: Vec<Folder>, raw: &str) -> State {
    State::new(
        [1; 32],
        folders,
        RefKey { folder: 0, uid: 1 },
        Options::default(),
        facts(raw),
        QUOTA,
    )
    .unwrap()
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
fn windows_are_numeric_fair_and_advance_only_with_all_hits_reserved() {
    let mut state = state(
        vec![folder("INBOX", 1, 701), folder("Sent", 2, 301)],
        "Message-ID: <seed@test>\r\n\r\n",
    );
    let first = search(&state);
    assert_eq!(
        (first.folder, first.field, first.start, first.end),
        (0, SearchField::MessageId, 502, 701)
    );
    let budget = state.owned_bytes();
    assert_eq!(
        state.apply_search(first, &[501], QUOTA),
        Err(StateError::InvalidHits)
    );
    assert_eq!(
        state.apply_search(first, &[701, 701], QUOTA),
        Err(StateError::InvalidHits)
    );
    assert_eq!(
        state.apply_search(first, &[700, 701], budget),
        Err(StateError::Capacity)
    );
    assert_eq!(search(&state), first);
    assert_eq!(state.checkpoint(), 0);
    state.apply_search(first, &[700, 701], QUOTA).unwrap();
    assert_eq!(candidate(&state).reference.uid, 701);
    assert!(
        !state
            .apply_facts(
                candidate(&state),
                facts("Message-ID: <unrelated@test>\r\n"),
                QUOTA
            )
            .unwrap()
    );
    assert_eq!(candidate(&state).reference.uid, 700);
    state
        .apply_facts(
            candidate(&state),
            facts("Message-ID: <unrelated@test>\r\n"),
            QUOTA,
        )
        .unwrap();
    for expected in [
        (0, SearchField::References, 502, 701),
        (0, SearchField::InReplyTo, 502, 701),
        (1, SearchField::MessageId, 102, 301),
        (1, SearchField::References, 102, 301),
        (1, SearchField::InReplyTo, 102, 301),
        (0, SearchField::MessageId, 302, 501),
    ] {
        let work = search(&state);
        assert_eq!((work.folder, work.field, work.start, work.end), expected);
        state.apply_search(work, &[], QUOTA).unwrap();
    }
}

#[test]
fn exact_fields_expand_both_directions_and_keep_missing_ids_independent() {
    let mut state = state(
        vec![
            folder("INBOX", 10, 8),
            folder("Sent", 11, 8),
            folder("Archive", 12, 8),
        ],
        "Message-ID: <seed@test>\r\nReferences: <parent@test>\r\n\r\n",
    );
    let work = search(&state);
    state.apply_search(work, &[2], QUOTA).unwrap();
    let child = "Message-ID: <child@test>\r\nReferences: <seed@test>\r\n\r\n";
    assert!(
        !state
            .apply_facts(candidate(&state), facts(child), QUOTA)
            .unwrap()
    );
    assert_eq!(state.coverage().known_locations, 1);
    let work = search(&state);
    assert_eq!(work.field, SearchField::References);
    state.apply_search(work, &[2, 3], QUOTA).unwrap();
    assert!(
        state
            .apply_facts(
                candidate(&state),
                facts("References: <seed@test>\r\n\r\n"),
                QUOTA
            )
            .unwrap()
    );
    assert!(
        state
            .apply_facts(candidate(&state), facts(child), QUOTA)
            .unwrap()
    );
    assert_eq!(state.message_id(2).unwrap().as_str(), "<child@test>");
    assert_eq!(state.message_id(1).unwrap().as_str(), "<parent@test>");
    let mut saw_parent = false;
    let mut saw_reply = false;
    while let Some(work) = state.next_work() {
        match work {
            Work::Search(unit) => {
                let id = state.message_id(unit.target).unwrap().as_str();
                let found: &[u32] = match (unit.folder, unit.field, id) {
                    (1, SearchField::MessageId, "<parent@test>")
                    | (2, SearchField::InReplyTo, "<child@test>") => &[1],
                    _ => &[],
                };
                state.apply_search(unit, found, QUOTA).unwrap();
            }
            Work::Verify(item) if item.reference.folder == 1 => {
                saw_parent = state
                    .apply_facts(
                        item,
                        facts("Message-ID: <parent@test>\r\nReferences: <child@test>\r\n\r\n"),
                        QUOTA,
                    )
                    .unwrap();
            }
            Work::Verify(item) => {
                saw_reply = state
                    .apply_facts(item, facts("In-Reply-To: <child@test>\r\n\r\n"), QUOTA)
                    .unwrap();
            }
        }
    }
    assert!(saw_parent && saw_reply);
    assert!(state.coverage().graph_complete);
    assert_eq!(state.coverage().known_locations, 5);
    let events = state
        .plan_delta()
        .items()
        .iter()
        .map(|key| state.event(*key).unwrap())
        .collect::<Vec<_>>();
    let missing = events
        .iter()
        .filter(|event| event.message_id.is_none())
        .collect::<Vec<_>>();
    assert_eq!(missing.len(), 2);
    assert_ne!(missing[0].entity, missing[1].entity);
    assert_eq!(
        state
            .reference(RefKey { folder: 1, uid: 1 })
            .unwrap()
            .uid_validity,
        11
    );
}

#[test]
fn failure_checkpoints_resume_and_do_not_block_independent_folders() {
    let mut state = state(
        vec![folder("INBOX", 1, 8), folder("Sent", 2, 8)],
        "Message-ID: <seed@test>\r\n\r\n",
    );
    let first = search(&state);
    state.search_failed(first, Failure::Protocol).unwrap();
    state.folder_failed(0, Failure::Backend).unwrap();
    assert_eq!(
        state.search_errors().collect::<Vec<_>>(),
        [(first, Failure::Protocol)]
    );
    assert_eq!(state.folder_checked(0, 99), Err(StateError::InvalidScope));
    let work = search(&state);
    assert_eq!(work.folder, 1);
    state.apply_search(work, &[2], QUOTA).unwrap();
    let retry = candidate(&state);
    state.candidate_failed(retry, Failure::Missing).unwrap();
    assert_eq!(
        state.candidate_errors().collect::<Vec<_>>(),
        [(retry, Failure::Missing)]
    );
    assert!(!state.coverage().graph_complete);
    state.begin_page();
    state.folder_checked(0, 1).unwrap();
    assert_eq!(candidate(&state), retry);
    state
        .apply_facts(retry, facts("Message-ID: <seed@test>\r\n\r\n"), QUOTA)
        .unwrap();
    let mut retried = false;
    while let Some(Work::Search(work)) = state.next_work() {
        retried |= work == first;
        state.apply_search(work, &[], QUOTA).unwrap();
    }
    assert!(retried && state.coverage().graph_complete);
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

#[test]
fn fitting_a_delivery_prefix_retains_the_remaining_ordered_deltas() {
    let raw = "Message-ID: <same@test>\r\n\r\n";
    let mut state = state(vec![folder("INBOX", 1, 25)], raw);
    let window = search(&state);
    state
        .apply_search(window, &(2..=25).collect::<Vec<_>>(), QUOTA)
        .unwrap();
    while let Some(Work::Verify(item)) = state.next_work() {
        state.apply_facts(item, facts(raw), QUOTA).unwrap();
    }
    let mut page = state.plan_delta();
    assert_eq!(page.items().len(), MAX_RESULTS);
    let first_omitted = page.items()[3];
    page.truncate(3);
    assert!(state.coverage_after(&page).unwrap().delivery_pending);
    state.mark_emitted(&page).unwrap();
    let next = state.plan_delta();
    assert_eq!(next.items().len(), MAX_RESULTS);
    assert_eq!(next.items()[0].location, first_omitted.location);
    assert!(!next.items()[0].new_entity);
}

#[test]
fn unavailable_initial_boundary_stays_incomplete_without_creating_later_work() {
    let unavailable = Folder {
        name: "Archive".into(),
        boundary: Boundary::Unavailable(Failure::Timeout),
    };
    let state = state(
        vec![folder("INBOX", 1, 1), unavailable],
        "Subject: seed with no ID or links\r\n\r\n",
    );
    assert!(state.next_work().is_none());
    assert!(state.folder_coverage(0).unwrap().complete);
    assert_eq!(
        state.folder_coverage(1).unwrap().failure,
        Some(Failure::Timeout)
    );
    assert_eq!(state.coverage().unresolved_errors, 1);
    assert!(!state.coverage().graph_complete);
}

#[test]
fn error_snapshot_has_a_fixed_detail_limit_and_reports_omitted_errors() {
    let mut folders = vec![folder("INBOX", 1, 1)];
    folders.extend((1..MAX_FOLDERS).map(|index| Folder {
        name: format!("folder-{index}").into_boxed_str(),
        boundary: Boundary::Unavailable(Failure::Backend),
    }));
    let mut state = state(folders, "Message-ID: <seed@test>\r\n\r\n");
    while let Some(Work::Search(unit)) = state.next_work() {
        state.search_failed(unit, Failure::Protocol).unwrap();
    }
    let errors = state.error_snapshot();
    assert_eq!(errors.count(), MAX_FOLDERS + 2);
    assert_eq!(errors.entries().count(), MAX_ERROR_DETAILS);
    assert_eq!(errors.omitted(), 2);
    assert_eq!(errors.count(), state.coverage().unresolved_errors);
    assert_eq!(
        errors.entries().next(),
        Some(CheckpointError::Folder {
            folder: 1,
            failure: Failure::Backend
        })
    );
}

#[test]
fn checked_folder_subset_can_deliver_later_locations_without_losing_earlier_events() {
    let mut state = state(
        vec![folder("INBOX", 1, 1), folder("Sent", 2, 2)],
        "Message-ID: <seed@test>\r\n\r\n",
    );
    loop {
        let unit = search(&state);
        if unit.folder == 1 && unit.field == SearchField::References {
            state.apply_search(unit, &[2], QUOTA).unwrap();
            break;
        }
        state.apply_search(unit, &[], QUOTA).unwrap();
    }
    state
        .apply_facts(
            candidate(&state),
            facts("Message-ID: <reply@test>\r\nReferences: <seed@test>\r\n\r\n"),
            QUOTA,
        )
        .unwrap();
    let mut eligible = [false; MAX_FOLDERS];
    eligible[1] = true;
    let delivery = state.plan_delta_for_folders(eligible);
    assert_eq!(delivery.items().len(), 1);
    assert_eq!(state.event(delivery.items()[0]).unwrap().location.folder, 1);
    assert!(state.coverage_after(&delivery).unwrap().delivery_pending);
    state.mark_emitted(&delivery).unwrap();
    let rest = state.plan_delta();
    assert_eq!(rest.items().len(), 1);
    assert_eq!(state.event(rest.items()[0]).unwrap().location.folder, 0);
}

#[test]
fn folder_preflight_rotates_all_starts_without_marking_search_progress() {
    let mut state = state(
        vec![
            folder("INBOX", 1, 1),
            folder("Sent", 2, 2),
            folder("Archive", 3, 3),
            folder("Other", 4, 4),
        ],
        "Message-ID: <seed@test>\r\n\r\n",
    );
    for expected in [0, 1, 2, 3, 0] {
        assert_eq!(state.folder_probe_start(), expected);
        assert_eq!(state.rotate_folder_probe(), expected);
    }
    assert_eq!(state.checkpoint(), 0);
}

#[test]
fn another_search_field_cannot_retry_a_failed_header_in_the_same_page() {
    let raw = "Message-ID: <seed@test>\r\n\r\n";
    let mut state = state(vec![folder("INBOX", 1, 3)], raw);
    let first = search(&state);
    state.apply_search(first, &[2], QUOTA).unwrap();
    let failed = candidate(&state);
    state.candidate_failed(failed, Failure::Protocol).unwrap();

    let second = search(&state);
    assert_eq!(second.field, SearchField::References);
    state.apply_search(second, &[2], QUOTA).unwrap();
    let third = search(&state);
    assert_eq!(third.field, SearchField::InReplyTo);
    state.apply_search(third, &[], QUOTA).unwrap();
    assert!(state.next_work().is_none());
    assert_eq!(state.coverage().pending_candidates, 2);
    assert_eq!(state.coverage().unresolved_errors, 1);

    state.begin_page();
    assert_eq!(candidate(&state), failed);
    state.apply_facts(failed, facts(raw), QUOTA).unwrap();
    assert!(state.coverage().graph_complete);
}

#[test]
fn filtered_first_copy_does_not_suppress_new_flags_or_choose_its_unchecked_reference() {
    let raw = "Message-ID: <same@test>\r\nSubject: identical\r\n\r\n";
    let mut proof = facts(raw);
    proof.evidence.content = Some(ContentFingerprint::raw_mime_body(b"identical"));
    let mut state = State::new(
        [1; 32],
        vec![folder("INBOX", 1, 1), folder("Sent", 2, 2)],
        RefKey { folder: 0, uid: 1 },
        Options::default(),
        proof,
        QUOTA,
    )
    .unwrap();
    loop {
        let unit = search(&state);
        if unit.folder == 1 && unit.field == SearchField::MessageId {
            state.apply_search(unit, &[2], QUOTA).unwrap();
            break;
        }
        state.apply_search(unit, &[], QUOTA).unwrap();
    }
    let mut copy = facts(raw);
    copy.evidence.content = Some(ContentFingerprint::raw_mime_body(b"identical"));
    state.apply_facts(candidate(&state), copy, QUOTA).unwrap();
    let plan = state.plan_delta();
    assert_eq!(plan.items().len(), 2);
    assert!(!plan.items()[1].new_entity && !plan.items()[1].new_variant);

    let mut eligible = [false; MAX_FOLDERS];
    eligible[1] = true;
    let selected = state.plan_delta_for_folders(eligible);
    let item = selected.items()[0];
    assert_eq!(selected.items().len(), 1);
    assert_eq!(item.location, plan.items()[1].location);
    assert!(item.new_entity && item.new_variant);
    let event = state.event(item).unwrap();
    assert_eq!(event.representative, RefKey { folder: 1, uid: 2 });
    assert_eq!(event.variant, state.event(plan.items()[0]).unwrap().variant);
    state.mark_emitted(&selected).unwrap();

    let remaining = state.plan_delta();
    assert_eq!(remaining.items().len(), 1);
    assert_eq!(remaining.items()[0].location, plan.items()[0].location);
    assert!(!remaining.items()[0].new_entity && !remaining.items()[0].new_variant);
    // The committed event is replayable even though the normal plan has changed.
    assert_eq!(
        state.event(item).unwrap().representative,
        event.representative
    );
    assert!(state.event(item).unwrap().new_variant);
}
