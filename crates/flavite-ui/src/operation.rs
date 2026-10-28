//! GUI-side tracking for correlated worker operations.

use flavite_api::{Flavite, session::SessionResult};
use std::collections::BTreeMap;

/// A correlated worker operation accepted by the Session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOperation {
    /// Request ID returned by the Session.
    pub request_id: u64,
    /// User-facing operation label.
    pub label: String,
}

/// Terminal outcome collected from the independent operation transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalOperation {
    /// Request ID completed by the Session.
    pub request_id: u64,
    /// Human-readable terminal state.
    pub result: String,
}

/// Lossless pending-operation ledger owned by the UI adapter.
#[derive(Debug, Default)]
pub struct OperationPump {
    pending: BTreeMap<u64, PendingOperation>,
}

impl OperationPump {
    /// Record an accepted asynchronous operation.
    pub fn accept(&mut self, request_id: u64, label: impl Into<String>) {
        self.pending.insert(
            request_id,
            PendingOperation {
                request_id,
                label: label.into(),
            },
        );
    }

    /// Track every terminal completion currently available from the Session.
    pub fn drain(&mut self, flavite: &Flavite) -> Vec<TerminalOperation> {
        flavite
            .session()
            .drain_operation_completions()
            .into_iter()
            .map(|completion| {
                self.pending.remove(&completion.request_id);
                TerminalOperation {
                    request_id: completion.request_id,
                    result: describe_result(&completion.result),
                }
            })
            .collect()
    }

    /// Pending operations in request order.
    pub fn pending(&self) -> impl Iterator<Item = &PendingOperation> {
        self.pending.values()
    }
}

fn describe_result(result: &SessionResult) -> String {
    match result {
        SessionResult::Success(_) => "Completed".into(),
        SessionResult::Pending { request_id } => format!("Still pending ({request_id})"),
        SessionResult::Superseded { by_request, .. } => {
            format!("Superseded by request {by_request}")
        }
        SessionResult::Conflict { path } => format!("Conflict: {}", path.display()),
        SessionResult::ActionRequired { code, data } => {
            format!("Action required: {code} {}", data.as_deref().unwrap_or(""))
        }
        SessionResult::MergeReady { .. } => "Merge requires review".into(),
        SessionResult::Busy => "Busy — retry shortly".into(),
        SessionResult::StaleRevision {
            buffer_id: _,
            expected,
            actual,
        } => {
            format!(
                "Stale revision: expected {}, current {}",
                expected.as_u64(),
                actual.as_u64()
            )
        }
        SessionResult::Error { message } => format!("Failed: {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_ledger_retains_acceptance_until_correlated_completion() {
        let mut pump = OperationPump::default();
        pump.accept(19, "note.save");
        let pending = pump.pending().next().unwrap();
        assert_eq!(pending.request_id, 19);
        assert_eq!(pending.label, "note.save");
    }
}
