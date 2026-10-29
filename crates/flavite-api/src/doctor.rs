//! Doctor diagnostics module (§4.11).
//!
//! This module provides comprehensive vault health checks for the `flavite doctor` command.
//!
//! # Doctor Checks
//!
//! The current checks inspect SQLite quick_check, indexed note hashes, links,
//! stable IDs, orphan temps, journals, path collisions, history size, stale
//! registry records, and orphan IPC sockets. Checks owned by unfinished slices
//! are omitted until they can report real results.
//!
//! Doctor never cleans files referenced by a live journal.

use crate::types::ApiVaultRef;
use directories::ProjectDirs;
use flavite_fs::{OsFs, list_orphan_temps};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// A doctor check result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DoctorCheck {
    /// Check name.
    pub name: String,
    /// Check description.
    pub description: String,
    /// Check status.
    pub status: CheckStatus,
    /// Details if available.
    pub details: Option<String>,
    /// Fix suggestion if available.
    pub fix: Option<String>,
}

impl DoctorCheck {
    /// Create a new check with OK status.
    #[must_use]
    pub fn ok(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            status: CheckStatus::Ok,
            details: None,
            fix: None,
        }
    }

    /// Create a new check with Warning status.
    #[must_use]
    pub fn warn(
        name: impl Into<String>,
        description: impl Into<String>,
        details: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            status: CheckStatus::Warning,
            details: Some(details.into()),
            fix: None,
        }
    }

    /// Create a new check with Error status.
    #[must_use]
    pub fn error(
        name: impl Into<String>,
        description: impl Into<String>,
        details: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            status: CheckStatus::Error,
            details: Some(details.into()),
            fix: Some(fix.into()),
        }
    }

    /// Returns true if this check passed.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self.status, CheckStatus::Ok)
    }

    /// Returns true if this check has issues.
    #[must_use]
    pub fn has_issues(&self) -> bool {
        !self.is_ok()
    }

    /// Attach diagnostic details to a check, including successful checks.
    #[must_use]
    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }
}

/// Check status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CheckStatus {
    /// Check passed.
    Ok,
    /// Check has warnings.
    Warning,
    /// Check failed.
    Error,
}

/// Doctor report for a vault.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    /// Vault reference.
    pub vault: ApiVaultRef,
    /// Timestamp of the report.
    pub timestamp: String,
    /// All checks performed.
    pub checks: Vec<DoctorCheck>,
    /// Summary statistics.
    pub summary: DoctorSummary,
}

impl DoctorReport {
    /// Create a new doctor report.
    #[must_use]
    pub fn new(vault: ApiVaultRef) -> Self {
        let timestamp = chrono::Utc::now().to_rfc3339();
        Self {
            vault,
            timestamp,
            checks: Vec::new(),
            summary: DoctorSummary::default(),
        }
    }

    /// Add a check to the report.
    pub fn add_check(&mut self, check: DoctorCheck) {
        self.checks.push(check.clone());
        match check.status {
            CheckStatus::Ok => self.summary.ok_count += 1,
            CheckStatus::Warning => self.summary.warning_count += 1,
            CheckStatus::Error => self.summary.error_count += 1,
        }
    }

    /// Returns true if all checks passed.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.summary.error_count == 0
    }

    /// Get the overall status.
    #[must_use]
    pub fn status(&self) -> CheckStatus {
        if self.summary.error_count > 0 {
            CheckStatus::Error
        } else if self.summary.warning_count > 0 {
            CheckStatus::Warning
        } else {
            CheckStatus::Ok
        }
    }

    /// Get the status as a string.
    #[must_use]
    pub fn status_as_str(&self) -> &'static str {
        match self.status() {
            CheckStatus::Ok => "OK",
            CheckStatus::Warning => "WARNING",
            CheckStatus::Error => "ERROR",
        }
    }
}

/// Doctor summary statistics.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct DoctorSummary {
    /// Number of OK checks.
    pub ok_count: u64,
    /// Number of warning checks.
    pub warning_count: u64,
    /// Number of error checks.
    pub error_count: u64,
    /// Total number of checks.
    pub total: u64,
}

