//! RFC-0004 S7 — scan equivalence gate (FR-9, AC-7).
//!
//! The parallel scan must be observationally identical to the serial scan:
//! the full `WorkerMessage` stream (batch boundaries, batch contents,
//! progress cadence) and the `ScanResult` (every field, including
//! `present_*` and `incomplete`) for the same filesystem state. Corpora
//! cover the shapes the reconcile scan must survive: typical note/attachment
//! trees, unicode and pathological names, parse-failure and oversize mixes,
//! empty directories, and symlink/`.flavite` skips. Path orderings are
//! randomized (different construction orders yield different `walk_paths`
//! and nominated-set enumerations) and every corpus is scanned serially,
//! through a 1-thread pool, and through a 4-thread pool.

use std::collections::HashSet;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use flavite_fs::{Fs, MemFs};
use flavite_vault::reconcile_worker::{ScanJob, ScanResult, WorkerMessage, scan};
use flavite_vault::types::ReconcileScope;
use rayon::{ThreadPool, ThreadPoolBuilder};

const ROOT: &str = "/vault";

fn pools() -> (Option<ThreadPool>, ThreadPool, ThreadPool) {
    (
        None,
        ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("1-pool"),
        ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .expect("4-pool"),
    )
}

/// Run one scan to completion, capturing the entire message stream. The
/// channel is far larger than production's bounded(4) on purpose: the
/// production consumer's `try_send` drops are a consumption-timing artifact
/// the SCAN does not produce; cadence itself (which indices send what) is
/// exactly what this gate compares.
fn run(
    fs: &dyn Fs,
    scope: &ReconcileScope,
    nominated: &HashSet<PathBuf>,
    pool: Option<&ThreadPool>,
) -> (Vec<WorkerMessage>, ScanResult) {
    let root = PathBuf::from(ROOT);
    let (tx, rx) = crossbeam::channel::bounded::<WorkerMessage>(16_384);
    let collector = std::thread::spawn(move || {
        let mut messages = Vec::new();
        while let Ok(message) = rx.recv() {
            messages.push(message);
        }
        messages
    });
    let job = ScanJob {
        id: 42,
        scope,
        nominated,
    };
    let result = scan(fs, &root, &job, &tx, pool);
    drop(tx);
    (collector.join().expect("collector"), result)
}

/// One gate case: same filesystem, three execution modes, identical streams.
#[track_caller]
fn assert_all_modes_equal(fs: &dyn Fs, scope: &ReconcileScope, nominated: &HashSet<PathBuf>) {
    let (serial_pool, one, four) = pools();
    assert!(serial_pool.is_none());
    let (serial_messages, serial_result) = run(fs, scope, nominated, None);
    let (one_messages, one_result) = run(fs, scope, nominated, Some(&one));
    let (four_messages, four_result) = run(fs, scope, nominated, Some(&four));
    assert_eq!(
        serial_messages, one_messages,
        "1-thread pool diverged from serial"
    );
    assert_eq!(
        serial_messages,
        four_messages,
        "4-thread pool diverged from serial ({} vs {} messages)",
        serial_messages.len(),
        four_messages.len()
    );
    assert_eq!(serial_result, one_result, "1-thread result diverged");
    assert_eq!(serial_result, four_result, "4-thread result diverged");
}

/// Deterministic shuffle (xorshift64*) so "randomized orderings" are
/// reproducible across runs.
struct Rng(u64);

impl Rng {
    fn next(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)) >> 33) as usize % bound
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.next(i + 1);
            items.swap(i, j);
        }
    }
}

