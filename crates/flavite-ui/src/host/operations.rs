//! Durable replacement operations host (RFC-0006 S08/§6.4): a thin,
//! read-only aggregator over the per-tab Replace hosts' job states. The
//! host owns no files and never blocks; the view renders the aggregated
//! job inventory (phase + totals), and per-job controls live on the Replace
//! tab where the sealed preview is owned.

use std::collections::BTreeMap;

use flavite_workspace::TabId;

use super::replace::{JobView, ReplaceHost};

/// One aggregated job row (identity + phase + totals; never note text).
#[derive(Debug, Clone, PartialEq)]
pub struct OperationRow {
    /// The Replace tab that owns the job.
    pub tab: TabId,
    pub job: JobView,
}

/// Operations host: aggregates live replacement jobs across Replace tabs.
#[derive(Debug, Default)]
pub struct OperationsHost {
    /// Debounced refresh counter (job status refreshes at most every 250 ms
    /// while active, NFR-17).
    last_refresh: Option<std::time::Instant>,
}

impl OperationsHost {
    /// Aggregate every live job.
    #[must_use]
    pub fn rows(&self, replace: &ReplaceHost) -> Vec<OperationRow> {
        replace
            .tabs
            .iter()
            .filter_map(|(tab, state)| {
                state.job.as_ref().map(|job| OperationRow {
                    tab: *tab,
                    job: job.clone(),
                })
            })
            .collect()
    }

    /// Refresh gating (NFR-17): at most every 250 ms while a job is active.
    #[must_use]
    pub fn refresh_due(&mut self, replace: &ReplaceHost) -> bool {
        let active = replace.tabs.values().any(|state| {
            state
                .job
                .as_ref()
                .is_some_and(|job| matches!(job.phase.as_str(), "applying" | "partial"))
        });
        if !active {
            self.last_refresh = None;
            return false;
        }
        let due = self
            .last_refresh
            .is_none_or(|last| last.elapsed() >= std::time::Duration::from_millis(250));
        if due {
            self.last_refresh = Some(std::time::Instant::now());
        }
        due
    }

    /// Aggregate counts for diagnostics (bounded labels).
    #[must_use]
    pub fn summary(&self, replace: &ReplaceHost) -> BTreeMap<&'static str, u64> {
        let mut summary = BTreeMap::new();
        for row in self.rows(replace) {
            *summary
                .entry(match row.job.phase.as_str() {
                    "completed" => "completed",
                    "partial" => "partial",
                    "cancelled" => "cancelled",
                    "paused" => "paused",
                    "needs-recovery" => "needs_recovery",
                    _ => "applying",
                })
                .or_insert(0) += 1;
        }
        summary
    }
}