impl DoctorSummary {
    /// Calculate the total.
    pub fn calculate_total(&mut self) {
        self.total = self.ok_count + self.warning_count + self.error_count;
    }
}

/// Doctor checker for checks supported by the current storage slices.
#[derive(Debug)]
pub struct DoctorChecker {
    vault: ApiVaultRef,
    db: PathBuf,
}

impl DoctorChecker {
    #[must_use]
    pub fn new(mut vault: ApiVaultRef) -> Self {
        // The vault file is authoritative. This also lets lightweight callers
        // such as the CLI recover the actual cache key without opening a
        // writing Session or acquiring the single-instance lock.
        if let Ok(Some(uuid)) = flavite_vault::identity::read_vault_toml(&vault.root) {
            vault.uuid = uuid;
        }
        let db = cache_db_path(vault.uuid);
        Self { vault, db }
    }

    #[cfg(test)]
    fn with_db_path(vault: ApiVaultRef, db: PathBuf) -> Self {
        Self { vault, db }
    }

    /// Check Tantivy generation matches SQLite.
    fn check_tantivy_generation(&self) -> DoctorCheck {
        if !self.db.exists() {
            return DoctorCheck::warn(
                "tantivy_generation",
                "Tantivy generation matches SQLite",
                "search index has not been built yet",
            );
        }
        let result = (|| -> rusqlite::Result<(String, String)> {
            let conn = rusqlite::Connection::open_with_flags(
                &self.db,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            let get = |key: &str| {
                conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |row| {
                    row.get::<_, String>(0)
                })
            };
            Ok((get("generation")?, get("tantivy_committed_generation")?))
        })();
        match result {
            Ok((generation, committed)) if generation == committed => DoctorCheck::ok(
                "tantivy_generation",
                format!("Tantivy generation {committed} matches SQLite generation {generation}"),
            ),
            Ok((generation, committed)) => DoctorCheck::warn(
                "tantivy_generation",
                "Tantivy search view is behind or ahead of SQLite",
                format!("SQLite generation={generation}, Tantivy committed_generation={committed}"),
            ),
            Err(error) => DoctorCheck::warn(
                "tantivy_generation",
                "Could not read Tantivy generation metadata",
                error.to_string(),
            ),
        }
    }

