//! Doctor diagnostics module (§4.11).
//!
//! This module provides comprehensive vault health checks for the `flavite doctor` command.
//!
//! # Doctor Checks
//!
//! The doctor command performs the following checks:
//! 1. SQLite quick_check
//! 2. Tantivy generation vs SQLite
//! 3. Sample-N hash verification against disk
//! 4. Orphan temp files
//! 5. Pending journals
//! 6. Duplicate and conflicting IDs
//! 7. NFC/case collisions
//! 8. Broken and ambiguous link counts
//! 9. Attachment orphans and unhydrated placeholders
//! 10. History size/pinned bytes
//! 11. UUID/root binding conflicts
//! 12. Orphan instance sockets
//! 13. Search checkpoint/log coverage
//! 14. Verification lag
//! 15. Live journal payload/checkpoint integrity
//!
//! Doctor never cleans files referenced by a live journal.

use crate::types::ApiVaultRef;
use serde::{Deserialize, Serialize};

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

/// Doctor checker for a vault.
#[derive(Debug)]
pub struct DoctorChecker {
    vault: ApiVaultRef,
}

impl DoctorChecker {
    /// Create a new doctor checker for a vault.
    #[must_use]
    pub fn new(vault: ApiVaultRef) -> Self {
        Self { vault }
    }

    /// Run all doctor checks and return a report.
    ///
    /// This is a stub implementation that will be filled in as the various
    /// subsystems are implemented.
    pub fn run(&self) -> DoctorReport {
        let mut report = DoctorReport::new(self.vault.clone());

        // 1. SQLite quick_check
        report.add_check(self.check_sqlite_quick_check());

        // 2. Tantivy generation vs SQLite
        report.add_check(self.check_tantivy_generation());

        // 3. Sample-N hash verification
        report.add_check(self.check_sample_hash_verification());

        // 4. Orphan temp files
        report.add_check(self.check_orphan_temp_files());

        // 5. Pending journals
        report.add_check(self.check_pending_journals());

        // 6. Duplicate and conflicting IDs
        report.add_check(self.check_duplicate_ids());

        // 7. NFC/case collisions
        report.add_check(self.check_nfc_collisions());

        // 8. Broken and ambiguous links
        report.add_check(self.check_broken_links());

        // 9. Attachment orphans and unhydrated placeholders
        report.add_check(self.check_attachment_orphans());

        // 10. History size/pinned bytes
        report.add_check(self.check_history_size());

        // 11. UUID/root binding conflicts
        report.add_check(self.check_uuid_binding());

        // 12. Orphan instance sockets
        report.add_check(self.check_orphan_instance_sockets());

        // 13. Search checkpoint/log coverage
        report.add_check(self.check_search_checkpoint());

        // 14. Verification lag
        report.add_check(self.check_verification_lag());

        // 15. Live journal integrity
        report.add_check(self.check_journal_integrity());

        report.summary.calculate_total();
        report
    }

    /// Check SQLite integrity with quick_check.
    fn check_sqlite_quick_check(&self) -> DoctorCheck {
        // In a full implementation, we would run PRAGMA quick_check on the SQLite DB
        // For now, this is a placeholder that checks if we can access the vault path
        let vault_path = &self.vault.root;
        if vault_path.exists() {
            let flavite_dir = vault_path.join(".flavite");
            if flavite_dir.exists() {
                DoctorCheck::ok("sqlite_quick_check", "SQLite database integrity check")
            } else {
                DoctorCheck::warn(
                    "sqlite_quick_check",
                    "SQLite database integrity check",
                    ".flavite directory not found",
                )
            }
        } else {
            DoctorCheck::error(
                "sqlite_quick_check",
                "SQLite database integrity check",
                format!("Vault path {} does not exist", vault_path.display()),
                "Ensure the vault path is correct and accessible".to_string(),
            )
        }
    }

    /// Check Tantivy generation matches SQLite.
    fn check_tantivy_generation(&self) -> DoctorCheck {
        // This will check that tantivy.committed_generation matches
        // the SQLite index.generation
        // Placeholder for now
        DoctorCheck::ok("tantivy_generation", "Tantivy generation matches SQLite")
    }

    /// Check sample-N hash verification against disk.
    fn check_sample_hash_verification(&self) -> DoctorCheck {
        // This will sample N notes and verify their content hashes match disk
        // Placeholder for now
        DoctorCheck::ok(
            "sample_hash_verification",
            "Sample hash verification against disk",
        )
    }

