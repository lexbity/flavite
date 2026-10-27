# S3 spike, slice 2 of 2 — filesystem semantics (S3-2)

**Status:** recorded for Linux (measured); macOS/Windows rows are
source-verified and pending manual probe runs (labeled honestly below).
**Spec basis:** RFC-0001 §9 Phase 0 spike S3 ("atomic-replace semantics on
Windows and a network FS, cloud placeholder detection (notes and
attachments), advisory-lock behavior after `kill -9`"), D6 (atomic write;
symlinks resolved so we write the target; MUST NOT replace a symlink with a
regular file), §4.8 (cloud placeholders never hydrated by background scans),
§4.12 (advisory locks die with the process), §4.13/§4.16.2 (native-root key
and containment key on the *(volume, file)* identity pair, never the path
string), §10.6.1 (durability is a property of the backing filesystem).
**Plan:** PLAN-C slice C2. **Probe:** `xtask/src/bin/s3_fs_probe.rs` over
`xtask::spike_s3_fs` (raw `libc`/`windows-sys` FFI in the probe only — never
in `core`). **Recorded:** 2026-10-26, Linux 7.2.11-arch1-1, AMD Ryzen 7
5800X3D, x86_64. The default probe vault lived on tmpfs (`/tmp`); scenario 3
additionally measured a disk-backed `target/s3-fs-fsync` directory on ext4.

Windows and macOS development is deferred for now (per project decision).
Their rows are **source-verified against the platform API contracts**
(`ReplaceFileW`/`MoveFileExW`, `renamex_np(RENAME_EXCL)`,
`FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`, `UF_DATALESS`) and marked *pending
manual probe run* — they are evidence about the API's documented behavior,
not executed measurements, and the manual runs remain a C2 follow-up.

## Method

Seven scripted scenarios (tempdir vault), each exercising one filesystem
primitive the `Fs` trait's method contracts depend on. The probe records raw
measurements; `--self-check` asserts only the properties production wrappers
rely on (a fresh file is never a placeholder; the advisory lock survives a
`kill -9`; a symlink alias and its target share one identity; an opened target
through a symlinked parent yields the target's identity; no-clobber rename
fails when the destination exists and plain rename clobbers).

1. **rename-over** — `rename(2)` over an existing file; records whether the
   destination keeps the *source's* mode/mtime (the inode moves) and that
   content is replaced atomically;
2. **symlink rename-over** — demonstrates the D6 hazard: renaming a temp file
   directly over a *link path* replaces the link with a regular file;
   resolving the link first and writing the target preserves the link;
3. **fsync** — file + directory fsync timing per location (tmpfs `/tmp` and
   ext4 `target/s3-fs-fsync`), with filesystem identification on Linux;
   network-FS (SMB/NFS) caveats are documented, not measurable here;
4. **cloud placeholders** — detection attribute per platform; a freshly
   created fully-local file must not report as a placeholder;
5. **advisory lock after `kill -9`** — a child process takes an exclusive
   flock; the parent `kill -9`s it and re-acquires immediately;
6. **native identity / aliases / opened targets** — a symlink alias to the
   same file, a distinct file, and a path opened *through* a symlinked parent;
   records whether the *(volume, native-id)* pairs match/differ as the
   containment design requires;
7. **no-clobber rename** — compares plain `rename(2)` (clobbers) against
   `renameat2(RENAME_NOREPLACE)` for the `rename_no_replace` contract.

**What is asserted (self-check, `--self-check`):** the properties listed
above. CI re-runs all scenarios with `--self-check` on Linux headless.
**What is only measured:** every timing, and the raw
preserve/clobber/identity booleans outside the self-check set.

## Measured matrix — Linux (ext4 root, tmpfs `/tmp`), x86_64

Probe output, verbatim (self-check PASS):

| scenario | result |
| --- | --- |
| 1 rename-over | measured; mode preserved, content replaced, mtime preserved (inode swap) |
| 2 symlink | hazard demonstrated; resolve-first preserves the link |
| 3 fsync (tmpfs `/tmp`) | file 0 µs median / 1 µs p95; dir 0 µs median / 0 µs p95 — success is a **no-op guarantee** on tmpfs |
| 3 fsync (ext4 `target/s3-fs-fsync`) | file 647 µs median / 1540 µs p95; dir 0 µs median / 395 µs p95 |
| 4 placeholder | backend "none (Linux: treat as normal, §4.8)"; fresh file is not a placeholder |
| 5 advisory lock | child held; after `kill -9`: held-confirmed by parent, re-acquired, reacquire latency 0 ms |
| 6 identity | alias matches target; distinct file differs; **opened target through a symlinked parent matches the target** |
| 7 no-clobber | plain `rename(2)` clobbers; `renameat2(RENAME_NOREPLACE)` succeeds when missing and fails when present |

### Scenario notes

