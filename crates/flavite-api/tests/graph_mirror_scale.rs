//! RFC-0004 S6 — mirror scale gate (AC-4, NFR-2; `#[ignore]`, run via
//! `just bench-scale` in the release profile).
//!
//! Builds a 50k-note / 150k-link synthetic vault, boots a cold
//! [`GraphMirror`] (service-start Reset ⇒ full-read rebuild), then applies
//! 300 real edits through the pool's apply spine and times the mirror's
//! `synchronize` (change-feed read + per-entity point reads + canonical
//! splices) for each. Assertions:
//!
//! * NFR-2/AC-4: synchronize wall p99 ≤ 15 ms at the design bound on the
//!   reference machine;
//! * full-read fallback parity: sampled during the run and re-verified at
//!   the end — the drift guard compares the mirror against
//!   `full_saved_graph` with ZERO findings (mirror-assembled == full
//!   consistent read byte-comparably, FR-5).
//! * RFC-0005 S2: one in fifty seeded links targets a ghost (≈3 000
//!   distinct unresolved targets across the vault), so every delta's
//!   `rebuild_unresolved` runs over a real section — the scale the F2
//!   linear dedup replaced an O(U²) scan on. The settle line prints the
//!   full counter set including `unresolved_targets`.
//!
//! Numbers are printed for the slice notes; the guard is honest — if a
//! machine cannot meet the bound it fails visibly rather than skipping.

use std::time::{Duration, Instant};

use flavite_api::graph_mirror::{GraphMirror, Synchronize, full_saved_graph, parity_findings};
use flavite_index::{IndexActor, open_or_init};
use rusqlite::{Connection, params};

const NOTES: i64 = 50_000;
const LINKS: i64 = 150_000;
const EDITS: usize = 300;
const SYNC_P99_BUDGET: Duration = Duration::from_millis(15);

/// Deterministic pseudo-random step (LCG) so runs are comparable.
struct Rng(u64);

impl Rng {
    fn next(&mut self, bound: i64) -> i64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as i64).rem_euclid(bound)
    }
}

fn seed_vault(conn: &Connection) {
    let tx = conn.unchecked_transaction().unwrap();
    {
        let mut statement = tx
            .prepare(
                "INSERT INTO notes(path, path_native, path_key, stem_key, title, id_conflict,
                                   content_hash, size, mtime_ns, scan_ns, parse_ok, generation)
                 VALUES(?1, ?2, ?3, ?4, ?5, 0, ?6, 64, 1, 1, 1, 1)",
            )
            .unwrap();
        for id in 1..=NOTES {
            let path = format!("n{id}.md");
            let path_key = flavite_syntax::text::fold_key(&path);
            let stem_key = flavite_syntax::text::fold_key(&format!("n{id}"));
            let hash = blake3::hash(&id.to_le_bytes()).as_bytes().to_vec();
            statement
                .execute(params![
                    path,
                    path.as_bytes(),
                    path_key,
                    stem_key,
                    format!("Note {id}"),
                    hash
                ])
                .unwrap();
        }
    }
    {
        let mut statement = tx
            .prepare(
                "INSERT INTO links(src, ord, kind, dst_raw, target_key, target_stem,
                                   span_start, span_end, dst_note, res_state)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'resolved')",
            )
            .unwrap();
        // RFC-0005 S2: one in fifty links is a ghost (an unresolved row no
        // edit can resolve — `g{link}` matches no note identity). That
        // seeds ≥3_000 distinct unresolved targets across the vault, so
        // every delta's `rebuild_unresolved` runs over a real section —
        // the scale that made the former O(U²) membership scan O(U²) and
        // what the F2 linear rebuild now handles at O(U log U).
        let mut ghost = tx
            .prepare(
                "INSERT INTO links(src, ord, kind, dst_raw, target_key, target_stem,
                                   span_start, span_end, res_state)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'unresolved')",
            )
            .unwrap();
        let mut rng = Rng(0x5EED);
        for link in 0..LINKS {
            let src = (link % NOTES) + 1;
            let ord = link / NOTES;
            let raw = if link % 50 == 0 {
                format!("g{link}")
            } else {
                format!("n{}", rng.next(NOTES) + 1)
            };
            let key = flavite_syntax::text::fold_key(&raw);
            if raw.starts_with('g') {
                ghost
                    .execute(params![
                        src,
                        ord,
                        "link",
                        raw,
                        key,
                        key,
                        10_i64 + ord * 100,
                        20_i64 + ord * 100,
                    ])
                    .unwrap();
                continue;
            }
            let target: i64 = raw[1..].parse().expect("resolved ghost-free target");
            statement
                .execute(params![
                    src,
                    ord,
                    "link",
                    raw,
                    key,
                    key,
                    10_i64 + ord * 100,
                    20_i64 + ord * 100,
                    target
                ])
                .unwrap();
        }
    }
    tx.execute("UPDATE meta SET value='1' WHERE key='generation'", [])
        .unwrap();
    tx.commit().unwrap();
}

