//! Coverage floors and ratchet (§10.5).
//!
//! `cov-check` reads an `lcov.info` + the floors config (`coverage.toml`),
//! fails if a gating crate is below its floor, and supports `--ratchet`
//! to fail on any decrease vs `main`'s lcov artifact.

use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use std::collections::HashMap;
use std::path::Path;

use crate::arch::load_coverage_toml;

/// Per-crate coverage summary parsed from an lcov file.
#[derive(Debug, Clone)]
pub struct CrateCoverage {
    pub lines_found: u64,
    pub lines_hit: u64,
    pub pct: f64,
}

impl CrateCoverage {
    pub fn pct(&self) -> f64 {
        if self.lines_found == 0 {
            // Zero executable lines → N/A (not a failure for gating).
            return 100.0;
        }
        self.lines_hit as f64 / self.lines_found as f64 * 100.0
    }
}

/// Parse an lcov file into per-crate coverage summaries.
///
/// Maps source files to crates by matching the path against
/// `crates/<crate-name>/src/`.
pub fn parse_lcov(lcov_content: &str) -> HashMap<String, CrateCoverage> {
    let mut crates: HashMap<String, CrateCoverage> = HashMap::new();

    let mut current_file: Option<String> = None;
    let mut lines_found: u64 = 0;
    let mut lines_hit: u64 = 0;

    for line in lcov_content.lines() {
        if let Some(path) = line.strip_prefix("SF:") {
            // Flush previous file.
            if let Some(file) = current_file.take()
                && let Some(crate_name) = file_to_crate(&file)
            {
                let entry = crates.entry(crate_name).or_insert(CrateCoverage {
                    lines_found: 0,
                    lines_hit: 0,
                    pct: 0.0,
                });
                entry.lines_found += lines_found;
                entry.lines_hit += lines_hit;
            }
            current_file = Some(path.to_string());
            lines_found = 0;
            lines_hit = 0;
        } else if let Some(count) = line.strip_prefix("DA:") {
            // DA:<line>,<count>
            if let Some(comma) = count.find(',') {
                let hit_count: u64 = count[comma + 1..].parse().unwrap_or(0);
                lines_found += 1;
                if hit_count > 0 {
                    lines_hit += 1;
                }
            }
        } else if line == "end_of_record" {
            if let Some(file) = current_file.take()
                && let Some(crate_name) = file_to_crate(&file)
            {
                let entry = crates.entry(crate_name).or_insert(CrateCoverage {
                    lines_found: 0,
                    lines_hit: 0,
                    pct: 0.0,
                });
                entry.lines_found += lines_found;
                entry.lines_hit += lines_hit;
            }
            lines_found = 0;
            lines_hit = 0;
        }
    }

    // Flush last file if no end_of_record.
    if let Some(file) = current_file
        && let Some(crate_name) = file_to_crate(&file)
    {
        let entry = crates.entry(crate_name).or_insert(CrateCoverage {
            lines_found: 0,
            lines_hit: 0,
            pct: 0.0,
        });
        entry.lines_found += lines_found;
        entry.lines_hit += lines_hit;
    }

    // Compute percentages.
    for cov in crates.values_mut() {
        cov.pct = cov.pct();
    }

    crates
}

/// Map a source file path to a crate name.
///
/// Matches paths containing `crates/<crate-name>/src/`.
fn file_to_crate(path: &str) -> Option<String> {
    // Normalize path separators.
    let normalized = path.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').collect();
    // Find "crates/<name>/src" pattern.
    for (i, part) in parts.iter().enumerate() {
        if *part == "crates" && i + 2 < parts.len() && parts[i + 2] == "src" {
            return Some(parts[i + 1].to_string());
        }
    }
    None
}

/// The result of a coverage check.
#[derive(Debug)]
pub struct CovReport {
    pub failures: Vec<String>,
    pub warnings: Vec<String>,
}