    /// Run checks against the opened vault and its read-only cache.
    pub fn run(&self) -> DoctorReport {
        let mut report = DoctorReport::new(self.vault.clone());
        let conn = Connection::open_with_flags(&self.db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok();
        report.add_check(sqlite_check(conn.as_ref(), &self.db));
        report.add_check(self.check_tantivy_generation());
        if !self.db.exists() {
            report.add_check(DoctorCheck::ok(
                "broken_links",
                "Index cache is not present; link counts unavailable",
            ));
            report.add_check(DoctorCheck::ok(
                "ambiguous_links",
                "Index cache is not present; ambiguity counts unavailable",
            ));
            report.add_check(DoctorCheck::ok(
                "duplicate_ids",
                "Index cache is not present; stable ID counts unavailable",
            ));
            report.add_check(DoctorCheck::ok(
                "attachments",
                "Index cache is not present; attachment hydration and orphan counts unavailable",
            ));
        } else {
            report.add_check(unresolved_links_check(conn.as_ref()));
            report.add_check(db_count_check(
                conn.as_ref(),
                "ambiguous_links",
                "SELECT count(*) FROM links WHERE res_state = 'ambiguous'",
                "Ambiguous links",
            ));
            report.add_check(duplicate_ids_check(conn.as_ref()));
            report.add_check(attachment_check(conn.as_ref()));
        }
        report.add_check(temp_check(&self.vault.root));
        report.add_check(journal_check(&self.vault.root));
        report.add_check(collision_check(&self.vault.root));
        report.add_check(history_check(self.vault.uuid));
        report.add_check(observability_check());
        report.add_check(runtime_counters_check());
        report.add_check(DoctorCheck::warn(
            "watcher_health",
            "Filesystem watcher health",
            "Doctor runs without starting the vault actor; use `flavite info` for the live watcher status",
        ));
        report.add_check(sample_hash_check(conn.as_ref(), &self.vault.root));
        report.add_check(registry_check());
        report.add_check(orphan_socket_check());
        report.summary.calculate_total();
        report
    }
}

/// Local diagnostics directory used by the rotating log and crash report.
#[must_use]
pub fn diagnostics_log_dir() -> Option<PathBuf> {
    ProjectDirs::from("com", "flavite", "Flavite")
        .map(|dirs| dirs.data_local_dir().join("flavite").join("logs"))
}

fn observability_check() -> DoctorCheck {
    let Some(dir) = diagnostics_log_dir() else {
        return DoctorCheck::warn(
            "observability",
            "Local logs and crash reports",
            "OS data directory unavailable",
        );
    };
    let log = dir.join("flavite.log");
    let crash = dir.join("crash-report.txt");
    if log.exists() {
        DoctorCheck::ok("observability", "Local rotating log is available").with_details(format!(
            "Log: {}; last crash report: {}",
            log.display(),
            if crash.exists() { "present" } else { "none" }
        ))
    } else {
        DoctorCheck::warn(
            "observability",
            "Local rotating log has not been created",
            format!("Expected {}", log.display()),
        )
    }
}

fn attachment_check(conn: Option<&Connection>) -> DoctorCheck {
    let Some(conn) = conn else {
        return DoctorCheck::warn(
            "attachments",
            "Attachment health",
            "Index database is unavailable",
        );
    };
    let result = conn.query_row(
        "SELECT count(*), sum(CASE WHEN hydrated = 0 THEN 1 ELSE 0 END), sum(CASE WHEN att_id NOT IN (SELECT dst_att FROM links WHERE dst_att IS NOT NULL) THEN 1 ELSE 0 END) FROM attachments",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?.unwrap_or(0), row.get::<_, Option<i64>>(2)?.unwrap_or(0))),
    );
    match result {
        Ok((total, unhydrated, orphans)) if unhydrated == 0 && orphans == 0 => DoctorCheck::ok(
            "attachments",
            "No orphaned attachments or unhydrated placeholders",
        )
        .with_details(format!(
            "{total} indexed attachment(s); 0 orphaned; 0 unhydrated"
        )),
        Ok((total, unhydrated, orphans)) => DoctorCheck::warn(
            "attachments",
            "Attachment health needs attention",
            format!("{total} indexed; {orphans} orphaned; {unhydrated} unhydrated placeholder(s)"),
        ),
        Err(error) => DoctorCheck::warn(
            "attachments",
            "Attachment health unavailable",
            error.to_string(),
        ),
    }
}

/// Locate the current schema's disposable SQLite cache for a vault identity.
#[must_use]
pub fn cache_db_path(uuid: uuid::Uuid) -> PathBuf {
    ProjectDirs::from("com", "flavite", "Flavite")
        .map(|p| {
            p.cache_dir()
                .join("flavite")
                .join(uuid.to_string())
                .join(format!("v{}", flavite_index::SCHEMA_MAJOR))
                .join("index.sqlite3")
        })
        .unwrap_or_default()
}

