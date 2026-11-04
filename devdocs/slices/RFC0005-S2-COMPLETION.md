# RFC-0005 Slice S2 — Mirror hygiene F2 + identity helper + doc truth

> Completion record. Locked decisions honored: FR-10 (dedup O(U log U) or
> better, **identical output** — the equivalence gate is the oracle),
> §7 debt rows (helper + doc truth). Small, safe, landed before the S3/S4
> mirror diff so those reviews shrink.

## Summary

`rebuild_unresolved` — the per-delta rebuild of the canonical unresolved
sections — no longer re-walks the growing node list per occurrence. The
`!nodes.iter().any(|node| node.id.id == id)` membership test (an O(U²) tax
on every delta, U = unresolved-section size) is replaced by a
`BTreeSet<u64>` seen-set updated alongside `nodes`: the whole section
rebuild is now O(U log U). The scattered `u64::try_from(..).unwrap_or(0)`
sentinel noise is consolidated into one helper with one documented
policy, and every false cost claim in the touched files now states the
truth (with the S3 removal pointer, so the S3 diff is honest).

## F2 — linear unresolved dedup (`graph_mirror.rs::rebuild_unresolved`)

- `pushed: BTreeSet<u64>`, updated with `insert` (true = first visit) at
  the exact place the old scan stood. Push order unchanged — statement
  order, first spelling of a target wins — and the canonical
  `nodes.sort_by_key(|record| record.id)` at the end of the function is
  **kept**, with a comment naming why it is what makes the rewrite
  output-identical: the sections' final shape depends only on membership,
  and membership is exactly the set of targets the scan visits.
- The function's doc comment was itself a false claim ("this stays
  O(unresolved + their records) per delta"); it now states
  O(U log U) since F2 and names the scan it replaced.
- Module header cost line (`synchronize + assembly at O(change +
  affected-run memmove)`) widened to include the unresolved-section term,
  with the F2 annotation — numbers, not adjectives.

## Identity helper (§7 debt row)

Three `#[inline]` helpers, one sentinel policy (doc verbatim in code):
note/attachment ids are ROWID-allocated positive integers; a negative id
cannot exist in a database this writer created; the 0 sentinel is
unreachable and exists only to keep the conversion total — a corrupt
negative row collapses to node 0 rather than panicking the pump, and the
parity guard still surfaces the drift.

- `node_row_id(id: i64) -> u64` — the policy's single home.
- `node_id_of(id: i64) -> NodeId` — note call shape (the RFC's named
  helper and signature).
- `attachment_id_of(id: i64) -> NodeId` — the second call shape over the
  same policy.

Sites consolidated (all previously inline `unwrap_or(0)` sentinels):
apply_delta heal read, `arriving` set construction, note-phase node id,
attachment removal, `splice_source_links` source + resolved-target arms
(the two `dst_note`/`dst_att` arms shared the identical pattern), and the
`unresolved_by_src` loop in `rebuild_unresolved` — plus the
`NoteRow::record`/`AttachmentRow::record` builders so the whole file
shares one policy. **Exit grep**
`grep -c "try_from(*id).unwrap_or(0)" crates/flavite-api/src/graph_mirror.rs` →
**0**; the only surviving `unwrap_or(0)` in the file is the helper body
itself. The inverse conversion (`node_id_entity`, i64 for SQL binds,
`unwrap_or(-1)`) is a different policy, intentionally not folded.

## Telemetry (`MirrorStats.unresolved_targets`)

New fixed-cardinality counter: distinct unresolved guard nodes in the view
after the last synchronize/reset — the scale `rebuild_unresolved` carries
per delta. Set in both producers of the canonical sections (delta arm of
`synchronize`, `force_reset`), documented "never a behavior input",
surfaced through the existing `stats()` Debug impression and printed by
the scale bench. (Service metrics-surface extension is S3/S5 territory;
this counter stays mirror-side for now.)

## Doc truth (S2's "doc truth" item)

