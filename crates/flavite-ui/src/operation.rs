//! GUI-side tracking for correlated worker operations.

use flavite_api::{ExecutionResult, Flavite, LocalResult};
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

    /// Ingest one drained batch of completions, retiring matching pending
    /// entries. The caller drains the Session once and routes completions
    /// to every interested surface.
    pub fn ingest(&mut self, completions: &[flavite_api::OperationCompletion]) {
        for completion in completions {
            self.pending.remove(&completion.request_id);
        }
    }

    /// Drain the Session once and ingest, returning the terminal operations
    /// for presentation.
    pub fn drain(&mut self, flavite: &Flavite) -> Vec<TerminalOperation> {
        let completions = flavite.drain_operation_completions();
        self.ingest(&completions);
        completions
            .into_iter()
            .map(|completion| TerminalOperation {
                request_id: completion.request_id,
                result: describe_result(&completion.result),
            })
            .collect()
    }

    /// Pending operations in request order.
    pub fn pending(&self) -> impl Iterator<Item = &PendingOperation> {
        self.pending.values()
    }
}

fn describe_result(result: &ExecutionResult) -> String {
    use flavite_api::RejectionReason;
    match result {
        ExecutionResult::Local(local) => match local {
            LocalResult::Success(_) => "Completed".into(),
            LocalResult::Void => "Completed".into(),
            LocalResult::Error(error) => format!("Failed: {}", error.message),
        },
        ExecutionResult::Pending { request_id } => format!("Still pending ({request_id})"),
        ExecutionResult::Superseded { by_request, .. } => {
            format!("Superseded by request {by_request}")
        }
        ExecutionResult::ActionRequired { code, .. } => format!("Action required: {code}"),
        ExecutionResult::Rejected(rejected) => match rejected.reason {
            RejectionReason::Busy => "Busy — retry shortly".into(),
            other => format!("Rejected: {other:?}"),
        },
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