fn sqlite_check(conn: Option<&Connection>, path: &Path) -> DoctorCheck {
    if !path.exists() {
        return DoctorCheck::ok(
            "sqlite_quick_check",
            "Index cache has not been created; no database to check",
        );
    }
    let Some(conn) = conn else {
        return DoctorCheck::error("sqlite_quick_check", "SQLite integrity check", format!("Cannot open cache database read-only: {}", path.display()), "Open the vault once to create its index; if this database should exist, restore or rebuild the cache".to_string());
    };
    match conn.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0)) {
        Ok(result) if result == "ok" => {
            DoctorCheck::ok("sqlite_quick_check", "SQLite quick_check passed")
        }
        Ok(result) => DoctorCheck::error(
            "sqlite_quick_check",
            "SQLite quick_check failed",
            result,
            "Close Flavite and rebuild the disposable index cache".to_string(),
        ),
        Err(e) => DoctorCheck::error(
            "sqlite_quick_check",
            "SQLite quick_check failed",
            e.to_string(),
            "Close Flavite and rebuild the disposable index cache".to_string(),
        ),
    }
}
fn db_count_check(conn: Option<&Connection>, name: &str, sql: &str, label: &str) -> DoctorCheck {
    let Some(conn) = conn else {
        return DoctorCheck::error(
            name,
            label,
            "Index database unavailable".to_string(),
            "Open the vault once or rebuild its cache".to_string(),
        );
    };
    match conn.query_row(sql, [], |r| r.get::<_, i64>(0)) {
        Ok(0) => DoctorCheck::ok(name, format!("No {} found", label.to_lowercase())),
        Ok(n) => DoctorCheck::warn(name, label, format!("{n} found")),
        Err(e) => DoctorCheck::error(
            name,
            label,
            e.to_string(),
            "Rebuild the disposable index cache".to_string(),
        ),
    }
}

fn unresolved_links_check(conn: Option<&Connection>) -> DoctorCheck {
    let Some(conn) = conn else {
        return DoctorCheck::error(
            "broken_links",
            "Unresolved links",
            "Index database unavailable",
            "Open the vault once or rebuild its cache",
        );
    };
    match conn.query_row(
        "SELECT count(*) FROM links WHERE res_state='unresolved'",
        [],
        |row| row.get::<_, i64>(0),
    ) {
        Ok(count) => DoctorCheck::ok("broken_links", "Unresolved links are informational")
            .with_details(format!("{count} unresolved link(s)")),
        Err(error) => DoctorCheck::error(
            "broken_links",
            "Unresolved links",
            error.to_string(),
            "Rebuild the disposable index cache",
        ),
    }
}

fn duplicate_ids_check(conn: Option<&Connection>) -> DoctorCheck {
    let Some(conn) = conn else {
        return DoctorCheck::error(
            "duplicate_ids",
            "Stable ID check",
            "Index database unavailable".to_string(),
            "Open the vault once or rebuild its cache".to_string(),
        );
    };
    let total_notes = conn.query_row("SELECT count(*) FROM notes", [], |r| r.get::<_, i64>(0));
    let ids_present = conn.query_row("SELECT count(stable_id) FROM notes", [], |r| {
        r.get::<_, i64>(0)
    });
    if let (Ok(total), Ok(0)) = (total_notes, ids_present)
        && total > 0
    {
        return DoctorCheck::warn(
            "duplicate_ids",
            "Stable IDs are not yet extracted",
            format!(
                "{total} indexed note(s) have no stable_id value; duplicate detection becomes meaningful when extraction lands"
            ),
        );
    }
    let result = conn.query_row("SELECT count(*) FROM (SELECT stable_id_key FROM notes WHERE stable_id_key IS NOT NULL GROUP BY stable_id_key HAVING count(*) > 1)", [], |r| r.get::<_, i64>(0));
    match result {
        Ok(0) => DoctorCheck::ok("duplicate_ids", "No duplicate stable IDs"),
        Ok(n) => DoctorCheck::error(
            "duplicate_ids",
            "Duplicate stable IDs",
            format!("{n} duplicated IDs"),
            "Remint IDs on the non-winning notes",
        ),
        Err(e) => DoctorCheck::error(
            "duplicate_ids",
            "Stable ID check",
            e.to_string(),
            "Rebuild the disposable index cache".to_string(),
        ),
    }
}

