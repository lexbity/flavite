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
    /// Accepted command needs an explicit user decision before it can proceed.
    ActionRequired {
        /// The invocation that needs a decision.
        request_id: RequestId,
        /// Stable action code for a host to present.
        code: String,
        /// Structured details for the decision UI.
        data: Option<serde_json::Value>,
    },
    /// An admitted operation was replaced by a newer coalesced request.
    Superseded {
        request_id: RequestId,
        by_request: RequestId,
    },
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
            Self::ActionRequired { request_id, .. } => Some(*request_id),
            Self::Superseded { request_id, .. } => Some(*request_id),
            _ => None,
        }
    }

    /// Get request ID from any variant.
    #[must_use]
    pub fn any_request_id(&self) -> Option<RequestId> {
        match self {
            Self::Local(LocalResult::Success(s)) => Some(s.request_id),
            Self::Local(LocalResult::Void) => None,
            Self::Local(LocalResult::Error(e)) => e.request_id,
            Self::Pending { request_id }
            | Self::ActionRequired { request_id, .. }
            | Self::Superseded { request_id, .. } => Some(*request_id),
            Self::Rejected(_) => None,
        }
    }

    /// Extract the success data if this is a success.
    #[must_use]
    pub fn into_success(self) -> Option<LocalSuccess> {
        match self {
            Self::Local(LocalResult::Success(s)) => Some(s),
            _ => None,
        }
    }

    /// Extract the error if this is an error.
    #[must_use]
    pub fn into_error(self) -> Option<LocalError> {
        match self {
            Self::Local(LocalResult::Error(e)) => Some(e),
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

impl LocalResult {
    /// Returns true if this is a success.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success(_))
    }

    /// Returns true if this is an error.
    #[must_use]
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }
}

/// Local success with data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalSuccess {
    /// Request ID.
    pub request_id: RequestId,
    /// Command ID.
    pub command_id: String,
    /// Data.
    pub data: serde_json::Value,
    /// Epoch from the vault.
    pub epoch: Option<u64>,
    /// Generation from the vault.
    pub generation: Option<u64>,
}

impl LocalSuccess {
    /// Create new success.
    #[must_use]
    pub fn new(request_id: RequestId, data: serde_json::Value) -> Self {
        Self {
            request_id,
            command_id: String::new(),
            data,
            epoch: None,
            generation: None,
        }
    }

    /// Create new success with command ID.
    #[must_use]
    pub fn with_command(
        request_id: RequestId,
        command_id: impl Into<String>,
        data: serde_json::Value,
    ) -> Self {
        Self {
            request_id,
            command_id: command_id.into(),
            data,
            epoch: None,
            generation: None,
        }
    }

    /// Create void success.
    #[must_use]
    pub fn unit(request_id: RequestId) -> Self {
        Self::new(request_id, serde_json::Value::Null)
    }

    /// Set epoch and generation.
    #[must_use]
    pub fn with_version(mut self, epoch: u64, generation: u64) -> Self {
        self.epoch = Some(epoch);
        self.generation = Some(generation);
        self
    }
}

/// Local error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalError {
    /// Error code.
    pub code: String,
    /// Message.
    pub message: String,
    /// Optional request ID.
    pub request_id: Option<RequestId>,
    /// Optional note path for context.
    pub note_path: Option<String>,
}

impl LocalError {
    /// Create new error.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            request_id: None,
            note_path: None,
        }
    }

    /// Create new error with request ID.
    #[must_use]
    pub fn with_request(
        code: impl Into<String>,
        message: impl Into<String>,
        request_id: RequestId,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            request_id: Some(request_id),
            note_path: None,
        }
    }

    /// Add note path context.
    #[must_use]
    pub fn with_note_path(mut self, path: impl Into<String>) -> Self {
        self.note_path = Some(path.into());
        self
    }
}

/// Rejected result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RejectedResult {
    /// Reason.
    pub reason: RejectionReason,
    /// Optional request ID.
    pub request_id: Option<RequestId>,
    /// Optional message.
    pub message: Option<String>,
}

impl RejectedResult {
    /// Create new rejected result.
    #[must_use]
    pub fn new(reason: RejectionReason) -> Self {
        Self {
            reason,
            request_id: None,
            message: None,
        }
    }

    /// Create with request ID.
    #[must_use]
    pub fn with_request(reason: RejectionReason, request_id: RequestId) -> Self {
        Self {
            reason,
            request_id: Some(request_id),
            message: None,
        }
    }

    /// Add message.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }
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
    /// Unsupported command.
    Unsupported,
    /// Invalid arguments.
    InvalidArgs,
    /// Caller did not declare the required capability.
    CapabilityDenied,
    /// Conflict state.
    Conflict,
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
            request_id: None,
            message: None,
        });
        assert!(!rejected.is_success());
        assert!(!rejected.is_pending());
        assert!(rejected.is_rejected());
    }

    #[test]
    fn result_accessors_cover_all_variants() {
        let success = ExecutionResult::Local(LocalResult::Success(
            LocalSuccess::with_command(7, "save", serde_json::json!({"ok": true}))
                .with_version(2, 9),
        ));
        assert_eq!(success.request_id(), None);
        assert_eq!(success.any_request_id(), Some(7));
        assert_eq!(success.clone().into_success().unwrap().generation, Some(9));
        assert!(success.clone().into_error().is_none());

        let void = ExecutionResult::Local(LocalResult::Void);
        assert_eq!(void.any_request_id(), None);
        assert!(void.clone().into_success().is_none());
        assert!(!void.is_success());

        let error = ExecutionResult::Local(LocalResult::Error(
            LocalError::with_request("conflict", "changed", 8).with_note_path("a.md"),
        ));
        assert_eq!(error.any_request_id(), Some(8));
        assert_eq!(
            error.clone().into_error().unwrap().note_path.as_deref(),
            Some("a.md")
        );
        assert!(LocalResult::Error(LocalError::new("x", "y")).is_error());

        let pending = ExecutionResult::Pending { request_id: 9 };
        assert_eq!(pending.request_id(), Some(9));
        assert_eq!(pending.any_request_id(), Some(9));
        assert!(pending.clone().into_success().is_none());
        assert!(pending.into_error().is_none());

        let rejected = ExecutionResult::Rejected(
            RejectedResult::with_request(RejectionReason::Busy, 10).with_message("busy"),
        );
        assert_eq!(rejected.any_request_id(), None);
        assert!(rejected.clone().into_error().is_none());
        assert!(rejected.is_rejected());
    }
}