- **rename-over (1).** The destination's mode and mtime are the *source's*
  after `rename(2)`, because the directory entry now points at the source's
  inode. This is the property `rename_replace` relies on: permissions travel
  with the content, and readers see old or new bytes, never partial.
- **symlink (2).** Renaming a temp file over the link path *destroys the
  link* (it becomes a regular file). This is exactly the D6 hazard: the
  atomic-save path MUST `resolve_symlink` first and write the target, so a
  note that is a symlink stays a symlink.
- **fsync (3).** tmpfs returns success in ~0 µs — a no-op guarantee with no
  media to flush; callers MUST NOT infer a durability tier from success
  (§10.6.1). ext4 file fsync costs ~0.65 ms median (~1.5 ms p95); directory
  fsync median is ~0 µs with a ~0.4 ms p95 tail. Network filesystems are
  best-effort (§4.9) — documented here, not measurable on this host.
- **placeholder (4).** Linux exposes no standard "recalled on demand"
  attribute, so `is_placeholder` treats local files as normal. Windows
  (`FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` / `_RECALL_ON_OPEN`) and macOS
  (`UF_DATALESS` in `st_flags`) rows are source-verified, pending manual run.
- **advisory lock (5).** The parent re-acquired the lock immediately
  (0 ms) after `kill -9`ing the holder: the kernel releases flock with the
  process, leaving no stale lock (§4.12). Linux-only measurement for now.
- **identity (6).** A symlink alias and its target report one *(volume,
  native-id)* pair; a different file reports a different `native_id`; and a
  note opened *through* a symlinked parent resolves to the **target's** pair.
  This is the basis for root-key contention (§4.13), PathKey uniqueness, and
  opened-target containment checks (§4.16.2): validate the *opened target's*
  identity, never the path string alone.
- **no-clobber (7).** Plain `rename(2)` clobbers an existing destination, so
  `rename_no_replace` needs a dedicated primitive: `renameat2(RENAME_NOREPLACE)`
  on Linux (measured), `renamex_np(RENAME_EXCL)` on macOS and `MoveFileExW`
  without `MOVEFILE_REPLACE_EXISTING` on Windows (source-verified, pending
  manual runs).

## Per-OS matrix

Linux rows are measured on the machine above. macOS/Windows rows are
**source-verified against the platform API contracts** and marked *pending
manual probe run* — evidence about documented behavior, not executed
measurements. The deferred manual runs are a C2 follow-up.

| behavior | Linux (ext4/tmpfs) — measured | macOS (APFS) — source-verified, pending manual run | Windows (NTFS) — source-verified, pending manual run |
| --- | --- | --- | --- |
| atomic replace primitive | `rename(2)` | `rename(2)` (same POSIX semantics) | `ReplaceFileW` when the destination exists, falling back to `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` when it does not — `ReplaceFileW` alone fails over a missing target |
| replace preserves mode/mtime | yes (inode moves with the entry) | yes (same `rename(2)` semantics) | expected via `ReplaceFileW` (preserves ACLs/metadata of the target stream); pending manual run |
| symlink replaced by temp rename | yes — link destroyed (D6 hazard) | same `rename(2)` semantics | `MoveFileExW`/`ReplaceFileW` follow the same link-replace rule; pending manual run |
| no-clobber rename | `renameat2(RENAME_NOREPLACE)` — measured | `renamex_np(RENAME_EXCL)` | `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` |
| file fsync cost | ~647 µs median / 1540 µs p95 (ext4); ~0 µs (tmpfs, no-op) | pending manual run | `FlushFileBuffers`; pending manual run |
| directory fsync | `fsync` on a dir fd — ~0 µs median / 395 µs p95 (ext4); no-op (tmpfs) | same dir-fd `fsync` | **unsupported** — `FlushFileBuffers` cannot flush a directory handle; return `FsyncUnsupported`, fall back to the weaker documented mode (§4.9) |
| placeholder attribute | none — treat as normal (§4.8) | `UF_DATALESS` in `st_flags` | `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` / `_RECALL_ON_OPEN` |
| fresh file is a placeholder | false (asserted) | must be false | must be false |
| advisory lock after `kill -9` | released immediately; re-acquired at 0 ms (measured) | released with the process (POSIX flock) | released with the process (`LockFileEx`); pending manual run |
| identity is *(volume, file)* pair | alias==target, distinct differs, opened-through-parent==target (measured) | same `st_dev`+`st_ino` rule; pending manual run | volume serial + file index; pending manual run |

## Trait-contract finalization (A2 `Fs` trait)

The evidence above finalized the following method contracts in
`crates/flavite-fs/src/fs.rs` (debt paid in this slice; `MemFs`/tests updated
to match). Each cites the S3-2 scenario it rests on:

- **`Metadata::volume_id`** — native identity is the *(volume_id, native_id)*
  pair (scenario 6), not the pathname or a bare inode; inode numbers collide
  across filesystems. Containment/root-key logic (§4.13, §4.16.2) keys on the
  pair.