/// Typical vault: nested note trees, attachments, junk, symlinks, the
/// `.flavite` directory, and >128 paths so batch/progress cadence fires.
fn typical_corpus(seed: u64) -> MemFs {
    let fs = MemFs::new();
    let root = PathBuf::from(ROOT);
    fs.create_dir_all(&root).expect("root");
    fs.create_dir_all(&root.join(".flavite/journal"))
        .expect("journal dir");
    fs.write_new(
        &root.join(".flavite/journal/open.journal"),
        b"must be skipped entirely",
    )
    .expect("journal file");
    let mut items: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    for index in 0..400 {
        let dir = ["", "a/", "a/b/", "deep/in/space here/"][index % 4];
        items.push((
            root.join(format!("{dir}note-{index:04}.md")),
            format!(
                "---\nid: I{index}\ntags: [t{index}, shared]\n---\n# Note {index}\n\n[[note-{}]] ![[pic.png]]\n",
                (index * 7) % 400
            )
            .into_bytes(),
        ));
    }
    for index in 0..60 {
        items.push((
            root.join(format!("pics/pic{index}.png")),
            format!("png-bytes-{index}").into_bytes(),
        ));
    }
    items.push((root.join("pics/pic.png"), b"png-bytes".to_vec()));
    for index in 0..25 {
        items.push((
            root.join(format!("docs/doc-{index}.pdf")),
            format!("pdf-{index}").into_bytes(),
        ));
    }
    for index in 0..30 {
        items.push((
            root.join(format!("junk/stray-{index}.txt")),
            format!("text-{index}").into_bytes(),
        ));
    }
    items.push((root.join("archive.tar.gz"), b"tgz".to_vec()));
    items.push((root.join("noextension"), b"raw".to_vec()));
    items.push((root.join("UPPER-case.MD"), b"# upper".to_vec()));
    items.push((
        root.join("weird name.with dots .md"),
        b"# dots\n[[x]]".to_vec(),
    ));
    let mut rng = Rng(seed | 1);
    rng.shuffle(&mut items);
    for (path, bytes) in &items {
        if let Some(parent) = path.parent() {
            fs.create_dir_all(parent).expect("mkdir");
        }
        fs.write_new(path, bytes).expect("write");
    }
    // Symlinks and empty directories must be skipped by the walk.
    fs.create_symlink(&root.join("link-to-note.md"), &root.join("note-0000.md"));
    fs.create_dir_all(&root.join("empty/inner/deeper"))
        .expect("empty dirs");
    fs.create_dir_all(&root.join("also-empty"))
        .expect("empty dir");
    fs
}

#[test]
fn typical_corpus_streams_are_identical_across_modes() {
    let fs = typical_corpus(0xC0FFEE);
    assert_all_modes_equal(&fs, &ReconcileScope::Full, &HashSet::new());
    // Sanity: the corpora actually exercise every interesting branch.
    let (messages, result) = run(&fs, &ReconcileScope::Full, &HashSet::new(), None);
    let batches = messages
        .iter()
        .filter(|message| matches!(message, WorkerMessage::Batch { .. }))
        .count();
    let progress = messages
        .iter()
        .filter(|message| matches!(message, WorkerMessage::Progress(_)))
        .count();
    assert!(batches >= 3, "batching never fired: {batches}");
    assert!(progress >= 4, "progress cadence never fired: {progress}");
    assert!(result.complete);
    assert!(
        !result.incomplete,
        "healthy corpus must not mark incomplete"
    );
    assert!(result.full_scan);
    assert_eq!(result.present_notes.len(), 402); // 400 numbered + UPPER + "with dots"
    assert!(result.present_attachments.len() >= 85); // 60 + 1 + 25 + tar.gz
    // .flavite and junk extensions are absent from both presence sets.
    let junk_present: Vec<u8> = fs
        .encode_path_key(Path::new("junk/stray-0.txt"))
        .expect("key");
    assert!(!result.present_notes.contains(&junk_present));
    assert!(!result.present_attachments.contains(&junk_present));
}

