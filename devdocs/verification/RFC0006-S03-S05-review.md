# RFC-0006 S03/S05 integration review — 2026-11-05

Status: **partial; neither slice is certified complete**.

HEAD: `13782e2eec055ba8844ff4f3d5db05cbd5438f39`.
Dirty source digest: `73a2a99ddd6a12f389316ca62c6ddf6b57c7dcce629d8b90cb00c02c2281f9c7`.
Digest framing matches the native runner's sorted tracked/unignored source algorithm.
Evidence classes: source, model, integration, fault, rendered_os_input.
This receipt makes no release-load, coverage, installed-production-artifact,
Windows/macOS-native or full-RFC conformance claim.

## Changes and positive verification

- Catalog SQL reads now use native-key/note-ID keyset continuation with at most
  500 returned entries. Every continuation checks epoch/generation in its read
  transaction. A changed version refuses the acquisition instead of mixing pages.
- API acquisition assembles a complete bounded snapshot on its worker; it refuses
  a corpus/memory overflow rather than publishing a truncated completion catalog.
  Native keys are decoded by the actual Fs codec, not reconstructed from display
  strings. Tests cover 603 entries, replay, exact ordering, aliases, limits, stale
  versions and refusal to redirect an unsupported native filename.
- Fuzzy completion retains at most eight ranked results. It no longer allocates
  and sorts a whole-catalog result vector. A 50k-candidate regression exercises
  the bound and deterministic order.
- Find option and replacement commands are discoverable in the palette; the
  option controls are exposed through runtime accessibility. Stale/unmounted
  option actions are rejected. Escape closes Find and restores editor focus
  without granting a graph insertion target.
- Native regex input now tests invalid capture-template refusal, original-source
  replacement, one Undo, Redo and exact saved bytes. The native completion
  scenario tests title and alias selection, canonical Markdown, Undo/Redo and
  actual saved source bytes. These are real Xvfb/OS-input scenarios with screenshots.
- Removed shortcut synonym mapping and migrated arrow defaults to canonical
  names. Non-Unix regex execution now explicitly refuses unsupported isolation;
  the previous empty resource-limit function/unisolated spawn is gone.

Passing focused evidence:
catalog keyset regression; eight API catalog tests; UI library and
note_search/note_regex/catalog_acquisition/runtime_accessibility tests;
editor library/controlled_input; 11 journal fault tests; gate/ownership tests;
95 cumulative source gates; formatting; Clippy; architecture check/lint.
Logs and provenance are under `target/rfc0006-s03-s05-review/`.
Native run paths are recorded in the corresponding `rfc6-*-verified.log`.

The final full workspace suite exited 0: 132 test binaries/doc-test groups,
with 1,660 passed and three ignored. The archived log is
`rfc6-workspace-final-result.log`. Earlier failing runs are retained too;
they are not substituted for the final result.

## Exit obligations still open

1. S03 release-profile reference-pattern and allocation/cancellation measurements
   at the prescribed sizes/sample counts, plus installed production GUI/CLI/helper
   smoke. The native scenario uses the fixture app and does not prove packaging.
2. S03 expanded multiline query/replacement input and Alt+Enter semantics;
   complete scale/AT/IME and encoding-matrix native evidence. The current fields
   remain single-line Iced text inputs.
3. Windows Job-object enforcement and supported-platform native receipts.
   Explicit refusal is safe behavior, not implementation of Windows regex support.
4. S05 complete bounded CatalogService/host ownership extraction. App still
   materializes picker/editor candidate copies when accepting a snapshot.
   The new SQL pages do not prove the host-turn or allocator NFRs.
5. S05 completion selection must bind canonical target identity and validate it
   through semantic prepare/insert at commit. The widget still constructs a
   source edit from candidate strings; buffer revision guarding alone cannot
   prove a target has not changed after catalog capture.
6. Source Fs currently refuses non-UTF8 names. The new negative test proves it
   does not redirect an unsupported name to its lossy spelling; it does not
   establish full native-name lifecycle support or visible exclusion reporting.

These are implementation/evidence obligations, not permissions to weaken the
RFC or mark slices complete. Prior receipts remain historical evidence.


## Fresh fuzz evidence

Both 60-second reruns exited 0: text_match completed 102,094 cases and
text_worker_protocol completed 9,024,734 cases. Commands used
`ASAN_OPTIONS=quarantine_size_mb=64 cargo +nightly fuzz run TARGET --
-max_total_time=60 -rss_limit_mb=512`, outside sandbox tracing.
The RSS ceiling stayed 512 MiB. Earlier runs are retained: matcher completed
106,013 cases but failed LeakSanitizer shutdown under tracing; protocol exceeded
the RSS ceiling with the default ASan quarantine. Those initial runs are failures,
not passes; sanitizer quarantine tuning does not establish production helper
memory conformance.

Final native artifacts:
- `target/workspace-evidence/note-regex-replace-1793946156-1196348/`
- `target/workspace-evidence/link-completion-1793946487-1322546/`

These prove their scripted Linux workflows only, not all AC-3/10/17/19/20/22
obligations. Open exit obligations above remain mandatory.