/// Run the coverage check.
///
/// - `lcov_path`: path to the lcov.info file.
/// - `workspace_root`: workspace root (for loading coverage.toml).
/// - `ratchet_path`: optional path to main's lcov for ratchet comparison.
pub fn check(
    lcov_path: &Path,
    workspace_root: &Path,
    ratchet_path: Option<&Path>,
) -> Result<CovReport> {
    let content = std::fs::read_to_string(lcov_path)
        .with_context(|| format!("failed to read lcov: {}", lcov_path.display()))?;
    let coverages = parse_lcov(&content);

    let config = load_coverage_toml(workspace_root)?;

    let mut report = CovReport {
        failures: Vec::new(),
        warnings: Vec::new(),
    };

    // Check floors for each gating crate.
    for (crate_name, tier) in &config.crates {
        let permanent_floor = match tier.as_str() {
            "tier1" => config.floors.tier1,
            "tier2" => config.floors.tier2,
            "tier3" => config.floors.tier3,
            _ => continue,
        };

        // Tier 3 is measured, not gating.
        if tier == "tier3" {
            if let Some(cov) = coverages.get(crate_name) {
                println!("  {crate_name}: {:.1}% (tier 3, reported only)", cov.pct());
            }
            continue;
        }

        let floor = if let Some(waiver) = config.waivers.get(crate_name) {
            let expiry = NaiveDate::parse_from_str(&waiver.expires, "%Y-%m-%d");
            let valid = match expiry {
                Ok(date)
                    if date >= Utc::now().date_naive()
                        && !waiver.reason.trim().is_empty()
                        && waiver.floor >= 0.0
                        && waiver.floor < permanent_floor =>
                {
                    Some(waiver.floor)
                }
                Ok(date) if date < Utc::now().date_naive() => {
                    report
                        .failures
                        .push(format!("{crate_name}: coverage waiver expired on {date}"));
                    None
                }
                Ok(_) => {
                    report.failures.push(format!("{crate_name}: invalid waiver; floor must be lower than {permanent_floor:.1}% and reason must be stated"));
                    None
                }
                Err(_) => {
                    report.failures.push(format!(
                        "{crate_name}: invalid waiver expiry {}; expected YYYY-MM-DD",
                        waiver.expires
                    ));
                    None
                }
            };
            if let Some(floor) = valid {
                println!(
                    "  {crate_name}: temporary floor {floor:.1}% (permanent {permanent_floor:.0}%, expires {})",
                    waiver.expires
                );
                floor
            } else {
                permanent_floor
            }
        } else {
            permanent_floor
        };

        match coverages.get(crate_name) {
            Some(cov) => {
                let pct = cov.pct();
                let status = if pct >= floor { "OK" } else { "FAIL" };
                println!(
                    "  {crate_name}: {:.1}% (floor: {:.0}%, {status})",
                    pct, floor
                );
                if pct < floor {
                    report
                        .failures
                        .push(format!("{crate_name}: {pct:.1}% < floor {floor:.0}%"));
                }
            }
            None => {
                // Crate has no coverage data — if it's an empty crate, that's N/A.
                // If it should have code, that's a failure.
                report
                    .warnings
                    .push(format!("{crate_name}: no coverage data (empty crate?)"));
                println!("  {crate_name}: N/A (no coverage data)");
            }
        }
    }

    // Ratchet: compare against main's lcov.
    if let Some(ratchet_path) = ratchet_path {
        let ratchet_content = std::fs::read_to_string(ratchet_path)
            .with_context(|| format!("failed to read ratchet lcov: {}", ratchet_path.display()))?;
        let ratchet_coverages = parse_lcov(&ratchet_content);

        for (crate_name, tier) in &config.crates {
            if tier == "tier3" {
                continue;
            }
            if let (Some(current), Some(previous)) =
                (coverages.get(crate_name), ratchet_coverages.get(crate_name))
            {
                let current_pct = current.pct();
                let previous_pct = previous.pct();
                if current_pct < previous_pct {
                    report.failures.push(format!(
                        "{crate_name}: coverage decreased from {previous_pct:.1}% to {current_pct:.1}% (ratchet)"
                    ));
                }
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_LCOV: &str = r#"SF:/home/user/project/crates/flavite-fs/src/lib.rs
DA:1,1
DA:2,1
DA:3,0
DA:4,5
end_of_record
SF:/home/user/project/crates/flavite-fs/src/mem.rs
DA:1,1
DA:2,0
end_of_record
SF:/home/user/project/crates/flavite-syntax/src/lib.rs
DA:1,1
end_of_record
SF:/home/user/project/xtask/src/main.rs
DA:1,1
end_of_record
"#;

    #[test]
    fn parse_lcov_aggregates_per_crate() {
        let coverages = parse_lcov(SAMPLE_LCOV);
        let fs_cov = coverages.get("flavite-fs").unwrap();
        // 6 lines total (4 + 2), 4 hit (3 + 1)
        assert_eq!(fs_cov.lines_found, 6);
        assert_eq!(fs_cov.lines_hit, 4);
        assert!((fs_cov.pct() - 66.67).abs() < 0.1);

        let syntax_cov = coverages.get("flavite-syntax").unwrap();
        assert_eq!(syntax_cov.lines_found, 1);
        assert_eq!(syntax_cov.lines_hit, 1);
        assert!((syntax_cov.pct() - 100.0).abs() < 0.1);
    }

    #[test]
    fn file_to_crate_extracts_name() {
        assert_eq!(
            file_to_crate("/home/user/project/crates/flavite-fs/src/lib.rs"),
            Some("flavite-fs".to_string())
        );
        assert_eq!(
            file_to_crate("C:\\project\\crates\\flavite-syntax\\src\\lib.rs"),
            Some("flavite-syntax".to_string())
        );
        // xtask is at the workspace root (not under crates/), so it's not
        // mapped — it's tooling, not a gating crate.
        assert_eq!(file_to_crate("/home/user/project/xtask/src/main.rs"), None);
        assert_eq!(file_to_crate("/no/crates/here.rs"), None);
    }

    #[test]
    fn empty_crate_reports_100() {
        let cov = CrateCoverage {
            lines_found: 0,
            lines_hit: 0,
            pct: 0.0,
        };
        assert_eq!(cov.pct(), 100.0);
    }

    #[test]
    fn lcov_with_windows_paths() {
        let lcov = "SF:C:\\project\\crates\\flavite-fs\\src\\lib.rs\nDA:1,1\nend_of_record\n";
        let coverages = parse_lcov(lcov);
        assert!(coverages.contains_key("flavite-fs"));
    }

    #[test]
    fn coverage_below_floor_fails() {
        // Create a temp workspace with coverage.toml and lcov.
        let tmp = std::env::temp_dir().join(format!("flavite-cov-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("devdocs/architecture")).unwrap();
        std::fs::write(
            tmp.join("devdocs/architecture/coverage.toml"),
            r#"
[floors]
tier1 = 90.0
tier2 = 80.0

[crates]
flavite-fs = "tier1"
"#,
        )
        .unwrap();

        // 50% coverage — below the 90% floor.
        let lcov_path = tmp.join("lcov.info");
        std::fs::write(
            &lcov_path,
            "SF:/project/crates/flavite-fs/src/lib.rs\nDA:1,1\nDA:2,0\nend_of_record\n",
        )
        .unwrap();

        let report = check(&lcov_path, &tmp, None).unwrap();
        assert!(!report.failures.is_empty());
        assert!(report.failures[0].contains("flavite-fs"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn coverage_above_floor_passes() {
        let tmp = std::env::temp_dir().join(format!("flavite-cov-test2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("devdocs/architecture")).unwrap();
        std::fs::write(
            tmp.join("devdocs/architecture/coverage.toml"),
            r#"
[floors]
tier1 = 90.0
tier2 = 80.0

[crates]
flavite-fs = "tier1"
"#,
        )
        .unwrap();

        // 100% coverage — above the 90% floor.
        let lcov_path = tmp.join("lcov.info");
        std::fs::write(
            &lcov_path,
            "SF:/project/crates/flavite-fs/src/lib.rs\nDA:1,1\nDA:2,1\nend_of_record\n",
        )
        .unwrap();

        let report = check(&lcov_path, &tmp, None).unwrap();
        assert!(report.failures.is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn active_waiver_applies_and_expired_waiver_fails() {
        let tmp = std::env::temp_dir().join(format!("flavite-cov-waiver-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("devdocs/architecture")).unwrap();
        let config = tmp.join("devdocs/architecture/coverage.toml");
        let lcov = tmp.join("lcov.info");
        std::fs::write(
            &lcov,
            "SF:/project/crates/flavite-fs/src/lib.rs\nDA:1,1\nDA:2,0\nend_of_record\n",
        )
        .unwrap();
        std::fs::write(&config, "[floors]\ntier1=90.0\n[crates]\nflavite-fs=\"tier1\"\n[waivers.flavite-fs]\nfloor=50.0\nexpires=\"2999-01-01\"\nreason=\"add tests\"\n").unwrap();
        assert!(check(&lcov, &tmp, None).unwrap().failures.is_empty());
        std::fs::write(&config, "[floors]\ntier1=90.0\n[crates]\nflavite-fs=\"tier1\"\n[waivers.flavite-fs]\nfloor=50.0\nexpires=\"2000-01-01\"\nreason=\"add tests\"\n").unwrap();
        assert!(
            check(&lcov, &tmp, None)
                .unwrap()
                .failures
                .iter()
                .any(|failure| failure.contains("waiver expired"))
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