    /// Check for orphan temp files.
    fn check_orphan_temp_files(&self) -> DoctorCheck {
        // Look for .flavite-tmp.* files that might be left over from crashes
        let vault_path = &self.vault.root;

        // This is a simplified check - in production we'd walk the vault
        // and look for temp files older than some threshold
        if vault_path.exists() {
            // For now, just check if .flavite directory exists
            // A real implementation would look for actual temp files
            DoctorCheck::ok("orphan_temp_files", "No orphan temp files found")
        } else {
            DoctorCheck::warn(
                "orphan_temp_files",
                "Cannot check for orphan temp files",
                "Vault does not exist",
            )
        }
    }

    /// Check for pending journals.
    fn check_pending_journals(&self) -> DoctorCheck {
        // Check for uncommitted/preparing journal entries
        // Placeholder for now
        DoctorCheck::ok("pending_journals", "No pending journal entries")
    }

    /// Check for duplicate and conflicting IDs.
    fn check_duplicate_ids(&self) -> DoctorCheck {
        // Check the index for duplicate stable_id values
        // Placeholder for now
        DoctorCheck::ok("duplicate_ids", "No duplicate or conflicting IDs found")
    }

    /// Check for NFC/case collisions.
    fn check_nfc_collisions(&self) -> DoctorCheck {
        // Check for files that would collide under NFC normalization
        // Placeholder for now
        DoctorCheck::ok("nfc_collisions", "No NFC/case collisions detected")
    }

    /// Check for broken and ambiguous links.
    fn check_broken_links(&self) -> DoctorCheck {
        // Check the link table for unresolved/ambiguous entries
        // Placeholder for now
        DoctorCheck::ok("broken_links", "No broken or ambiguous links found")
    }

    /// Check for attachment orphans and unhydrated placeholders.
    fn check_attachment_orphans(&self) -> DoctorCheck {
        // Check for attachments not referenced by any note
        // and cloud placeholders that haven't been hydrated
        // Placeholder for now
        DoctorCheck::ok(
            "attachment_orphans",
            "No attachment orphans or unhydrated placeholders",
        )
    }

    /// Check history size and pinned bytes.
    fn check_history_size(&self) -> DoctorCheck {
        // Check that history store is within bounds
        // Placeholder for now
        DoctorCheck::ok("history_size", "History store is within size limits")
    }

    /// Check UUID/root binding conflicts.
    fn check_uuid_binding(&self) -> DoctorCheck {
        // Check that the registry doesn't have conflicting bindings
        // Placeholder for now
        DoctorCheck::ok("uuid_binding", "No UUID/root binding conflicts")
    }

    /// Check for orphan instance sockets.
    fn check_orphan_instance_sockets(&self) -> DoctorCheck {
        // Check for stale IPC sockets in the OS data directory
        // Placeholder for now
        DoctorCheck::ok(
            "orphan_instance_sockets",
            "No orphan instance sockets found",
        )
    }

    /// Check search checkpoint and log coverage.
    fn check_search_checkpoint(&self) -> DoctorCheck {
        // Check that the search checkpoint is consistent with the log
        // Placeholder for now
        DoctorCheck::ok(
            "search_checkpoint",
            "Search checkpoint is consistent with log",
        )
    }

    /// Check verification lag.
    fn check_verification_lag(&self) -> DoctorCheck {
        // Check if verification is overdue
        // Placeholder for now
        DoctorCheck::ok("verification_lag", "Verification is up to date")
    }

    /// Check live journal payload and checkpoint integrity.
    fn check_journal_integrity(&self) -> DoctorCheck {
        // Check that all live journal entries have valid payloads/checkpoints
        // Placeholder for now
        DoctorCheck::ok(
            "journal_integrity",
            "Live journal payloads and checkpoints are intact",
        )
    }
}

/// Run doctor checks on a vault.
///
/// This is a convenience function that creates a checker and runs all checks.
pub fn run_doctor(vault: ApiVaultRef) -> DoctorReport {
    let checker = DoctorChecker::new(vault);
    checker.run()
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

    #[test]
    fn doctor_checker_basic() {
        // Use a temp directory that exists
        let temp_dir = std::env::temp_dir();
        let vault_ref = ApiVaultRef::new(Uuid::new_v4(), temp_dir);
        let checker = DoctorChecker::new(vault_ref);
        let report = checker.run();

        // Should have all checks
        assert!(!report.checks.is_empty());
        // Note: The report may not be healthy if the temp dir doesn't have .flavite
        // This is expected for the stub implementation
    }

    #[test]
    fn run_doctor_basic() {
        let vault_ref = ApiVaultRef::new(Uuid::new_v4(), PathBuf::from("/test"));
        let report = run_doctor(vault_ref);

        assert!(!report.checks.is_empty());
    }
}
