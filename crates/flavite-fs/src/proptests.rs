//! Property tests (proptest, in-crate — §10.6).
//!
//! - **MemFs oracle**: random op sequences over `MemFs` vs. a naive
//!   `HashMap<PathBuf, Vec<u8>>` oracle — reads agree, existence agrees.
//! - **FaultFs invariants**: `FaultFs<MemFs>` with random fault scripts —
//!   every returned error is typed (no panics); dead processes cannot
//!   continue; the trace records every attempted boundary.

#[cfg(test)]
mod mem_oracle {
    use crate::fs::{Fs, FsError};
    use crate::mem::MemFs;
    use proptest::prelude::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// A random filesystem operation over a small path space.
    #[derive(Debug, Clone)]
    enum Op {
        Write(PathBuf, Vec<u8>),
        Read(PathBuf),
        Exists(PathBuf),
        Remove(PathBuf),
        Rename(PathBuf, PathBuf),
        ReadCapped(PathBuf, u64),
        Metadata(PathBuf),
    }

    fn path_strategy() -> impl Strategy<Value = PathBuf> {
        (0..8usize).prop_map(|i| PathBuf::from(format!("/file{i}.md")))
    }

    fn bytes_strategy() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(any::<u8>(), 0..64)
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        let write = (path_strategy(), bytes_strategy()).prop_map(|(p, b)| Op::Write(p, b));
        let read = path_strategy().prop_map(Op::Read);
        let exists = path_strategy().prop_map(Op::Exists);
        let remove = path_strategy().prop_map(Op::Remove);
        let rename = (path_strategy(), path_strategy()).prop_map(|(a, b)| Op::Rename(a, b));
        let read_capped = (path_strategy(), 0u64..128).prop_map(|(p, m)| Op::ReadCapped(p, m));
        let metadata = path_strategy().prop_map(Op::Metadata);
        prop_oneof![
            3 => write,
            3 => read,
            2 => exists,
            2 => remove,
            1 => rename,
            1 => read_capped,
            1 => metadata,
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Random op sequences over MemFs vs. a naive oracle.
        /// Reads agree, existence agrees after every operation.
        #[test]
        fn memfs_matches_naive_oracle(ops in proptest::collection::vec(op_strategy(), 0..100)) {
            let fs = MemFs::new();
            let mut oracle: HashMap<PathBuf, Vec<u8>> = HashMap::new();

            for op in &ops {
                match op {
                    Op::Write(path, bytes) => {
                        let result = fs.write_new(path, bytes);
                        if oracle.contains_key(path) {
                            // AlreadyExists — MemFs must reject.
                            prop_assert!(
                                matches!(result, Err(FsError::AlreadyExists(_))),
                                "write_new to existing path must fail: {path:?}"
                            );
                        } else {
                            prop_assert!(result.is_ok(), "write_new failed: {result:?}");
                            oracle.insert(path.clone(), bytes.clone());
                        }
                    }
                    Op::Read(path) => {
                        let result = fs.read_if_exists(path).unwrap();
                        prop_assert_eq!(
                            &result,
                            &oracle.get(path).cloned(),
                            "read_if_exists disagrees with oracle for {:?}",
                            path
                        );
                    }
                    Op::Exists(path) => {
                        let result = fs.exists(path).unwrap();
                        prop_assert_eq!(
                            result,
                            oracle.contains_key(path),
                            "exists disagrees with oracle for {:?}",
                            path
                        );
                    }
                    Op::Remove(path) => {
                        let result = fs.remove(path);
                        if oracle.contains_key(path) {
                            prop_assert!(result.is_ok(), "remove failed: {result:?}");
                            oracle.remove(path);
                        } else {
                            prop_assert!(
                                matches!(result, Err(FsError::NotFound(_))),
                                "remove of missing path must fail: {result:?}"
                            );
                        }
                    }
                    Op::Rename(from, to) => {
                        let result = fs.rename_replace(from, to);
                        match (oracle.contains_key(from), oracle.contains_key(to)) {
                            (true, _) => {
                                prop_assert!(result.is_ok(), "rename failed: {result:?}");
                                let content = oracle.remove(from).unwrap();
                                oracle.insert(to.clone(), content);
                            }
                            (false, _) => {
                                prop_assert!(
                                    matches!(result, Err(FsError::NotFound(_))),
                                    "rename of missing source must fail: {result:?}"
                                );
                            }
                        }
                    }
                    Op::ReadCapped(path, max) => {
                        let result = fs.read_capped(path, *max);
                        match oracle.get(path) {
                            Some(content) if content.len() as u64 > *max => {
                                prop_assert!(
                                    matches!(result, Err(FsError::ReadTooLarge { .. })),
                                    "read_capped over cap must fail"
                                );
                            }
                            Some(content) => {
                                prop_assert_eq!(
                                    result.unwrap(),
                                    content.clone(),
                                    "read_capped disagrees with oracle"
                                );
                            }
                            None => {
                                prop_assert!(
                                    matches!(result, Err(FsError::NotFound(_))),
                                    "read_capped of missing must fail"
                                );
                            }
                        }
                    }
                    Op::Metadata(path) => {
                        let result = fs.metadata(path);
                        if let Some(content) = oracle.get(path) {
                            let md = result.unwrap();
                            prop_assert_eq!(md.size, content.len() as u64);
                            prop_assert!(!md.is_dir);
                            prop_assert!(!md.is_symlink);
                        } else {
                            prop_assert!(
                                matches!(result, Err(FsError::NotFound(_))),
                                "metadata of missing must fail"
                            );
                        }
                    }
                }
            }

            // Final full verification: every oracle entry is readable and correct.
            for (path, expected) in &oracle {
                prop_assert_eq!(
                    fs.read_if_exists(path).unwrap(),
                    Some(expected.clone()),
                    "final read disagrees for {:?}",
                    path
                );
                prop_assert!(
                    fs.exists(path).unwrap(),
                    "final exists disagrees for {:?}",
                    path
                );
            }
        }
    }
}