/// Unicode + pathological names: emoji, combining marks, RTL, embedded
/// control-ish bytes that survive OS path rules, non-UTF8 bytes, leading
/// dots and dashes, and a name that looks like a path-key.
fn unicode_corpus() -> MemFs {
    let fs = MemFs::new();
    let root = PathBuf::from(ROOT);
    fs.create_dir_all(&root).expect("root");
    let names: Vec<PathBuf> = vec![
        "физика/квант.md".into(),
        "日本語/ノート.md".into(),
        "emoji/📝-note.md".into(),
        "é/é­-combining.md".into(),
        "عربي/ملاحظة.md".into(),
        "ko/한글-노트-01.md".into(),
        "dots/...hidden.md".into(),
        "-dash/lead-.md".into(),
        "unicode mix/naïve — café.md".into(),
        "pics/🖼 picture.png".into(),
        "pics/über-pic.jpg".into(),
    ];
    for name in &names {
        let path = root.join(name);
        fs.create_dir_all(path.parent().expect("dir"))
            .expect("mkdir");
        let body = format!("# {name:?}\nlinks [[квант]] [[日本語]]\n");
        fs.write_new(&path, body.as_bytes()).expect("write");
    }
    let mut raw_bytes: Vec<u8> = b"binance/".to_vec();
    raw_bytes.extend_from_slice(&[0xf0, 0x9f, 0xa7, 0xbf]);
    raw_bytes.extend_from_slice(b"-note.md");
    let raw = PathBuf::from(std::ffi::OsString::from_vec(raw_bytes));
    fs.create_dir_all(&root.join("binance")).expect("mkdir");
    fs.write_new(&root.join(raw), b"# raw bytes".as_slice())
        .expect("write raw name");
    fs
}

#[test]
fn unicode_and_pathological_names_streams_are_identical_across_modes() {
    let fs = unicode_corpus();
    assert_all_modes_equal(&fs, &ReconcileScope::Full, &HashSet::new());
    let (_, result) = run(&fs, &ReconcileScope::Full, &HashSet::new(), None);
    assert!(!result.incomplete, "names alone are never incomplete");
    assert_eq!(result.present_notes.len(), 10);
    assert_eq!(result.present_attachments.len(), 2);
}

/// Parse failures (invalid UTF-8 bodies), the oversize listing route
/// (> MAX_NOTE_BYTES), a failing attachment read is exercised separately.
fn failure_mix_corpus() -> MemFs {
    let fs = MemFs::new();
    let root = PathBuf::from(ROOT);
    fs.create_dir_all(&root).expect("root");
    fs.create_dir_all(&root.join("broken")).expect("broken dir");
    for index in 0..150 {
        fs.write_new(
            &root.join(format!("broken/{index}.md")),
            &[0xff, 0xfe, 0x00, 0xc3, 0x28, index as u8],
        )
        .expect("invalid utf-8 note");
    }
    // Oversize note: present but listed unparsed, bytes absent, NOT incomplete.
    let oversized = vec![b'a'; flavite_index::MAX_NOTE_BYTES + 1];
    fs.write_new(&root.join("huge.md"), &oversized)
        .expect("oversize write");
    // A zero-byte note: parses to Some(empty)? (deterministic either way).
    fs.write_new(&root.join("empty.md"), b"")
        .expect("empty note");
    fs
}

#[test]
fn parse_failure_and_oversize_mix_streams_are_identical_across_modes() {
    let fs = failure_mix_corpus();
    assert_all_modes_equal(&fs, &ReconcileScope::Full, &HashSet::new());
    let (messages, result) = run(&fs, &ReconcileScope::Full, &HashSet::new(), None);
    assert!(
        !result.incomplete,
        "oversize/invalid content is data, not drift"
    );
    let huge_key = fs.encode_path_key(Path::new("huge.md")).expect("key");
    assert!(result.present_notes.contains(&huge_key));
    // The completed result drains its batch into Batch messages; the
    // oversize route must list the note UNPARSED with no bytes there.
    let oversize_seen = messages.iter().any(|message| match message {
        WorkerMessage::Batch { outcomes, .. } => outcomes.iter().any(|outcome| {
            matches!(outcome,
                flavite_vault::reconcile_worker::ScanOutcome::Note { path, bytes: None, parsed: None, .. }
                if path.file_name() == Some(std::ffi::OsStr::new("huge.md")))
        }),
        _ => false,
    });
    assert!(
        oversize_seen,
        "the oversize route must list unparsed without bytes"
    );
}

