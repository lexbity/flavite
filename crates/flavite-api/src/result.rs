//! Execution results (§4.5, D16).

use crate::types::RequestId;
use serde::{Deserialize, Serialize};

/// The result of executing a command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ExecutionResult {
    /// Completed locally with result.
    Local(LocalResult),
    /// Pending completion.
    Pending { request_id: RequestId },
    /// Rejected without effects.
    Rejected(RejectedResult),
}

impl ExecutionResult {
    /// Returns true if this is a success.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Local(LocalResult::Success(_)))
    }

    /// Returns true if pending.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending { .. })
    }

    /// Returns true if rejected.
    #[must_use]
    pub fn is_rejected(&self) -> bool {
        matches!(self, Self::Rejected(_))
    }

    /// Get request ID if pending.
    #[must_use]
    pub fn request_id(&self) -> Option<RequestId> {
        match self {
            Self::Pending { request_id } => Some(*request_id),
            _ => None,
        }
    }
}

/// Local result (synchronous completion).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LocalResult {
    /// Success with data.
    Success(LocalSuccess),
    /// Success with no data.
    Void,
    /// Error.
    Error(LocalError),
}

/// Local success with data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalSuccess {
    /// Request ID.
    pub request_id: RequestId,
    /// Data.
    pub data: serde_json::Value,
}

impl LocalSuccess {
    /// Create new success.
    #[must_use]
    pub fn new(request_id: RequestId, data: serde_json::Value) -> Self {
        Self { request_id, data }
    }

    /// Create void success.
    #[must_use]
    pub fn unit(request_id: RequestId) -> Self {
        Self::new(request_id, serde_json::Value::Null)
    }
}

/// Local error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalError {
    /// Error code.
    pub code: String,
    /// Message.
    pub message: String,
}

impl LocalError {
    /// Create new error.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Rejected result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RejectedResult {
    /// Reason.
    pub reason: RejectionReason,
}

/// Reason for rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RejectionReason {
    /// Unavailable.
    Unavailable,
    /// Read-only.
    ReadOnly,
    /// Buffer not found.
    BufferNotFound,
    /// Stale revision.
    StaleRevision,
    /// Busy.
    Busy,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_result_methods() {
        let success = ExecutionResult::Local(LocalResult::Success(LocalSuccess::unit(1)));
        assert!(success.is_success());
        assert!(!success.is_pending());
        assert!(!success.is_rejected());

        let pending = ExecutionResult::Pending { request_id: 1 };
        assert!(!pending.is_success());
        assert!(pending.is_pending());
        assert_eq!(pending.request_id(), Some(1));

        let rejected = ExecutionResult::Rejected(RejectedResult {
            reason: RejectionReason::Unavailable,
        });
        assert!(!rejected.is_success());
        assert!(!rejected.is_pending());
        assert!(rejected.is_rejected());
    }
}
