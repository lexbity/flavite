//! GUI-side tracking for correlated worker operations.

use flavite_api::{ExecutionResult, Flavite, LocalResult, OperationCompletion};
use std::collections::{BTreeMap, VecDeque};

const MAX_TERMINAL_COMPLETIONS: usize = 4096;

/// A correlated worker operation accepted by the Session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOperation {
    /// Request ID returned by the Session.
    pub request_id: u64,
    /// User-facing operation label.
    pub label: String,
}

/// Terminal outcome retained in typed form until the presentation boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalOperation {
    /// Structured Session outcome.
    pub completion: OperationCompletion,
}

/// Lossless pending-operation ledger owned by the UI adapter.
#[derive(Debug, Default)]
pub struct OperationPump {
    pending: BTreeMap<u64, PendingOperation>,
    terminal: VecDeque<TerminalOperation>,
    terminal_overflowed: u64,
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
            self.terminal.push_back(TerminalOperation {
                completion: completion.clone(),
            });
            if self.terminal.len() > MAX_TERMINAL_COMPLETIONS {
                self.terminal.pop_front();
                self.terminal_overflowed = self.terminal_overflowed.saturating_add(1);
            }
        }
    }

    /// Drain the Session once and ingest terminal outcomes.
    pub fn drain(&mut self, flavite: &Flavite) {
        let completions = flavite.drain_operation_completions();
        self.ingest(&completions);
    }

    /// Pending operations in request order.
    pub fn pending(&self) -> impl Iterator<Item = &PendingOperation> {
        self.pending.values()
    }

    /// Inspect retained typed outcomes in completion order.
    pub fn terminal(&self) -> impl Iterator<Item = &TerminalOperation> {
        self.terminal.iter()
    }

    /// Number of oldest terminal records displaced by the bounded host
    /// history. This remains visible to diagnostics instead of being silent.
    #[must_use]
    pub const fn terminal_overflowed(&self) -> u64 {
        self.terminal_overflowed
    }

    /// Acknowledge a terminal outcome after its presentation surface has
    /// rendered it. Unpresented outcomes remain available to the host.
    pub fn acknowledge_terminal(&mut self, request_id: u64) -> bool {
        let Some(index) = self
            .terminal
            .iter()
            .position(|terminal| terminal.completion.request_id == request_id)
        else {
            return false;
        };
        self.terminal.remove(index);
        true
    }
}

/// Present one typed completion at the presentation boundary.
///
/// Classification reads the structured result only (TD-09): no string
/// matching, and the typed outcome stays in the ledger regardless of what
/// is displayed here.
#[must_use]
pub fn describe_completion(completion: &OperationCompletion) -> String {
    match &completion.result {
        ExecutionResult::Local(LocalResult::Success(success)) => format!(
            "Operation {} completed{}",
            completion.request_id,
            success
                .data
                .get("revision")
                .and_then(serde_json::Value::as_u64)
                .map_or_else(String::new, |revision| format!(" at revision {revision}"))
        ),
        ExecutionResult::Local(LocalResult::Error(error)) => {
            format!(
                "Operation {} failed: {}",
                completion.request_id, error.message
            )
        }
        ExecutionResult::Rejected(rejected) => format!(
            "Operation {} rejected: {}",
            completion.request_id,
            describe_rejection(&rejected.reason)
        ),
        ExecutionResult::Pending { .. } => {
            format!("Operation {} is still pending", completion.request_id)
        }
        ExecutionResult::ActionRequired { code, .. } => {
            format!(
                "Operation {} needs attention: {code}",
                completion.request_id
            )
        }
        ExecutionResult::Superseded { by_request, .. } => format!(
            "Operation {} was superseded by request {by_request}",
            completion.request_id
        ),
        ExecutionResult::Local(LocalResult::Void) => {
            format!("Operation {} completed", completion.request_id)
        }
    }
}

/// Human presentation for typed rejection reasons.
#[must_use]
pub fn describe_rejection(reason: &flavite_api::RejectionReason) -> &'static str {
    use flavite_api::RejectionReason::*;
    match reason {
        Unavailable => "the core service is unavailable",
        ReadOnly => "the vault is open read-only",
        BufferNotFound => "the buffer no longer exists",
        StaleRevision => "the buffer changed since the request",
        Busy => "the core is busy; retry",
        Unsupported => "the command is unsupported",
        InvalidArgs => "the command arguments were invalid",
        CapabilityDenied => "the caller lacks the required capability",
        Conflict => "the note is in conflict and needs a decision",
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

    #[test]
    fn terminal_outcome_remains_typed_until_explicit_acknowledgement() {
        let mut pump = OperationPump::default();
        let completion = flavite_api::OperationCompletion {
            request_id: 27,
            result: flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Void),
        };
        pump.accept(27, "note.save");
        pump.ingest(std::slice::from_ref(&completion));
        assert_eq!(pump.pending().count(), 0);
        assert_eq!(pump.terminal().next().unwrap().completion, completion);
        assert!(pump.acknowledge_terminal(27));
        assert_eq!(pump.terminal().count(), 0);
    }

    #[test]
    fn terminal_retention_is_bounded_and_overflow_is_counted() {
        let mut pump = OperationPump::default();
        let completions = (0..=MAX_TERMINAL_COMPLETIONS)
            .map(|request_id| flavite_api::OperationCompletion {
                request_id: request_id as u64,
                result: flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Void),
            })
            .collect::<Vec<_>>();
        pump.ingest(&completions);
        assert_eq!(pump.terminal().count(), MAX_TERMINAL_COMPLETIONS);
        assert_eq!(pump.terminal_overflowed(), 1);
    }
}