fn sample_hash_check(conn: Option<&Connection>, root: &Path) -> DoctorCheck {
    let Some(conn) = conn else {
        return DoctorCheck::ok(
            "sample_hash_verification",
            "Index unavailable; no sample to verify",
        );
    };
    let mut stmt = match conn.prepare("SELECT path, content_hash FROM notes ORDER BY path LIMIT 10")
    {
        Ok(stmt) => stmt,
        Err(e) => {
            return DoctorCheck::error(
                "sample_hash_verification",
                "Sample hash verification",
                e.to_string(),
                "Rebuild the disposable index cache".to_string(),
            );
        }
    };
    let rows = match stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
    }) {
        Ok(rows) => rows,
        Err(e) => {
            return DoctorCheck::error(
                "sample_hash_verification",
                "Sample hash verification",
                e.to_string(),
                "Rebuild the disposable index cache".to_string(),
            );
        }
    };
    let mut checked = 0;
    let mut bad = Vec::new();
    for row in rows.flatten() {
        let (relative, expected) = row;
        let path = root.join(&relative);
        let Ok(real) = fs::canonicalize(&path) else {
            bad.push(format!("{relative} is missing"));
            continue;
        };
        if !real.starts_with(root) {
            bad.push(format!("{relative} resolves outside the vault"));
            continue;
        }
        match fs::read(real) {
            Ok(bytes) => {
                checked += 1;
                if blake3::hash(&bytes).as_bytes() != expected.as_slice() {
                    bad.push(format!("{relative} hash differs"));
                }
            }
            Err(e) => bad.push(format!("{relative}: {e}")),
        }
    }
    if bad.is_empty() {
        DoctorCheck::ok(
            "sample_hash_verification",
            format!("Verified {checked} indexed note(s)"),
        )
    } else {
        DoctorCheck::warn(
            "sample_hash_verification",
            "Indexed note samples differ from disk",
            bad.join(", "),
        )
    }
}

fn registry_check() -> DoctorCheck {
    let Some(project) = ProjectDirs::from("com", "flavite", "Flavite") else {
        return DoctorCheck::warn(
            "registry_records",
            "Registry unavailable",
            "OS data directory unavailable",
        );
    };
    let dir = project.data_dir().join("flavite/vaults");
    let Ok(entries) = fs::read_dir(&dir) else {
        return DoctorCheck::ok("registry_records", "No vault registry records found");
    };
    let mut stale = Vec::new();
    let mut invalid = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        match fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str::<flavite_vault::RegistryEntry>(&s).ok())
        {
            Some(record) if !Path::new(&record.root_path).exists() => stale.push(record.root_path),
            Some(_) => (),
            None => invalid.push(path.display().to_string()),
        }
    }
    if stale.is_empty() && invalid.is_empty() {
        DoctorCheck::ok(
            "registry_records",
            "Vault registry records point to existing vaults",
        )
    } else {
        stale.extend(invalid.into_iter().map(|p| format!("invalid record {p}")));
        DoctorCheck::warn(
            "registry_records",
            "Registry records without a readable vault",
            stale.join(", "),
        )
    }
}