- **`metadata` symlink contract** — follows the `MemFs` model: reports a
  final-component symlink as itself (`is_symlink: true`, like `lstat(2)`);
  intermediate symlink components resolve as usual; read methods follow
  symlinks. `OsFs` MUST use `symlink_metadata`, not `metadata`.
- **`resolve_symlink` before write (D6)** — renaming over a symlink path
  replaces the link (scenario 2); the atomic-save path writes the target.
- **`rename_replace`** — Unix `rename(2)` (scenario 1); Windows `ReplaceFileW`
  + `MoveFileExW` fallback (source-verified).
- **`rename_no_replace`** — needs a dedicated no-clobber primitive because
  plain `rename(2)` clobbers (scenario 7): `renameat2(RENAME_NOREPLACE)` /
  `renamex_np(RENAME_EXCL)` / `MoveFileExW` w/o `MOVEFILE_REPLACE_EXISTING`.
- **`fsync_file` / `fsync_dir`** — durability strength is a property of the
  backing filesystem (scenario 3): tmpfs success is a no-op; Windows directory
  fsync is best-effort (`FsyncUnsupported`). Callers MUST NOT infer a tier
  from success alone.
- **`is_placeholder`** — per-platform detection (scenario 4); a fresh local
  file is never a placeholder on any platform.

The `MemFs` read-side lookups (`read_if_exists`, `read_capped`, `metadata`,
`read_dir`, `exists`) resolve **intermediate** symlink components so a path
through a symlinked parent lands on the target's identity (the scenario-6
opened-target hazard), with a depth cap matching `canonicalize`. Writes remain
as-modeled (the save path resolves the full path first, D6) — a documented
limitation, not a gap.

## Exceptions

No SHOULD-level exceptions are recorded from the Linux evidence. macOS/Windows
measurements are pending manual runs; if those runs contradict a contract
above, the exception is recorded here with a reason, per the spec's
Conventions. Two asymmetries are already recorded as *findings*, not
exceptions:

- **Windows directory fsync is unsupported** (`FlushFileBuffers` cannot flush
  a directory handle) — the design already covers this via the weaker
  documented durability mode (§4.9) and visible status; C5 must return
  `FsyncUnsupported`, not a silent success.
- **Linux has no placeholder attribute** — `is_placeholder` treats local files
  as normal (§4.8); the Windows/macOS attribute rows are source-verified,
  pending manual confirmation that a freshly created local file does not trip
  the detection bit.

## Feeds

- **C5 `flavite-fs::os` / `path` / `atomic`** — primitive choice per OS
  (`rename(2)`; `ReplaceFileW`+`MoveFileExW`; `renameat2(RENAME_NOREPLACE)` /
  `renamex_np(RENAME_EXCL)` / `MoveFileExW` w/o replace); `metadata` via
  `symlink_metadata`; `resolve_symlink` before atomic write; `FsyncUnsupported`
  for Windows directory fsync; identity via the *(volume, file)* pair.
- **C8 `instance.rs`** — advisory-lock release on process death (§4.12): the
  kernel frees flock with the holder, so a crash leaves no stale lock and a
  fresh instance acquires immediately (0 ms measured on Linux).
- **C13** — opened-target containment (§4.16.2): validate the *target's*
  identity pair, never the path string, when a note is reached through a
  symlinked parent.

## Reversal conditions

- If the CI Linux self-check starts failing, an `Fs` trait contract regressed —
  investigate before adjusting any contract or primitive.
- If the macOS/Windows manual runs show `ReplaceFileW` failing to preserve
  mode/ACL, or a placeholder bit tripping on a fresh local file, the affected
  contract changes via a recorded exception here (with the measured evidence).
- If a filesystem reports success on fsync but does not persist (a lying
  fsync), the durability model records it as an explicitly weaker mode with
  visible status — never as a tier inferred from success (§10.6.1).

## Reproduce

```sh
# Full run (all scenarios, self-check, JSON + markdown):
just spike-s3-fs --scenarios all --self-check --json target/s3-fs/report.json --markdown
# Disk-backed fsync (ext4) in addition to the tmpfs default:
just spike-s3-fs --scenarios 3 --fsync-dir target/s3-fs-fsync --self-check
# CI variant (Linux headless, all scenarios, JSON report):
just spike-s3-fs --scenarios all --self-check --json target/s3-fs-report.json
# On macOS / Windows (manual evidence for the pending rows):
cargo run -p xtask --bin s3_fs_probe -- --self-check --markdown
```

The advisory-lock scenario (5) is proven by the probe binary spawning a child
that takes the lock and is `kill -9`ed — it runs via the probe, not `cargo
test` (a `cargo test` `current_exe` is the libtest harness, which rejects the
probe's `--hold-lock` child flag). Unit tests (scenario builders, statistics,
self-check logic, and the live no-clobber/fsync/lock tests) run under
`cargo test -p xtask`.
