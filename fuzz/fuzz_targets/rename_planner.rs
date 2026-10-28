//! Fuzz target for rename planner (C12, §10.6).
//!
//! This target tests the rename planner with fuzzed inputs.
//! For now, this is a minimal stub that will be expanded.

#![no_main]

use libfuzzer_sys::arbitrary::{Arbitrary, Result, Unstructured};
use flavite_fs::{Fs, MemFs};
use flavite_vault::RenamePlanner;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Minimal input for rename planner fuzzing.
#[derive(Debug)]
struct RenamePlannerInput {
    from_path: String,
    to_path: String,
}

impl<'a> Arbitrary<'a> for RenamePlannerInput {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let from_path: String = String::arbitrary(u)?;
        let to_path: String = String::arbitrary(u)?;
        Ok(Self {
            from_path: from_path.chars().take(100).collect(),
            to_path: to_path.chars().take(100).collect(),
        })
    }
}

libfuzzer_sys::fuzz_target!(|input: RenamePlannerInput| {
    let fs = MemFs::new();
    let _ = fs.create_dir_all(Path::new("/vault"));
    let _ = fs.write_new(Path::new("/vault/source.md"), b"bounded payload");
    let planner = RenamePlanner::new(std::sync::Arc::new(fs), PathBuf::from("/vault"), 1024);
    let _ = planner.plan_rename(
        PathBuf::from(input.from_path),
        PathBuf::from(input.to_path),
        &HashMap::new(),
        &HashMap::new(),
    );
});
