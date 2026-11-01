# RFC-0003 W09 implementation and verification record

Date: 2026-10-31. Baseline: `4a9c148`. Status: **integrated W09 behavior implemented;
complete RFC conformance remains incomplete** for the evidence and inherited
obligations listed below. No native-input waiver is treated as a pass.

## Review and implementation

The supplied concept separates the editor's document/cursor, exploration
selection, and a contextual tool's source. W09 retains those independent
authorities. Rendered reading is a mode of the existing note tab; Outline,
Properties, and Preview are movable context sinks. Neither a mode change nor
focusing a tool authorizes a new writing target. The concept's counts, text,
colors, and pixel geometry are not acceptance fixtures. `scratch-devdocs/`
remains ignored and is not a build/test dependency.

The baseline had source-only Preview and no integrated rendered Read mode,
Outline, Properties, or guarded task/property controls. This change connects
the following paths to the actual workspace, Session, parser, resolver, API,
and custom editor:

* `flavite-syntax::read` builds a bounded semantic tree using the canonical
  CommonMark options and existing extension spans. The custom UI renderer
  displays headings, paragraphs, emphasis, quotes, lists, tables, tasks, code,
  links, tags, block anchors, footnotes, frontmatter, local images, and bounded
  note embeds. Embeds also render inside emphasized prose. HTML is inert
  visible source; math remains source text. No additional Markdown grammar is
  enabled. Canonical headings now participate in the extension scanner, and
  inline HTML participates in its exclusion mask. Parser version 3 forces
  existing disposable index caches to reparse.
* Note mode, per-view content anchors, and structure-tool bindings use the
  existing workspace reducer/codec. Duplicate views share text/history and
  retain independent modes. Composition blocks mode changes. Switching mode
  neither mutates nor saves the document. Read uses the current immutable
  shared buffer snapshot and displays unsaved state; closed notes use saved
  snapshots. Anchors map by source block rather than sharing raw pixel offsets.
* Outline follows its explicit source and navigates that source's note view,
  or explicitly opens it. Properties inspects current frontmatter, edits
  supported scalar values, and offers Edit source for complex/ambiguous YAML.
  Edit source activates the bound note in Edit mode. Preview renders the same
  content and offers an explicitly named Source preview mode.
* `property.set` and `task.toggle` are typed, write-authorized API commands.
  Source scope comes from BufferId, not a caller path. Two bounded Session
  workers prepare patches against immutable text; the Session commits only
  against the same revision in Clean/Dirty state, with standalone Undo
  boundaries. Invalid YAML, wrong task spans, stale revisions, unsupported
  states, denied capabilities, and read-only vaults leave text unchanged.
  Property edits replace only the scalar's bytes and preserve comments,
  quoting, ordering, BOM, and recorded line endings. They do not reserialize
  YAML. Task edits verify the actual canonical parser marker.
* Read queries distinguish note, context, anchor, occurrence, and attachment
  request purposes. Session/view/incarnation/sequence guards prevent late
  results from repainting a replacement. A transient catalog publication
  retains valid rendered bytes; identity actions require a complete current
  publication. A newer source revision invalidates the old action resource.
* Note embeds track visited entities, depth 4, and 32 embedded notes, with a
  16 MiB combined text ceiling. Image decoding admits PNG/JPEG/WebP with
  compressed-byte, dimension, pixel, and decoder-allocation limits. Shared
  lifetime reservations bound admitted Read resources to 128 MiB; image
  handles retain the same reservation. Hidden rendered views release their
  resources. Actual process/GPU memory acceptance remains separate below.
* Local attachment reads run on workers through API/Session/Fs. The new
  directory-capability read refuses parent/absolute traversal and escaping
  symlinks, including a tested symlink-swap schedule. `read_capped` now stops
  at the limit instead of first allocating the whole file. Explicit external
  links allow only http/https/mailto; there is no remote-image fetch path.
* Ctrl/Cmd+F searches current buffer text. Find field input has keyboard
  ownership, does not type into the editor, and highlights rendered matches.
  Saved-content vault search retains its existing semantics.
* The accessibility projection includes mounted headings/lists/tables/links,
  task state/actions, Outline entries, property inputs/actions, bindings,
  mode switching, find, and Preview controls. Native actions route through
  normal host messages and guards. Removed controls cannot execute; repeated
  embeds have distinct semantic identities. Source preview exposes source
  text rather than hidden rendered task actions. This is integration evidence,
  not completed native assistive-technology conformance.

## Earlier-slice defects addressed

| Boundary | Correction and regression evidence |
|---|---|
| W03 rapid input | Real 8 ms OS typing exposed captures arriving before redraw with an older revision. The production editor now sends ordered input intents; the host interprets them against acknowledged per-view text/selection. Only a contiguous chain of that view's own acknowledged edits can consume captures made before redraw. Another-origin edit breaks the chain. Unicode deletion, selection, copy/cut/paste, history, and document-boundary navigation retain guarded dispatch. No second text/history owner or blind stale-edit retry was added. |
| W03 focus/target | Ready/activated notes establish DocumentAnchor without manufacturing a writing caret. Read/hidden editor callbacks reject. Scrolling preserves an already exact insertion authority; find/navigation selections invalidate it. Live sink-to-sink bindings reject. |
| W05 operation correlation | Semantic preparation previously reused a caller number in the Session's internal receipt namespace. It now allocates an internal Session request ID, so concurrent open/save/link/structure operations cannot overwrite facade correlation. A concurrent prepare/property test checks distinct public outcomes. |
| Coordinate ownership | Fuzzing found repeated normalization of current buffer text. A preserved standalone CR adjacent to an LF could shift task spans. `TextSnapshot::from_normalized`, the shared parser snapshot entry point, and Read's normalized entry point preserve Session TextByte coordinates. The exact crash input is retained as a seed; real Session tests check the final saved mixed-EOL bytes and semantic insertion after preserved carriage returns. The insertion fuzz harness also constructs its buffer from the original raw snapshot, avoiding a second normalization in its own setup. |
| W03 buffer line authority | Insertion fuzzing exposed a save/snapshot panic on a Unicode line separator. Ropey now uses LF-only line rules matching TextSnapshot/EolTable; standalone CR and Unicode separators remain content. EOL-table rebuilding also consumes normalized coordinates. `revision_history` covers editing, Undo/Redo, and exact saved bytes with NEL, line/paragraph separators, and mixed EOL. |
| Verification | Every App update now reaches semantic observation, including early-return input/pump handlers. The real-window scenario asserts source/target bytes, typed outcomes, and mounted controls. Captures additionally verify actual rendered text using OCR; a loading frame cannot pass merely because a prior semantic snapshot was ready. W09 contracts are additive and retain forbidden-import/missing-file negative controls. |