fn orphan_socket_check() -> DoctorCheck {
    let Some(project) = ProjectDirs::from("com", "flavite", "Flavite") else {
        return DoctorCheck::warn(
            "orphan_instance_sockets",
            "Socket check unavailable",
            "OS data directory unavailable",
        );
    };
    let dir = project.data_dir().join("flavite/vaults");
    let Ok(entries) = fs::read_dir(&dir) else {
        return DoctorCheck::ok("orphan_instance_sockets", "No instance sockets found");
    };
    let orphans: Vec<_> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if !path.is_dir() {
                return None;
            }
            let socket = path.join("instance.ipc");
            (socket.exists() && !path.join("instance.lock").exists())
                .then(|| socket.display().to_string())
        })
        .collect();
    if orphans.is_empty() {
        DoctorCheck::ok(
            "orphan_instance_sockets",
            "No orphan instance sockets found",
        )
    } else {
        DoctorCheck::warn(
            "orphan_instance_sockets",
            "Instance socket without a lock",
            orphans.join(", "),
        )
    }
}
fn temp_check(root: &Path) -> DoctorCheck {
    let fs = OsFs::new();
    let mut found = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        if let Ok(entries) = list_orphan_temps(&fs, &dir) {
            found.extend(entries);
        }
        if let Ok(entries) = flavite_fs::Fs::read_dir(&fs, &dir) {
            for entry in entries {
                if entry.is_dir && !entry.is_symlink {
                    if entry.name != ".obsidian" {
                        dirs.push(entry.path);
                    }
                } else if entry.name.to_string_lossy().contains(".flavite-tmp.") {
                    found.push(entry.path);
                }
            }
        }
    }
    if found.is_empty() {
        DoctorCheck::ok("orphan_temp_files", "No orphan temporary files found")
    } else {
        DoctorCheck::warn(
            "orphan_temp_files",
            "Orphan temporary files",
            found
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}
fn journal_check(root: &Path) -> DoctorCheck {
    let dir = root.join(".flavite/journal");
    let Ok(entries) = fs::read_dir(&dir) else {
        return DoctorCheck::ok("pending_journals", "No pending journals found");
    };
    let paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    if paths.is_empty() {
        DoctorCheck::ok("pending_journals", "No pending journals found")
    } else {
        DoctorCheck::warn(
            "pending_journals",
            "Pending rename journals",
            format!(
                "{} journal file(s): {}",
                paths.len(),
                paths
                    .iter()
                    .map(|p| p.file_name().unwrap_or_default().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    }
}
fn collision_check(root: &Path) -> DoctorCheck {
    let mut nfc: HashMap<String, Vec<String>> = HashMap::new();
    let mut folded: HashMap<String, Vec<String>> = HashMap::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path
                .file_name()
                .is_some_and(|n| n == ".flavite" || n == ".obsidian")
            {
                continue;
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                dirs.push(path.clone());
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            use unicode_normalization::UnicodeNormalization;
            let normalized: String = rel.nfc().collect();
            nfc.entry(normalized.clone()).or_default().push(rel.clone());
            folded
                .entry(flavite_syntax::text::fold_key(&normalized))
                .or_default()
                .push(rel);
        }
    }
    let nfc_groups: Vec<_> = nfc.into_values().filter(|v| v.len() > 1).collect();
    let case_groups: Vec<_> = folded
        .into_values()
        .filter(|v| v.len() > 1 && v.iter().collect::<std::collections::HashSet<_>>().len() > 1)
        .collect();
    let total = nfc_groups.len() + case_groups.len();
    if total == 0 {
        DoctorCheck::ok("nfc_collisions", "No NFC or case-folding path collisions")
    } else {
        let mut details = Vec::new();
        for g in nfc_groups {
            details.push(format!("NFC: {}", g.join(" <> ")));
        }
        for g in case_groups {
            details.push(format!("Case: {}", g.join(" <> ")));
        }
        DoctorCheck::warn(
            "nfc_collisions",
            "Path collisions detected",
            details.join("; "),
        )
    }
}
fn history_check(uuid: uuid::Uuid) -> DoctorCheck {
    let Some(project) = ProjectDirs::from("com", "flavite", "Flavite") else {
        return DoctorCheck::warn(
            "history_size",
            "History size unavailable",
            "OS data directory unavailable",
        );
    };
    let path = project
        .data_dir()
        .join("flavite/vaults")
        .join(uuid.to_string())
        .join("history");
    let bytes = tree_size(&path);
    let snapshots = tree_file_count(&path);
    const LIMIT: u64 = 1024 * 1024 * 1024;
    if bytes <= LIMIT {
        DoctorCheck::ok(
            "history_size",
            format!(
                "History store is {bytes} bytes across {snapshots} snapshot file(s) (1 GiB cap)"
            ),
        )
    } else {
        DoctorCheck::error(
            "history_size",
            "History store exceeds its cap",
            format!("{bytes} bytes; cap is {LIMIT}"),
            "Review history retention and remove old snapshots through the history manager"
                .to_string(),
        )
    }
}

fn tree_file_count(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let child = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                tree_file_count(&child)
            } else {
                1
            }
        })
        .sum()
}

