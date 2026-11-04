//! RFC-0004 S8 — host-turn budget guard (§6.7, NFR-10, AC-9).
//!
//! A 50-ticket burst plus catalog drain runs every turn through the
//! measured ingress; the decision hook (the sanctioned test surface —
//! `tracing::warn` is a thin wrapper over it) proves: turns under the
//! 2 ms budget never warn, a synthetic over-budget turn warns exactly
//! once per rate window, and the burst itself stays under the budget.
//!
//! The decision state is a process-global rate window, so both tests
//! serialize on a file-local lock.

use flavite_api::turn_budget::{TURN_BUDGET, reset_for_test, warn_decision};
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[path = "support/links.rs"]
mod support;

fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();
    SERIAL
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// (NFR-10) The real ingress stays under budget: a 50-ticket catalog
/// burst + drain (every submit is its own host turn through
/// `Flavite::execute`'s measurement) — the slowest individual turn must
/// sit under the 2 ms budget, so the guard emitted nothing.
#[test]
fn fifty_ticket_burst_stays_under_the_budget() {
    let _guard = serial();
    reset_for_test();
    let f = support::fixture(&[
        ("a.md", "Alpha\n"),
        ("b.md", "Beta\n"),
        ("folder/c.md", "Gamma\n"),
    ]);

    let mut slowest = Duration::ZERO;
    for _ in 0..50 {
        let started = Instant::now();
        f.flavite.submit_catalog().expect("ticket admitted");
        slowest = slowest.max(started.elapsed());
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && f.flavite.query_service().in_flight() > 0 {
        f.flavite.pump();
        let _ = f.flavite.query_service().drain();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        f.flavite.query_service().in_flight(),
        0,
        "all 50 tickets served"
    );

    assert!(
        slowest <= TURN_BUDGET,
        "the slowest submit turn {slowest:?} exceeds the 2 ms budget"
    );
    // The guard agrees with the measurement: a turn the size of the
    // observed worst submit is under budget, so `observe` emitted nothing.
    assert!(!warn_decision(slowest));
}

/// (§6.7) A synthetic over-budget turn warns; the rate limit suppresses
/// the immediate follow-up; the window elapses and warns again.
#[test]
fn over_budget_turn_warns_once_per_rate_window() {
    let _guard = serial();
    reset_for_test();

    // Under the budget: never a warn.
    assert!(!warn_decision(TURN_BUDGET));
    assert!(!warn_decision(TURN_BUDGET - Duration::from_micros(1)));
    assert!(!warn_decision(Duration::ZERO));

    // A synthetic 5 ms turn: first warns…
    assert!(
        warn_decision(Duration::from_millis(5)),
        "a 5 ms turn must exceed the 2 ms budget"
    );
    // …the immediate repeat is rate-limited…
    assert!(
        !warn_decision(Duration::from_millis(5)),
        "the rate window must suppress the follow-up warn"
    );
    assert!(
        !warn_decision(Duration::from_millis(50)),
        "the rate window covers any over-budget turn within 1 s"
    );

    // …and the window elapses: warns again.
    std::thread::sleep(Duration::from_millis(1050));
    assert!(
        warn_decision(Duration::from_millis(5)),
        "after the 1 s rate window a new warn is allowed"
    );

    // Under-budget turns after a warn stay silent (and do not re-arm the
    // window for a later over-budget turn — the window is warn-paced).
    std::thread::sleep(Duration::from_millis(1050));
    assert!(!warn_decision(Duration::from_micros(100)));
    assert!(
        warn_decision(Duration::from_millis(5)),
        "under-budget turns must not extend the rate window"
    );
}
