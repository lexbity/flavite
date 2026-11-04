//! RFC-0004 S5 — ticketed catalog acquisition (W-D, D9, AC-6, R9).
//!
//! The catalog read leaves the host turn: submit is an admission, the
//! worker serves the whole 50k bounded read, outcomes carry provenance and
//! the truncation flag, supersession coalesces refreshes, and saturation
//! yields typed `Busy` — never a silent drop or a blocking frame.

use flavite_api::{
    CATALOG_CONTEXT_TAB, ContextQueryResult, Flavite, MAX_NOTE_CATALOG_LIMIT, MAX_QUERY_SLOTS,
    QueryPayload, QueryPurpose, QuerySubmitError,
};
use std::time::{Duration, Instant};

#[path = "support/links.rs"]
mod support;

fn fixture() -> support::Fixture {
    let notes = [
        ("a.md", "Alpha\n"),
        ("b.md", "Beta\n"),
        ("folder/c.md", "Gamma\n"),
    ];
    support::fixture(&notes)
}

/// Drain until `token` is served (workers are real; poll with a deadline).
fn served(flavite: &Flavite, deadline: Instant) -> Vec<flavite_api::QueryOutcome> {
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        flavite.pump();
        let drained = flavite.query_service().drain();
        seen.extend(drained);
        if !seen.is_empty() && flavite.query_service().in_flight() == 0 {
            return seen;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    seen
}

fn catalog_of(outcomes: &[flavite_api::QueryOutcome]) -> Vec<flavite_api::QueryOutcome> {
    outcomes
        .iter()
        .filter(|outcome| outcome.token.purpose == QueryPurpose::Catalog)
        .cloned()
        .collect()
}

#[test]
fn submit_roundtrip_matches_the_sync_path_exactly() {
    let f = fixture();
    let sync = f.flavite.note_catalog(MAX_NOTE_CATALOG_LIMIT).unwrap();
    let token = f.flavite.submit_catalog().expect("ticket admitted");
    assert_eq!(token.purpose, QueryPurpose::Catalog);
    assert_eq!(token.context_tab, CATALOG_CONTEXT_TAB);
    let deadline = Instant::now() + Duration::from_secs(10);
    let result = loop {
        if Instant::now() >= deadline {
            panic!("catalog ticket never served");
        }
        flavite_pump(&f);
        if let Some(outcome) = f
            .flavite
            .query_service()
            .drain()
            .into_iter()
            .find(|outcome| outcome.token == token)
        {
            break outcome.result;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let ContextQueryResult::Catalog {
        entries,
        provenance,
        total,
        truncated,
    } = result
    else {
        panic!("expected a catalog page, got {result:?}");
    };
    // Contents equality: the ticketed path IS the sync path.
    assert_eq!(entries.to_vec(), sync.entries);
    assert_eq!(
        (provenance.epoch, provenance.generation),
        (sync.epoch, sync.generation)
    );
    assert_eq!(total, sync.total);
    assert!(!truncated, "three notes cannot hit the 50k bound");
}

fn flavite_pump(f: &support::Fixture) {
    f.flavite.pump();
}

#[test]
fn supersession_keeps_only_the_newest_ticket_for_the_reserved_context() {
    let f = fixture();
    let first = f.flavite.submit_catalog().expect("first admitted");
    let second = f.flavite.submit_catalog().expect("second admitted");
    assert!(second.request_sequence > first.request_sequence);
    let outcomes = catalog_of(&served(
        &f.flavite,
        Instant::now() + Duration::from_secs(10),
    ));
    // Either the older result was superseded before any drain saw it, or it
    // was served strictly before the newer one — never after. A catalog
    // page applied after settlement must always be the newest ticket.
    let positions: Vec<usize> = outcomes
        .iter()
        .enumerate()
        .filter_map(|(index, outcome)| (outcome.token == second).then_some(index))
        .collect();
    let one_newest = positions.last().copied().expect("newest ticket served");
    if let Some(older) = outcomes
        .iter()
        .enumerate()
        .find(|(_, outcome)| outcome.token == first)
        .map(|(index, _)| index)
    {
        assert!(
            older < one_newest,
            "a stale page must never land after the newest"
        );
    }
    let last = outcomes.last().expect("at least the newest page");
    assert_eq!(last.token, second);
}

#[test]
fn saturation_is_typed_busy_and_recovery_serves_every_accepted_ticket() {
    let f = fixture();
    let payload = QueryPayload::Catalog {
        limit: MAX_NOTE_CATALOG_LIMIT as u64,
    };
    let mut accepted = 0usize;
    for attempt in 0..(4 * MAX_QUERY_SLOTS) {
        match f.flavite.query_service().submit(
            3_000 + u128::try_from(attempt).unwrap(),
            1,
            1,
            payload.clone(),
        ) {
            Ok(_) => accepted += 1,
            Err(QuerySubmitError::Busy) => {}
        }
        if accepted == MAX_QUERY_SLOTS {
            break;
        }
    }
    assert_eq!(accepted, MAX_QUERY_SLOTS, "capacity must be fillable");
    // The in-flight guard rejects the next admission as typed Busy —
    // deterministically, while every accepted slot is held (results are
    // served into the ready map and stay undrained).
    let busy = f.flavite.submit_catalog();
    assert!(
        matches!(busy, Err(QuerySubmitError::Busy)),
        "slot 65 must be typed Busy, never queued or lost, got {busy:?}"
    );
    // Draining releases capacity: the facade ticket is admitted again and
    // every accepted catalog was served exactly once.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut drained = Vec::new();
    while f.flavite.query_service().in_flight() > 0 {
        drained.extend(f.flavite.query_service().drain());
        if Instant::now() >= deadline {
            panic!("catalog tickets never settled");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    drained.extend(f.flavite.query_service().drain());
    let catalogs = catalog_of(&drained);
    assert_eq!(catalogs.len(), MAX_QUERY_SLOTS);
    assert!(catalogs.iter().all(|outcome| {
        matches!(
            outcome.result,
            ContextQueryResult::Catalog {
                truncated: false,
                total: 3,
                ..
            }
        )
    }));
    let again = f.flavite.submit_catalog().expect("capacity freed");
    let _ = again;
}

#[test]
fn truncation_flag_is_honest_at_tight_bounds() {
    let f = fixture();
    let service = f.flavite.query_service();
    let token = service
        .submit(
            CATALOG_CONTEXT_TAB,
            9,
            9,
            QueryPayload::Catalog { limit: 2 },
        )
        .expect("admitted");
    let outcomes = served(&f.flavite, Instant::now() + Duration::from_secs(10));
    let outcome = outcomes
        .iter()
        .find(|outcome| outcome.token == token)
        .expect("page served");
    let ContextQueryResult::Catalog {
        entries,
        total,
        truncated,
        ..
    } = &outcome.result
    else {
        panic!("expected catalog page");
    };
    assert_eq!(entries.len(), 2);
    assert_eq!(*total, 3);
    assert!(*truncated, "2 of 3 must flag truncation (R9)");

    // The service clamps `0` up to the smallest honest page, never zero.
    let zero = service
        .submit(
            CATALOG_CONTEXT_TAB,
            10,
            10,
            QueryPayload::Catalog { limit: 0 },
        )
        .expect("admitted");
    let outcomes = served(&f.flavite, Instant::now() + Duration::from_secs(10));
    let outcome = outcomes
        .iter()
        .find(|o| o.token == zero)
        .expect("clamped page served");
    let ContextQueryResult::Catalog {
        entries, truncated, ..
    } = &outcome.result
    else {
        panic!("expected catalog page");
    };
    assert_eq!(entries.len(), 1, "limit 0 clamps to 1");
    assert!(*truncated);
}

#[test]
fn submit_p99_is_microseconds_not_a_vault_read() {
    let f = fixture();
    let mut samples = Vec::new();
    for _ in 0..200 {
        // Settle fully so each sample measures only the ticket admission.
        while f.flavite.query_service().in_flight() > 0 {
            let _ = f.flavite.query_service().drain();
            std::thread::sleep(Duration::from_millis(1));
        }
        let started = Instant::now();
        let _token = f.flavite.submit_catalog().expect("admitted");
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    let p99 = samples[samples.len() * 99 / 100];
    assert!(
        p99 < Duration::from_micros(100),
        "AC-6: submit_catalog p99 must be < 100µs (host turn pays an admission only), measured {p99:?}"
    );
}