#[cfg(all(test, feature = "fault"))]
mod fault_invariants {
    use crate::fault::{FaultBoundary, FaultFs, FaultKind, FaultScript, TraceOutcome};
    use crate::fs::{Fs, FsError};
    use crate::mem::MemFs;
    use proptest::prelude::*;
    use std::path::PathBuf;

    fn crash_kind_strategy() -> impl Strategy<Value = FaultKind> {
        prop::sample::select(vec![FaultKind::ProcessCrash, FaultKind::PowerLoss])
    }

    fn fault_kind_strategy() -> impl Strategy<Value = FaultKind> {
        prop::sample::select(vec![
            FaultKind::Error,
            FaultKind::FsyncFails,
            FaultKind::FsyncUnsupported,
            FaultKind::ProcessCrash,
            FaultKind::PowerLoss,
            FaultKind::ExternalWrite,
            FaultKind::PartialWriteThenError,
            FaultKind::ShortWrite,
        ])
    }

    fn boundary_strategy() -> impl Strategy<Value = FaultBoundary> {
        prop::sample::select(vec![
            FaultBoundary::Before,
            FaultBoundary::After,
            FaultBoundary::Progress,
        ])
    }

    fn method_strategy() -> impl Strategy<Value = &'static str> {
        prop::sample::select(vec![
            "write_new",
            "read_if_exists",
            "exists",
            "remove",
            "fsync_file",
            "fsync_dir",
            "rename_replace",
            "create_dir_all",
            "metadata",
            "read_dir",
        ])
    }

    fn fault_script_strategy() -> impl Strategy<Value = FaultScript> {
        (1..5usize)
            .prop_flat_map(|n| {
                proptest::collection::vec(
                    (
                        method_strategy(),
                        1..10usize,
                        boundary_strategy(),
                        fault_kind_strategy(),
                    ),
                    n..=n,
                )
            })
            .prop_map(|faults| {
                let mut script = FaultScript::new();
                for (method, occurrence, boundary, kind) in faults {
                    script = script.fault_at(method, occurrence, boundary, kind);
                }
                script
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        /// FaultFs<MemFs> with random fault scripts — invariants hold:
        /// - No panics (every error is a typed `FsError`).
        /// - Dead processes cannot continue (all ops return `ProcessDead`).
        /// - The trace records every attempted boundary.
        #[test]
        fn faultfs_invariants_hold(script in fault_script_strategy(), num_ops in 1..30usize) {
            let fs = FaultFs::new_mem(MemFs::new(), script);

            let mut crashed = false;
            for i in 0..num_ops {
                let path = PathBuf::from(format!("/file{i}.md"));
                let result = fs.write_new(&path, b"data");

                if crashed {
                    // Dead process: every operation must return ProcessDead.
                    prop_assert!(
                        matches!(result, Err(FsError::ProcessDead)),
                        "dead process must reject all operations, got: {result:?}"
                    );
                    prop_assert!(!fs.is_alive());
                    continue;
                }

                match result {
                    Ok(()) => {
                        prop_assert!(fs.is_alive());
                    }
                    Err(FsError::ProcessDead) => {
                        crashed = true;
                        prop_assert!(!fs.is_alive());
                    }
                    Err(_) => {
                        // Other typed errors are acceptable (injected Error, FsyncFails, etc.).
                    }
                }

                // If alive, reads should also work (or return typed errors).
                if fs.is_alive() {
                    let _ = fs.read_if_exists(&path);
                    let _ = fs.exists(&path);
                } else {
                    // Dead: reads must fail with ProcessDead.
                    let err = fs.read_if_exists(&path).unwrap_err();
                    prop_assert!(matches!(err, FsError::ProcessDead));
                    let err = fs.exists(&path).unwrap_err();
                    prop_assert!(matches!(err, FsError::ProcessDead));
                }
            }

            // Invariant: the trace is recorded.
            let trace = fs.trace();
            // Every write_new attempt should have at least one trace entry.
            let write_count = fs.call_count("write_new");
            if write_count > 0 {
                let write_traces = trace.iter().filter(|e| e.method == "write_new").count();
                prop_assert!(
                    write_traces > 0,
                    "trace must record write_new attempts (count={write_count})"
                );
            }
        }

        /// A crash at occurrence N means the first N-1 calls succeed (or fail
        /// with non-crash errors) and the Nth call crashes.
        #[test]
        fn crash_occurs_at_expected_point(
            crash_at in 1..15usize,
            kind in crash_kind_strategy(),
        ) {
            let script = FaultScript::new().fault_at(
                "write_new",
                crash_at,
                FaultBoundary::Before,
                kind,
            );
            let fs = FaultFs::new_mem(MemFs::new(), script);

            let mut crashed_at = None;
            for i in 1..=crash_at + 3 {
                let path = PathBuf::from(format!("/f{i}.md"));
                let result = fs.write_new(&path, b"x");
                if matches!(result, Err(FsError::ProcessDead)) && crashed_at.is_none() {
                    crashed_at = Some(i);
                }
                if crashed_at.is_some() {
                    // All subsequent calls must also be ProcessDead.
                    prop_assert!(
                        matches!(result, Err(FsError::ProcessDead)),
                        "after crash at {i}, all ops must be ProcessDead, got {result:?}"
                    );
                }
            }
            prop_assert_eq!(
                crashed_at,
                Some(crash_at),
                "crash must occur at the scripted occurrence"
            );
        }

        /// ProcessCrash retains volatile state; PowerLoss may discard it.
        /// This is the core AC-36 distinction.
        #[test]
        fn crash_models_differ_at_unsynced_writes(
            kind in crash_kind_strategy(),
            fsync_file in any::<bool>(),
            fsync_dir in any::<bool>(),
        ) {
            let mem = MemFs::new();
            let fs = FaultFs::new_mem(mem.clone(), FaultScript::new());
            fs.create_dir_all(std::path::Path::new("/vault")).unwrap();
            if fsync_dir {
                fs.fsync_dir(std::path::Path::new("/")).unwrap();
                fs.fsync_dir(std::path::Path::new("/vault")).unwrap();
            }
            fs.write_new(std::path::Path::new("/vault/note.md"), b"data").unwrap();
            if fsync_file {
                fs.fsync_file(std::path::Path::new("/vault/note.md")).unwrap();
            }
            if fsync_dir {
                // Re-fsync dir after the write to persist the new entry.
                fs.fsync_dir(std::path::Path::new("/vault")).unwrap();
            }

            // Crash.
            let script = FaultScript::new().fault_at(
                "exists",
                1,
                FaultBoundary::Before,
                kind,
            );
            let fs2 = FaultFs::new_mem(mem.clone(), script);
            let _ = fs2.exists(std::path::Path::new("/"));
            prop_assert!(!fs2.is_alive());

            let fs3 = fs2.revive();
            // Check content, not just existence: a file "survives" with correct
            // content iff BOTH the file content and the directory entry were
            // fsync'd. (A directory entry without fsync'd content reads as empty.)
            let content = fs3.read_if_exists(std::path::Path::new("/vault/note.md")).unwrap();
            let content_ok = content == Some(b"data".to_vec());

            match kind {
                FaultKind::ProcessCrash => {
                    // ProcessCrash retains ALL volatile state (synced or not).
                    prop_assert!(
                        content_ok,
                        "ProcessCrash must retain unsynced state (content lost)"
                    );
                }
                FaultKind::PowerLoss => {
                    // PowerLoss retains only durable state.
                    // The file content survives only if BOTH the file content and
                    // the directory entry were fsync'd.
                    let expected = fsync_file && fsync_dir;
                    prop_assert_eq!(
                        content_ok, expected,
                        "PowerLoss: content survives iff file+dir were fsync'd (file={}, dir={})",
                        fsync_file, fsync_dir
                    );
                }
                _ => unreachable!(),
            }
        }

        /// Every fault kind at the write_new Progress boundary produces
        /// a typed result (no panic).
        #[test]
        fn write_progress_faults_are_typed(kind in fault_kind_strategy()) {
            let boundary = match kind {
                FaultKind::PartialWriteThenError | FaultKind::ShortWrite => FaultBoundary::Progress,
                _ => FaultBoundary::Before,
            };
            let script = FaultScript::new().fault_at("write_new", 1, boundary, kind);
            let fs = FaultFs::new_mem(MemFs::new(), script);
            // Must not panic — returns a typed Result.
            let _ = fs.write_new(std::path::Path::new("/note.md"), b"hello world");
        }

        /// Trace outcomes are recorded for every fault kind.
        #[test]
        fn trace_records_all_fault_kinds(kind in fault_kind_strategy()) {
            // Progress-boundary faults only fire on write_new.
            let (method, boundary): (&'static str, FaultBoundary) = match kind {
                FaultKind::PartialWriteThenError | FaultKind::ShortWrite => {
                    ("write_new", FaultBoundary::Progress)
                }
                _ => ("exists", FaultBoundary::Before),
            };
            let script = FaultScript::new().fault_at(method, 1, boundary, kind);
            let fs = FaultFs::new_mem(MemFs::new(), script);
            match method {
                "write_new" => {
                    let _ = fs.write_new(std::path::Path::new("/note.md"), b"data");
                }
                _ => {
                    let _ = fs.exists(std::path::Path::new("/"));
                }
            }
            let trace = fs.trace();
            let has_fault = trace.iter().any(|e| {
                matches!(&e.outcome, TraceOutcome::Fault(k) if *k == kind)
            });
            prop_assert!(
                has_fault,
                "trace must record fault kind {:?} at {} {:?}",
                kind, method, boundary
            );
        }
    }
}