fn runtime_counters_check() -> DoctorCheck {
    let Some(path) = diagnostics_log_dir().map(|dir| dir.join("flavite.log")) else {
        return DoctorCheck::warn(
            "runtime_counters",
            "Save and drift counters",
            "OS data directory unavailable",
        );
    };
    let Ok(contents) = fs::read_to_string(&path) else {
        return DoctorCheck::warn(
            "runtime_counters",
            "Save and drift counters unavailable",
            "No local log file yet",
        );
    };
    let save_total = contents
        .lines()
        .filter(|line| line.contains("save completed"))
        .count();
    let drift_total = contents
        .lines()
        .filter_map(|line| {
            let (_, suffix) = line.split_once("drift_found_total=")?;
            suffix
                .split(|ch: char| !ch.is_ascii_digit())
                .next()?
                .parse::<u64>()
                .ok()
        })
        .max()
        .unwrap_or(0);
    DoctorCheck::ok("runtime_counters", "Local save and reconcile counters").with_details(format!(
        "save.completed_total={save_total}; reconcile.drift_found_total={drift_total}"
    ))
}
fn tree_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            let p = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                tree_size(&p)
            } else {
                e.metadata().map(|m| m.len()).unwrap_or(0)
            }
        })
        .sum()
}

/// Run doctor checks on a vault.
///
/// This is a convenience function that creates a checker and runs all checks.
pub fn run_doctor(vault: ApiVaultRef) -> DoctorReport {
    let checker = DoctorChecker::new(vault);
    checker.run()
}

