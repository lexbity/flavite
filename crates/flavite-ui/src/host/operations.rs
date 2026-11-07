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
pub struct OperationsHost {}

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