## Verification and evidence

Reproducible commands:

```sh
just workspace-gate W09
just gate
cargo +1.90.0 check --workspace
bash scripts/workspace-visual-linux.sh --scenario read-structure
just network-denied
```

| Check | Result |
|---|---|
| Full `just gate` | Pass: formatting, workspace Clippy/tests, journal fault tests, architecture lint and generated-graph check |
| `just workspace-gate W09` and additive W09 contract profile | Pass |
| Rust 1.90 workspace check | Pass |
| Final X11 window workflow | Pass: `read-structure-1793499652-201475`, including actual captured text checks |
| Read-structure / insertion / parser-parity / restore fuzz smoke | Pass: respectively 144,528 / 384,333 / 9,704 / 2,654,970 iterations, 61 seconds each |
| Network namespace run | Incomplete/fail: host rejects `unshare -n` |

`workspace-gate W09` includes the W01–W08 model/integration tests and the new
`read_structure`, `property_edits`, `embed_limits`, `read_mode`, `read_views`, and
contained-read tests. These use real temporary-vault/Session/API paths, parser
output, guarded edits, actual saves, and Undo/Redo. The contained-read suite
includes 1,000 reads during a symlink swap. Workspace property tests retain
the 100,000-operation layout campaign.

The Linux window scenario runs in a private Xvfb X11 window, using actual OS
pointer/keyboard input. It checks fast typing, document-end caret placement,
Edit/Read identity, task edit/Undo/Redo, dirty find without note mutation,
source-bound Outline navigation, byte-preserving property editing, rendered
and Source Preview, terminal saving, unchanged destination-note bytes, and a
640×480 capture without desired-ratio changes. It also captures a real embed
inside emphasized prose. Evidence class: `rendered_os_input`, not native IME
or screen-reader evidence. Tesseract English data is a test-host dependency;
CI installs it through `ui-visual-setup`.

Final command logs and the evidence manifest live locally under
`target/workspace-evidence/w09/`. Each window run has its own manifest, fixture
hashes, build/lock/worktree fingerprint, semantic trace, reading checks, and
screenshots. Only disposable fixture data is exported by CI. HEAD alone does
not identify an uncommitted implementation; compare the worktree fingerprint.

The new `read_structure` fuzz target checks actual tree ranges and task/scalar
patches, including unchanged surrounding bytes. It is included in required
smoke membership and the existing strict nightly discovery loop. Parser
parity, semantic insertion, and workspace restore also run 60-second sanitizer
campaigns. The initial Read campaign found the coordinate bug described above;
the corrected algorithm is rerun, rather than weakening the task-span proof.
An initial 256 MiB sanitizer-process limit also failed with an empty reproducer;
the standard repository 4 GiB process budget is used for sanitizer campaigns.
That campaign does not establish the separate 256 MiB release-parser bound.

## Explicit unmet obligations

| Contract | Remaining work / evidence limit |
|---|---|
| W01 predecessor; FR-18, NFR-16 | The complete Linux X11/Wayland, macOS, and Windows screen-reader and Japanese/Pinyin/Korean IME matrix is not available here. Native heading/link/task/property operation and real AT reading still require those hosts. Source projection and simulated OS-action dispatch are not substitutes. |
| W09 network-isolation recipe | `just network-denied` compiles the test executables, but this host rejects `unshare -n` with Operation not permitted, including the outside-sandbox run. The namespace test remains failed/incomplete. Scheme/remote-image refusal tests and source gates pass independently. |
| NFR-07/11/17 and W14 release proof | Small fixtures and reservation counters do not establish dedicated-host parse latency, total RSS/GPU/cache use, or the maximum-input release-parser 256 MiB bound. Sanitizer process RSS is reported separately. Shared reference workloads and measured allocation headroom remain required. |
| Existing W06–W08 debt | Synchronous host persistence waits, prescribed dirty-parse scheduling/streaming, multi-graph resource policy/performance, pin pending-intent reconciliation, and saved-history graph inspection remain as recorded in `workspace-w08.md`. W09 does not claim to close them. |
| W06 unavailable restore records | `flavite-workspace/src/restore.rs` still drops unsupported view kinds and unrepresentable locators from the live restore plan. The RFC's inert unavailable tabs and complete native locator restore contract remain unfinished; codec preservation alone does not close that requirement. |
| W11–W14 scope | The full theme/font/size/contrast/backend matrix, production startup, final close/recovery choices, sustained workload, and full release acceptance are not supplied by W09. The development example still explicitly creates fixtures. |

These are unmet requirements, not waivers. The whole RFC and all predecessor
exit criteria must not be marked implemented from this record.
