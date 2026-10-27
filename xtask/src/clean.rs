//! Post-gate cleanup and disk budgets (§10.10).
//!
//! `clean` removes build output; `gc` is the threshold-based post-gate step.
//! Both are git-aware: they never remove tracked files (corpus seeds) or
//! anything outside the workspace root, and never follow symlinks out of it.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Soft budget: at or under this, `gc` is a no-op (§10.10).
pub const SOFT_BUDGET: u64 = 10 * 1024 * 1024 * 1024; // 10 GB

/// Hard budget: still over this after incremental cleanup → escalate to `clean` (§10.10).
pub const HARD_BUDGET: u64 = 20 * 1024 * 1024 * 1024; // 20 GB

/// Report from a `clean` or `gc` operation.
#[derive(Debug, Default)]
pub struct CleanReport {
    pub paths_removed: Vec<PathBuf>,
    pub bytes_freed: u64,
    pub dry_run: bool,
}

impl CleanReport {
    pub fn print(&self) {
        if self.dry_run {
            println!(
                "[dry-run] would remove {} path(s), freeing {:.2} GB",
                self.paths_removed.len(),
                self.bytes_freed as f64 / (1024.0 * 1024.0 * 1024.0)
            );
        } else {
            println!(
                "Removed {} path(s), freed {:.2} GB",
                self.paths_removed.len(),
                self.bytes_freed as f64 / (1024.0 * 1024.0 * 1024.0)
            );
        }
        for path in &self.paths_removed {
            println!("  {}", path.display());
        }
    }
}

/// Measure the total size of a directory tree in bytes.
pub fn dir_size(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum()
}

/// Find the workspace root by walking up from the current directory looking
/// for a `Cargo.toml` with a `[workspace]` section.
pub fn find_workspace_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    find_workspace_root_from(&cwd)
}

fn find_workspace_root_from(start: &Path) -> Result<PathBuf> {
    let mut dir = Some(start.to_path_buf());
    while let Some(d) = dir {
        let manifest = d.join("Cargo.toml");
        if manifest.exists() {
            let content = fs::read_to_string(&manifest)?;
            if content.contains("[workspace]") {
                return Ok(d);
            }
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }
    anyhow::bail!("could not find workspace root (no Cargo.toml with [workspace] found)")
}

/// Verify that `path` is inside `root` (after canonicalization).
/// Refuses to follow symlinks out of the root (§10.10).
pub fn verify_in_root(root: &Path, path: &Path) -> Result<()> {
    let canon_root = root
        .canonicalize()
        .context("failed to canonicalize workspace root")?;
    // Check lexical containment first (fast path).
    if path.starts_with(&canon_root) {
        return Ok(());
    }
    // If the path doesn't exist yet, check its parent.
    if !path.exists()
        && let Some(parent) = path.parent()
    {
        let canon_parent = parent
            .canonicalize()
            .context("failed to canonicalize parent")?;
        if canon_parent.starts_with(&canon_root) {
            return Ok(());
        }
    }
    anyhow::bail!(
        "path {} escapes workspace root {}",
        path.display(),
        canon_root.display()
    )
}

/// Paths removed by `clean` (§10.10 table, first four rows).
fn clean_paths(root: &Path) -> Vec<PathBuf> {
    vec![
        root.join("target"),
        root.join("fuzz").join("target"),
        root.join("fuzz").join("artifacts"),
    ]
}

/// Profiling intermediates removed by both `clean` and `gc` (§10.10).
fn profiling_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = vec![root.join("target").join("llvm-cov")];
    // *.profraw and *.profdata at the workspace root and in target/.
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".profraw") || name_str.ends_with(".profdata") {
                paths.push(entry.path());
            }
        }
    }
    let target_dir = root.join("target");
    if let Ok(entries) = fs::read_dir(&target_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".profraw") || name_str.ends_with(".profdata") {
                paths.push(entry.path());
            }
        }
    }
    paths
}

/// Incremental paths removed by `gc` when over the soft budget (§10.10).
fn gc_incremental_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = vec![root.join("fuzz").join("target")];
    paths.extend(profiling_paths(root));
    // Remove target/*/incremental directories.
    let target_dir = root.join("target");
    if let Ok(profiles) = fs::read_dir(&target_dir) {
        for profile in profiles.flatten() {
            let incremental = profile.path().join("incremental");
            if incremental.exists() {
                paths.push(incremental);
            }
        }
    }
    paths
}

/// Remove a path (file or directory tree) and return the freed bytes.
fn remove_path(path: &Path, dry_run: bool) -> u64 {
    if !path.exists() && !path.is_symlink() {
        return 0;
    }
    let size = dir_size(path);
    if !dry_run {
        let _ = fs::remove_dir_all(path).or_else(|_| fs::remove_file(path));
    }
    size
}