#[test]
#[ignore = "scale gate (RFC-0004 AC-4): run via `just bench-scale`"]
fn mirror_synchronize_meets_the_bound_at_design_scale() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("scale.sqlite");
    let conn = open_or_init(&db).unwrap();
    // Speed up the bulk seed; safety irrelevant for a synthetic fixture.
    conn.pragma_update(None, "synchronous", "OFF").unwrap();
    let seeded = Instant::now();
    seed_vault(&conn);
    println!(
        "seeded {NOTES} notes / {LINKS} links in {:?}",
        seeded.elapsed()
    );

    // The actor owns the writer: real apply spine, real change rows.
    let mut actor = IndexActor::new(&db, 1).unwrap();
    drop(conn);
    let reader = Connection::open(&db).unwrap();
    let mut mirror = GraphMirror::new(None);

    // Cold start: service-start semantics — a Reset, then the full rebuild.
    let started = Instant::now();
    assert!(matches!(
        mirror.synchronize(&reader).unwrap(),
        Synchronize::Reset { .. }
    ));
    let rebuilt_version = mirror.force_reset(&reader).unwrap();
    let reset_wall = started.elapsed();
    let rebuilt = mirror.view().expect("view").clone();
    assert_eq!(rebuilt.version, rebuilt_version);
    assert_eq!(
        rebuilt.saved.nodes.len(),
        usize::try_from(NOTES).unwrap(),
        "the rebuild must carry the whole vault"
    );
    let full_baseline = full_saved_graph(&reader, None).unwrap();
    assert_eq!(
        parity_findings(&full_baseline, &rebuilt),
        Vec::<String>::new(),
        "the reset rebuild must equal the full read"
    );
    println!("cold reset + rebuild wall: {reset_wall:?}");

    // 300 real edits; measure synchronize (feed read + point reads +
    // canonical splices) per delta.
    let mut walls = Vec::with_capacity(EDITS);
    let mut last_generation = rebuilt.version.generation;
    let mut rng = Rng(0xC0FFEE);
    for index in 0..EDITS {
        let target = rng.next(NOTES) + 1;
        let path = format!("n{target}.md");
        let body = format!("# Rewritten {index}\nsee [[n{}]]\n", rng.next(NOTES) + 1);
        let stamp = i64::try_from(1_000_000 + index).unwrap();
        actor
            .apply_saved_note(path.as_bytes(), &path, body.as_bytes(), stamp, stamp)
            .unwrap();
        let started = Instant::now();
        let outcome = mirror.synchronize(&reader).unwrap();
        walls.push(started.elapsed());
        let Synchronize::Delta { version, reset, .. } = outcome else {
            panic!("a content advance inside the epoch must arrive as a delta");
        };
        assert!(!reset, "deltas must not reset the mirror");
        assert!(
            version.generation > last_generation,
            "every delta advances the version: {} -> {}",
            last_generation,
            version.generation
        );
        last_generation = version.generation;
        // Sampled equivalence proof (FR-5): the spliced view stays
        // byte-equal to a fresh full read at the same version.
        if index % 37 == 0 {
            let full = full_saved_graph(&reader, None).unwrap();
            let findings = parity_findings(&full, mirror.view().expect("view"));
            assert!(findings.is_empty(), "mirror drift at {index}: {findings:?}");
        }
    }

    walls.sort();
    let p50 = walls[walls.len() / 2];
    let p99 = walls[(((walls.len() as f64) * 0.99).ceil() as usize).saturating_sub(1)];
    let mean = walls.iter().map(|w| w.as_micros()).sum::<u128>() as u64 / EDITS as u64;
    println!(
        "mirror synchronize (feed read + point reads + canonical splices) at design bound: \
         p50={p50:?} p99={p99:?} mean={mean}µs (n={EDITS})"
    );
    assert!(
        p99 <= SYNC_P99_BUDGET,
        "NFR-2/AC-4 violated: p99 {p99:?} > {SYNC_P99_BUDGET:?}"
    );

    // Full-read fallback parity at the settled version.
    let started = Instant::now();
    let parity = mirror.parity_check(&reader).unwrap();
    println!(
        "parity check (includes a full consistent read) wall {:?}",
        started.elapsed()
    );
    assert!(parity.matched(), "mirror drift at scale: {parity:?}");
    assert_eq!(mirror.stats().mismatches_total, 0);
    assert_eq!(
        mirror.stats().resets_total,
        1,
        "only the cold start rebuilt"
    );
    assert!(matches!(
        mirror.synchronize(&reader).unwrap(),
        Synchronize::UpToDate
    ));

    // RFC-0005 S2: the settled counter set — `unresolved_targets` is the
    // scale every delta's linear `rebuild_unresolved` (F2) carries; the
    // count is printed, never an adjective, and never a behavior input.
    println!("mirror stats at settle: {:?}", mirror.stats());
}