#[test]
fn empty_directories_only_corpus_streams_are_identical_across_modes() {
    let fs = MemFs::new();
    let root = PathBuf::from(ROOT);
    fs.create_dir_all(&root).expect("root");
    fs.create_dir_all(&root.join("a/b/c/d/e"))
        .expect("deep empties");
    fs.create_dir_all(&root.join(".flavite/journal"))
        .expect("journal");
    assert_all_modes_equal(&fs, &ReconcileScope::Full, &HashSet::new());
    let (messages, result) = run(&fs, &ReconcileScope::Full, &HashSet::new(), None);
    assert_eq!(result.total, 0);
    assert!(result.complete);
    assert!(result.outcomes.is_empty());
    assert!(matches!(messages.as_slice(), [WorkerMessage::Progress(_)]));
}

/// AC-7's "≥3 randomized fs event orders": the same logical vault built in
/// different insertion orders walks in different orders (MemFs preserves
/// directory insertion order), and nominated jobs enumerate their set in a
/// further distinct order. Every order must produce the identical
/// serial/parallel match.
#[test]
fn randomized_orders_three_times_stream_identical() {
    for seed in [1u64, 0xDeadBeef, 0b1010_1010_1010_1010] {
        let fs = typical_corpus(seed.wrapping_mul(7919) | 1);
        assert_all_modes_equal(&fs, &ReconcileScope::Full, &HashSet::new());
        // Nominated subset jobs: the requested-path enumeration order is
        // the HashSet iteration order of this exact set, shared by modes.
        let mut nominated: HashSet<PathBuf> = HashSet::new();
        let mut rng = Rng(seed | 3);
        let mut wanted: Vec<PathBuf> = (0..200)
            .map(|index| fs_root().join(format!("a/note-{index:04}.md")))
            .collect();
        rng.shuffle(&mut wanted);
        nominated.extend(wanted.into_iter().take(140));
        nominated.insert(fs_root().join("pics/pic.png"));
        nominated.insert(fs_root().join("missing/deleted.md"));
        assert_all_modes_equal(&fs, &ReconcileScope::Full, &nominated);
    }
}

fn fs_root() -> PathBuf {
    PathBuf::from(ROOT)
}

#[test]
fn nominated_directory_falls_back_to_full_scan_identically() {
    let fs = typical_corpus(424242);
    let mut nominated = HashSet::new();
    nominated.insert(fs_root().join("a")); // a directory ⇒ fallback full
    assert_all_modes_equal(&fs, &ReconcileScope::Full, &nominated);
    let (_, result) = run(&fs, &ReconcileScope::Full, &nominated, None);
    assert!(result.full_scan, "the fallback must present as a full scan");
}

#[test]
fn single_scope_streams_are_identical() {
    let fs = typical_corpus(77);
    let scope = ReconcileScope::Single(fs_root().join("note-0003.md"));
    assert_all_modes_equal(&fs, &scope, &HashSet::new());
    let (_, result) = run(&fs, &scope, &HashSet::new(), None);
    assert!(!result.full_scan);
    assert_eq!(result.total, 1);
}

#[test]
fn out_of_root_nominated_path_marks_incomplete_identically() {
    let fs = typical_corpus(99);
    let mut nominated = HashSet::new();
    nominated.insert(PathBuf::from("/etc/somewhere-else.md"));
    assert_all_modes_equal(&fs, &ReconcileScope::Full, &nominated);
    let (_, result) = run(&fs, &ReconcileScope::Full, &nominated, None);
    assert!(
        result.incomplete,
        "a path outside the root must be incomplete"
    );
}

#[test]
fn missing_and_vanished_paths_emit_identical_missing_outcomes() {
    let fs = typical_corpus(5150);
    let mut nominated = HashSet::new();
    for index in 0..50 {
        nominated.insert(fs_root().join(format!("gone/{index}.md")));
    }
    assert_all_modes_equal(&fs, &ReconcileScope::Full, &nominated);
    let (messages, _) = run(&fs, &ReconcileScope::Full, &nominated, None);
    let missing = messages
        .iter()
        .filter_map(|message| match message {
            WorkerMessage::Batch { outcomes, .. } => Some(outcomes),
            _ => None,
        })
        .flatten()
        .filter(|outcome| {
            matches!(
                outcome,
                flavite_vault::reconcile_worker::ScanOutcome::Missing(_)
            )
        })
        .count();
    assert_eq!(missing, 50, "every vanished path must report Missing");
}