/// Resolve the vault's persisted/registered identity and hold its root and
/// instance guards while running diagnostics, without opening the writable
/// index actor or reconciling the vault.
pub fn run_doctor_root(root: PathBuf) -> Result<DoctorReport, String> {
    let fs = OsFs::new();
    let root = std::fs::canonicalize(&root).map_err(|e| e.to_string())?;
    if !root.is_dir() {
        return Err(format!("{} is not a vault directory", root.display()));
    }
    let identity = flavite_vault::IdentityManager::open(&fs, root)
        .map_err(|e| format!("Cannot acquire vault identity/instance lock: {e}"))?;
    let vault_ref = ApiVaultRef::new(identity.identity().uuid, identity.identity().root.clone());
    let report = DoctorChecker::new(vault_ref).run();
    drop(identity);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use uuid::Uuid;

    #[test]
    fn doctor_check_creation() {
        let check = DoctorCheck::ok("test", "Test check");
        assert!(check.is_ok());
        assert!(!check.has_issues());

        let check = DoctorCheck::warn("test", "Test check", "warning");
        assert!(!check.is_ok());
        assert!(check.has_issues());

        let check = DoctorCheck::error("test", "Test check", "error", "fix");
        assert!(!check.is_ok());
        assert!(check.has_issues());
    }

    #[test]
    fn doctor_report_basic() {
        let vault_ref = ApiVaultRef::new(Uuid::new_v4(), PathBuf::from("/test"));
        let mut report = DoctorReport::new(vault_ref);

        report.add_check(DoctorCheck::ok("test1", "First check"));
        report.add_check(DoctorCheck::ok("test2", "Second check"));

        assert_eq!(report.checks.len(), 2);
        assert_eq!(report.summary.ok_count, 2);
        assert!(report.is_healthy());
        assert_eq!(report.status(), CheckStatus::Ok);
    }

    #[test]
    fn doctor_report_with_errors() {
        let vault_ref = ApiVaultRef::new(Uuid::new_v4(), PathBuf::from("/test"));
        let mut report = DoctorReport::new(vault_ref);

        report.add_check(DoctorCheck::ok("test1", "First check"));
        report.add_check(DoctorCheck::error(
            "test2",
            "Second check",
            "Failed",
            "Fix it",
        ));

        assert_eq!(report.checks.len(), 2);
        assert_eq!(report.summary.error_count, 1);
        assert!(!report.is_healthy());
        assert_eq!(report.status(), CheckStatus::Error);
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, ApiVaultRef) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".flavite/journal")).unwrap();
        let db = root.path().join("index.sqlite3");
        flavite_index::open_or_init(&db).unwrap();
        let vault = ApiVaultRef::new(Uuid::new_v4(), root.path().to_path_buf());
        (root, db, vault)
    }

    fn check(checker: &DoctorChecker, name: &str) -> DoctorCheck {
        checker
            .run()
            .checks
            .into_iter()
            .find(|c| c.name == name)
            .unwrap()
    }

    #[test]
    fn corruption_is_reported_by_quick_check() {
        let (root, db, vault) = fixture();
        std::fs::write(&db, b"not a sqlite database").unwrap();
        let c = DoctorChecker::with_db_path(vault, db);
        assert!(check(&c, "sqlite_quick_check").has_issues());
        drop(root);
    }

    #[test]
    fn unresolved_and_ambiguous_links_are_counted() {
        let (_root, db, vault) = fixture();
        let conn = Connection::open(&db).unwrap();
        conn.execute("INSERT INTO notes(path,path_native,path_key,stem_key,title,content_hash,size,mtime_ns,scan_ns,generation) VALUES('a.md',X'61','a','a','a',X'00',0,0,0,0)", []).unwrap();
        conn.execute("INSERT INTO links(src,ord,kind,dst_raw,target_key,span_start,span_end,res_state) VALUES(1,0,'link','lost','lost',0,0,'unresolved')", []).unwrap();
        conn.execute("INSERT INTO links(src,ord,kind,dst_raw,target_key,span_start,span_end,res_state) VALUES(1,1,'link','maybe','maybe',0,0,'ambiguous')", []).unwrap();
        drop(conn);
        let c = DoctorChecker::with_db_path(vault, db);
        assert!(check(&c, "broken_links").is_ok());
        assert!(check(&c, "ambiguous_links").has_issues());
    }

    #[test]
    fn changed_disk_note_is_reported_by_sample_hash_check() {
        let (root, db, vault) = fixture();
        std::fs::write(root.path().join("sample.md"), b"current bytes").unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO notes(path,path_native,path_key,stem_key,title,content_hash,size,mtime_ns,scan_ns,generation) VALUES('sample.md',X'73616d706c65','sample.md','sample','sample',?1,13,0,0,0)",
            [blake3::hash(b"indexed bytes").as_bytes()],
        ).unwrap();
        drop(conn);
        let checker = DoctorChecker::with_db_path(vault, db);
        assert!(check(&checker, "sample_hash_verification").has_issues());
    }

    #[test]
    fn duplicate_ids_temps_journals_and_collisions_are_reported() {
        let (root, db, vault) = fixture();
        let conn = Connection::open(&db).unwrap();
        for (id, path) in [(1, "a.md"), (2, "b.md")] {
            conn.execute("INSERT INTO notes(note_id,path,path_native,path_key,stem_key,title,stable_id,stable_id_key,content_hash,size,mtime_ns,scan_ns,generation) VALUES(?1,?2,?2,?2,?2,?2,'same','same',X'00',0,0,0,0)", (id,path)).unwrap();
        }
        drop(conn);
        std::fs::write(root.path().join(".note.md.flavite-tmp.xyz"), b"tmp").unwrap();
        std::fs::write(
            root.path().join(".flavite/journal/txn.json"),
            b"{\"state\":\"applying\"}",
        )
        .unwrap();
        std::fs::write(root.path().join("Readme.md"), b"a").unwrap();
        std::fs::write(root.path().join("README.md"), b"b").unwrap();
        let c = DoctorChecker::with_db_path(vault, db);
        for name in [
            "duplicate_ids",
            "orphan_temp_files",
            "pending_journals",
            "nfc_collisions",
        ] {
            assert!(check(&c, name).has_issues(), "{name}");
        }
    }

    #[test]
    fn healthy_fixture_and_json_shape() {
        let (_root, db, vault) = fixture();
        let report = DoctorChecker::with_db_path(vault, db).run();
        assert!(report.is_healthy());
        let value = serde_json::to_value(report).unwrap();
        for c in value["checks"].as_array().unwrap() {
            for field in ["name", "status", "details", "fix"] {
                assert!(c.get(field).is_some(), "missing {field}");
            }
        }
    }
}