- `graph_service.rs` `Shared::mirror` comment: the false "critical
  sections are bounded reads" claim rewritten with the actual invariant —
  pump + doctor lock; no holder writes the database, **but** the pump's
  section spans `synchronize`, the parity check when due, and the full
  `graph_projection::build` overlay assembly; the honest bound is one
  pump, and S3's snapshot handoff (FR-8) is what makes the sections
  genuinely bounded.
- The "S6 record's bounded reads claim": no RFC-0004-S6 record file
  exists in the repo (the slice records stop at S5); the claim lived only
  in the code docs — both places are now fixed. Noted here so the
  checklist item is auditable.

## Tests

- **New unit test** (`graph_mirror.rs mod tests` — the file's first):
  `delta_rebuild_over_a_thousand_unresolved_targets_is_exact` — 1_024
  distinct ghost targets across 4 sources seeded, cold reset rebuild,
  then one delta splice through the real apply spine; asserts the
  sections complete, strictly sorted by `NodeId` (uniqueness falls out),
  include the new target and its identity-guard key, track the
  `unresolved_targets` counter, and — the FR-10 oracle —
  `parity_findings(full_saved_graph, view)` is empty: byte-equal to a
  fresh full consistent read at the same version.
- **Equivalence gate (the permanent oracle)**:
  `graph_mirror_equivalence` — 9 green unchanged, incl. the 2_000-case
  proptest driving unresolved flip-flops with byte-equality assertions;
  F2 changed no output by construction and the gate proves it.
- `graph_mirror_service` 6, `graph_projection` 3, `link_occurrences` 5 —
  green unmodified.

## Scale bench (`graph_mirror_scale.rs`) — numbers, not adjectives

Seeding extended: one in fifty links now targets a ghost identity
(`g{link}`, matching no note) → ~3 000 distinct unresolved targets
across 1 000 sources at the 50k/150k design bound. Every delta's rebuild —
including the sampled `full_saved_graph` equivalence checks and the final
parity — runs over that section, so the F2 path is load-bearing in the
gate rather than vacuous (the unmodified seed left it at zero, and the
old O(U²) scan's cost was never measured). Measured (release, this
machine):

- synchronize p50 **2.83 ms**, p99 **4.16 ms**, mean 2.9 ms (n=300) —
  budget p99 ≤ 15 ms held with 3.6× headroom while carrying the rebuild.
  The S6 record's 0.9 ms baseline was measured on the ghost-free workload
  — different work, honestly labeled; what the budget guards (NFR-8) is
  the ≤ 15 ms bound, and the quadratic tax is what this number now
  prevents.
- Cold reset + rebuild 186 ms; full-read parity check 162 ms;
  mismatches 0, resets 1 (cold only), sampled + final equivalence clean.
- Settle line prints the full counter set: `MirrorStats { …
  unresolved_targets: 2991 }` (3 000 seeded − 9 retired by edits — the
  delta path retires ghosts of rewritten sources correctly, byte-equal to
  the full read).

## Exit-criteria verification

- Equivalence + service suites green, byte-equality unchanged — ✔.
- Helper consolidates the sentinel sites: exit grep → 0 — ✔.
- Doc comments state true invariants (`Shared::mirror`, module header,
  `rebuild_unresolved`) — ✔.
- `cargo clippy -p flavite-api --all-targets -- -D warnings` (and
  workspace) — green — ✔.
- `cargo test -p flavite-api --lib` incl. the new test; `just gate` —
  green (recorded below when the background run completed).
- Scale bench at bound — green, numbers recorded above.

## Costs accepted

- The bench's synchronize p50/p99 now include ~3 000 ghost-target rebuild
  work per delta (2.8/4.2 ms) — deliberate: the gate measures the section
  F2 fixed. Budget unchanged.
- One more `BTreeSet<u64>` allocated per rebuild (the section size is
  bounded by unresolved targets, minority by convention; allocation count
  unchanged versus the Vec it parallelizes).

## Next

S3 — mirror handoff: cached cursor-versioned `Arc<SavedGraph>` snapshot,
`view_arc()`, and the mutex out of `build` (FR-8/NFR-7), followed by S4's
clone removal measured against the honest baseline this slice recorded.