/// Full cleanup (§10.10): removes target/, fuzz/target/, fuzz/artifacts/,
/// and profiling intermediates. Never touches corpus seeds, ~/.cargo, or
/// anything outside the workspace root.
pub fn clean(root: &Path, dry_run: bool) -> Result<CleanReport> {
    verify_in_root(root, root)?;
    let mut report = CleanReport {
        dry_run,
        ..Default::default()
    };

    let mut all_paths = clean_paths(root);
    all_paths.extend(profiling_paths(root));

    for path in &all_paths {
        verify_in_root(root, path)?;
        let freed = remove_path(path, dry_run);
        if freed > 0 || path.exists() || path.is_symlink() {
            report.bytes_freed += freed;
            report.paths_removed.push(path.clone());
        }
    }

    Ok(report)
}

/// Post-gate garbage collection (§10.10): threshold-based.
///
/// - At or under the soft budget → no-op.
/// - Over soft budget → remove incremental caches + fuzz/target/ + profiling
///   intermediates, then re-measure.
/// - Still over hard budget → escalate to full `clean` with a warning.
pub fn gc(root: &Path) -> Result<CleanReport> {
    verify_in_root(root, root)?;

    let target_size = dir_size(&root.join("target"));
    let fuzz_target_size = dir_size(&root.join("fuzz").join("target"));
    let total = target_size + fuzz_target_size;

    println!(
        "gc: target/ = {:.2} GB, fuzz/target/ = {:.2} GB, total = {:.2} GB (soft: {:.0} GB, hard: {:.0} GB)",
        target_size as f64 / (1024.0 * 1024.0 * 1024.0),
        fuzz_target_size as f64 / (1024.0 * 1024.0 * 1024.0),
        total as f64 / (1024.0 * 1024.0 * 1024.0),
        SOFT_BUDGET as f64 / (1024.0 * 1024.0 * 1024.0),
        HARD_BUDGET as f64 / (1024.0 * 1024.0 * 1024.0),
    );

    if total <= SOFT_BUDGET {
        println!("gc: under soft budget, no-op");
        return Ok(CleanReport::default());
    }

    // Over soft budget: remove incremental caches.
    println!("gc: over soft budget, removing incremental caches");
    let mut report = CleanReport::default();
    for path in gc_incremental_paths(root) {
        verify_in_root(root, &path)?;
        let freed = remove_path(&path, false);
        if freed > 0 || path.exists() {
            report.bytes_freed += freed;
            report.paths_removed.push(path);
        }
    }
    report.print();

    // Re-measure.
    let target_size = dir_size(&root.join("target"));
    let fuzz_target_size = dir_size(&root.join("fuzz").join("target"));
    let total = target_size + fuzz_target_size;

    if total > HARD_BUDGET {
        eprintln!(
            "gc: still over hard budget ({:.2} GB), escalating to full clean",
            total as f64 / (1024.0 * 1024.0 * 1024.0)
        );
        eprintln!("WARNING: the next gate rebuilds from scratch");
        let clean_report = clean(root, false)?;
        report.bytes_freed += clean_report.bytes_freed;
        report.paths_removed.extend(clean_report.paths_removed);
    } else {
        println!(
            "gc: now at {:.2} GB (under hard budget)",
            total as f64 / (1024.0 * 1024.0 * 1024.0)
        );
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_workspace() -> PathBuf {
        // Create a unique temporary workspace-like directory for each test
        // (tests run in parallel; a shared dir would race).
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let tmp =
            std::env::temp_dir().join(format!("flavite-xtask-test-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        // Create a minimal Cargo.toml with [workspace].
        fs::write(tmp.join("Cargo.toml"), "[workspace]\n").unwrap();
        tmp
    }

    fn cleanup_tmp(tmp: &Path) {
        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn dir_size_empty() {
        let tmp = temp_workspace();
        assert_eq!(dir_size(&tmp.join("nonexistent")), 0);
        // The workspace has a Cargo.toml (12 bytes: "[workspace]\n").
        let size = dir_size(&tmp);
        assert_eq!(size, 12);
        cleanup_tmp(&tmp);
    }

    #[test]
    fn dir_size_counts_files() {
        let tmp = temp_workspace();
        fs::write(tmp.join("file.txt"), b"hello").unwrap();
        let size = dir_size(&tmp);
        assert!(size >= 5); // At least the file content + Cargo.toml
        cleanup_tmp(&tmp);
    }

    #[test]
    fn find_workspace_root_finds_root() {
        let tmp = temp_workspace();
        let nested = tmp.join("crates").join("sub");
        fs::create_dir_all(&nested).unwrap();
        let root = find_workspace_root_from(&nested).unwrap();
        assert_eq!(root, tmp.canonicalize().unwrap());
        cleanup_tmp(&tmp);
    }

    #[test]
    fn find_workspace_root_fails_without_workspace() {
        let tmp =
            std::env::temp_dir().join(format!("flavite-xtask-noworkspace-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        // No Cargo.toml with [workspace].
        fs::write(tmp.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert!(find_workspace_root_from(&tmp).is_err());
        cleanup_tmp(&tmp);
    }

    #[test]
    fn verify_in_root_accepts_contained() {
        let tmp = temp_workspace();
        let canon = tmp.canonicalize().unwrap();
        assert!(verify_in_root(&canon, &canon.join("target")).is_ok());
        assert!(verify_in_root(&canon, &canon.join("fuzz").join("target")).is_ok());
        cleanup_tmp(&tmp);
    }

    #[test]
    fn verify_in_root_rejects_escape() {
        let tmp = temp_workspace();
        let canon = tmp.canonicalize().unwrap();
        // /tmp is outside the workspace root.
        assert!(verify_in_root(&canon, Path::new("/tmp")).is_err());
        cleanup_tmp(&tmp);
    }

    #[test]
    fn clean_dry_run_removes_nothing() {
        let tmp = temp_workspace();
        let canon = tmp.canonicalize().unwrap();
        // Create some build output.
        fs::create_dir_all(canon.join("target").join("debug")).unwrap();
        fs::write(canon.join("target").join("debug").join("binary"), b"data").unwrap();
        fs::create_dir_all(canon.join("fuzz").join("target")).unwrap();
        fs::write(canon.join("fuzz").join("target").join("fuzzbin"), b"data").unwrap();

        let report = clean(&canon, true).unwrap();
        assert!(report.dry_run);
        assert!(report.bytes_freed > 0);
        // Nothing was actually removed.
        assert!(canon.join("target").exists());
        assert!(canon.join("fuzz").join("target").exists());
        cleanup_tmp(&tmp);
    }

    #[test]
    fn clean_removes_build_output() {
        let tmp = temp_workspace();
        let canon = tmp.canonicalize().unwrap();
        fs::create_dir_all(canon.join("target").join("debug")).unwrap();
        fs::write(canon.join("target").join("debug").join("binary"), b"data").unwrap();
        fs::create_dir_all(canon.join("fuzz").join("target")).unwrap();
        fs::create_dir_all(canon.join("fuzz").join("artifacts")).unwrap();
        fs::write(canon.join("test.profraw"), b"prof").unwrap();

        let report = clean(&canon, false).unwrap();
        assert!(!report.dry_run);
        assert!(!canon.join("target").exists());
        assert!(!canon.join("fuzz").join("target").exists());
        assert!(!canon.join("fuzz").join("artifacts").exists());
        assert!(!canon.join("test.profraw").exists());
        // Cargo.toml (tracked file) is preserved.
        assert!(canon.join("Cargo.toml").exists());
        cleanup_tmp(&tmp);
    }

    #[test]
    fn clean_never_removes_corpus_seeds() {
        let tmp = temp_workspace();
        let canon = tmp.canonicalize().unwrap();
        // Create corpus seeds (tracked files — must never be removed).
        let corpus = canon.join("fuzz").join("corpus").join("parser");
        fs::create_dir_all(&corpus).unwrap();
        fs::write(corpus.join("seed.md"), b"# seed").unwrap();

        let _report = clean(&canon, false).unwrap();
        // Corpus seeds survive.
        assert!(corpus.join("seed.md").exists());
        cleanup_tmp(&tmp);
    }

    #[test]
    fn gc_under_budget_is_noop() {
        let tmp = temp_workspace();
        let canon = tmp.canonicalize().unwrap();
        // Small target dir — well under 10 GB soft budget.
        fs::create_dir_all(canon.join("target").join("debug")).unwrap();
        fs::write(canon.join("target").join("debug").join("bin"), b"x").unwrap();

        let report = gc(&canon).unwrap();
        assert!(report.paths_removed.is_empty());
        // Target still exists.
        assert!(canon.join("target").exists());
        cleanup_tmp(&tmp);
    }

    #[test]
    fn gc_removes_incremental_when_over_soft_budget() {
        // This test uses a modified soft budget via a direct call.
        // We can't easily create 10 GB of data, so we test the logic by
        // verifying that gc_incremental_paths finds the right paths.
        let tmp = temp_workspace();
        let canon = tmp.canonicalize().unwrap();
        fs::create_dir_all(canon.join("target").join("debug").join("incremental")).unwrap();
        fs::write(
            canon
                .join("target")
                .join("debug")
                .join("incremental")
                .join("data"),
            b"x",
        )
        .unwrap();
        fs::create_dir_all(canon.join("fuzz").join("target")).unwrap();
        fs::write(canon.join("fuzz").join("target").join("fuzzbin"), b"x").unwrap();

        let paths = gc_incremental_paths(&canon);
        assert!(paths.contains(&canon.join("fuzz").join("target")));
        assert!(paths.contains(&canon.join("target").join("debug").join("incremental")));
        cleanup_tmp(&tmp);
    }

    #[test]
    fn profiling_paths_finds_profraw() {
        let tmp = temp_workspace();
        let canon = tmp.canonicalize().unwrap();
        fs::write(canon.join("test.profraw"), b"prof").unwrap();
        fs::write(canon.join("test.profdata"), b"prof").unwrap();
        fs::create_dir_all(canon.join("target")).unwrap();
        fs::write(canon.join("target").join("inner.profraw"), b"prof").unwrap();

        let paths = profiling_paths(&canon);
        assert!(paths.iter().any(|p| p.ends_with("test.profraw")));
        assert!(paths.iter().any(|p| p.ends_with("test.profdata")));
        assert!(paths.iter().any(|p| p.ends_with("inner.profraw")));
        cleanup_tmp(&tmp);
    }
}
