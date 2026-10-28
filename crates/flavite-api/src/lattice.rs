//! Lattice facade — owns Session and command registry (§4.5, D16).
//!
//! This module provides the main public API facade that owns:
//! - The headless Session (from flavite-vault)
//! - The command registry
//! - Revisioned read views
//! - Event bus
//!
//! All external consumers (GUI, CLI, future plugins) interact with the core
//! system through this facade.

use crate::command::{CommandRegistry, Invocation};
use crate::event::Event;
use crate::result::{
    ExecutionResult, LocalError, LocalResult, LocalSuccess, RejectedResult, RejectionReason,
};
use crate::types::{ApiVaultRef, RequestId};
use crossbeam::channel::{Receiver, Sender, unbounded};
use flavite_buffer::revision::{BufferId, Revision};
use flavite_vault::session::{Session, SessionCommand, SessionError, SessionResult};
use flavite_vault::types::{Epoch, Generation, VaultConfig, VaultIdentity};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Re-export vault types for API consumers.
pub use flavite_vault::types::{ReconcileMode, ReconcileScope};

/// Result of opening a vault.
#[derive(Debug)]
pub struct OpenVaultResult {
    /// The vault reference.
    pub vault: ApiVaultRef,
    /// The epoch.
    pub epoch: Epoch,
    /// The generation.
    pub generation: Generation,
}

/// A handle to the Lattice API.
///
/// This is the main entry point for interacting with a vault through the
/// public API. It owns the Session and provides typed command dispatch,
/// revisioned read views, and event subscriptions.
#[derive(Debug)]
#[allow(dead_code)]
pub struct Lattice {
    /// Vault identity.
    identity: VaultIdentity,
    /// The underlying Session.
    session: Arc<Session>,
    /// Command registry.
    command_registry: CommandRegistry,
    /// Event sender for subscribers.
    event_sender: Sender<Event>,
    /// Event receiver (kept for drop behavior).
    _event_receiver: Receiver<Event>,
    /// Subscribers to events.
    subscribers: Arc<Mutex<Vec<Sender<Event>>>>,
    /// The API vault reference.
    vault_ref: ApiVaultRef,
    /// Whether the vault is read-only.
    read_only: bool,
}

impl Lattice {
    /// Open a vault and create a Lattice facade.
    ///
    /// # Arguments
    /// * `identity` - The vault identity
    /// * `session` - The headless Session
    /// * `config` - Vault configuration
    ///
    /// # Returns
    /// A new Lattice facade, or an error if the vault cannot be opened.
    pub fn open(
        identity: VaultIdentity,
        session: Session,
        _config: VaultConfig,
    ) -> Result<Self, LatticeError> {
        let vault_ref = ApiVaultRef::new(identity.uuid, identity.root.clone());

        let (event_sender, event_receiver) = unbounded();

        let lattice = Self {
            identity,
            session: Arc::new(session),
            command_registry: CommandRegistry::new(),
            event_sender,
            _event_receiver: event_receiver,
            subscribers: Arc::new(Mutex::new(Vec::new())),
            vault_ref,
            read_only: false,
        };

        // Register built-in commands
        lattice.register_builtin_commands()?;

        Ok(lattice)
    }

    /// Create a new Lattice with a custom session builder.
    ///
    /// This is useful for testing or when you need to customize the session.
    pub fn with_session(
        session: Session,
        vault_ref: ApiVaultRef,
        _config: VaultConfig,
    ) -> Result<Self, LatticeError> {
        let (event_sender, event_receiver) = unbounded();

        let lattice = Self {
            identity: VaultIdentity {
                uuid: vault_ref.uuid,
                root: vault_ref.root.clone(),
                root_key: Vec::new(),
            },
            session: Arc::new(session),
            command_registry: CommandRegistry::new(),
            event_sender,
            _event_receiver: event_receiver,
            subscribers: Arc::new(Mutex::new(Vec::new())),
            vault_ref,
            read_only: false,
        };

        lattice.register_builtin_commands()?;

        Ok(lattice)
    }

    /// Create a new Lattice with a shared Session reference.
    ///
    /// This is useful when the Session is owned by another component
    /// (e.g., the Vault in the CLI integration).
    pub fn with_vault(
        identity: VaultIdentity,
        session: Arc<Session>,
        _config: VaultConfig,
    ) -> Result<Self, LatticeError> {
        let vault_ref = ApiVaultRef::new(identity.uuid, identity.root.clone());
        let (event_sender, event_receiver) = unbounded();

        let lattice = Self {
            identity,
            session,
            command_registry: CommandRegistry::new(),
            event_sender,
            _event_receiver: event_receiver,
            subscribers: Arc::new(Mutex::new(Vec::new())),
            vault_ref,
            read_only: false,
        };

        lattice.register_builtin_commands()?;

        Ok(lattice)
    }

    /// Register built-in commands.
    fn register_builtin_commands(&self) -> Result<(), LatticeError> {
        // Register core commands
        // Note: Full command implementations will be added as other slices land
        Ok(())
    }

    /// Get the vault reference.
    #[must_use]
    pub fn vault_ref(&self) -> &ApiVaultRef {
        &self.vault_ref
    }

    /// Get the command registry.
    #[must_use]
    pub fn command_registry(&self) -> &CommandRegistry {
        &self.command_registry
    }

    /// Check if the vault is read-only.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Set the vault to read-only mode.
    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    /// Subscribe to events.
    ///
    /// Returns a receiver for events from this vault.
    pub fn subscribe(&self) -> Receiver<Event> {
        let (sender, receiver) = unbounded();
        {
            let mut subscribers = self.subscribers.lock().unwrap();
            subscribers.push(sender);
        }
        receiver
    }

    /// Get the underlying Session (for advanced use cases).
    #[must_use]
    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    /// Execute a Session command directly.
    pub fn execute_session(&self, command: SessionCommand) -> SessionResult {
        self.session.execute(command)
    }

    /// Execute a typed command through the registry.
    pub fn execute(&self, invocation: Invocation) -> ExecutionResult {
        // Check if read-only and command requires write access
        if self.read_only && invocation.command_id.starts_with("write:") {
            return ExecutionResult::Rejected(RejectedResult {
                reason: RejectionReason::ReadOnly,
                request_id: None,
                message: None,
            });
        }

        // Dispatch to the appropriate handler
        let result = self.dispatch_command(&invocation);

        // Convert SessionResult to ExecutionResult
        match result {
            Ok(session_result) => {
                self.convert_session_result(session_result, invocation.request_id)
            }
            Err(e) => ExecutionResult::Local(LocalResult::Error(LocalError::new(
                "dispatch_error",
                e.to_string(),
            ))),
        }
    }

    /// Dispatch a command to the appropriate handler.
    fn dispatch_command(&self, invocation: &Invocation) -> Result<SessionResult, LatticeError> {
        // For now, directly map known command IDs to Session commands
        // In the future, this will use a proper handler registry
        match invocation.command_id {
            "open_note" => {
                let path: PathBuf = serde_json::from_value(invocation.args.clone())
                    .map_err(|e| LatticeError::InvalidArgs(e.to_string()))?;
                Ok(self.session.execute(SessionCommand::OpenNote { path }))
            }
            "close_note" => {
                let path: PathBuf = serde_json::from_value(invocation.args.clone())
                    .map_err(|e| LatticeError::InvalidArgs(e.to_string()))?;
                Ok(self.session.execute(SessionCommand::CloseNote { path }))
            }
            "save_note" => {
                let path: PathBuf = serde_json::from_value(invocation.args.clone())
                    .map_err(|e| LatticeError::InvalidArgs(e.to_string()))?;
                Ok(self.session.execute(SessionCommand::SaveNote { path }))
            }
            "apply_edits" => {
                // Deserialize edit operations
                #[derive(Deserialize)]
                struct ApplyEditsArgs {
                    buffer_id: u64,
                    revision: u64,
                    ops: Vec<EditOpArgs>,
                }

                #[derive(Deserialize)]
                struct EditOpArgs {
                    range: (usize, usize),
                    replacement: String,
                }

                let args: ApplyEditsArgs = serde_json::from_value(invocation.args.clone())
                    .map_err(|e| LatticeError::InvalidArgs(e.to_string()))?;

                // Convert to EditOps
                use flavite_buffer::undo::EditOp;
                use flavite_syntax::text::{Span, TextByte};
                let ops: Vec<EditOp> = args
                    .ops
                    .into_iter()
                    .map(|op| {
                        EditOp::new(
                            Span::new(TextByte(op.range.0), TextByte(op.range.1)),
                            op.replacement,
                        )
                    })
                    .collect();

                Ok(self.session.execute(SessionCommand::ApplyEdits {
                    buffer_id: BufferId::from(args.buffer_id),
                    revision: Revision::new(args.revision),
                    ops,
                }))
            }
            "undo" => {
                #[derive(Deserialize)]
                struct UndoArgs {
                    buffer_id: u64,
                }
                let args: UndoArgs = serde_json::from_value(invocation.args.clone())
                    .map_err(|e| LatticeError::InvalidArgs(e.to_string()))?;
                Ok(self.session.execute(SessionCommand::Undo {
                    buffer_id: BufferId::from(args.buffer_id),
                }))
            }
            "redo" => {
                #[derive(Deserialize)]
                struct RedoArgs {
                    buffer_id: u64,
                }
                let args: RedoArgs = serde_json::from_value(invocation.args.clone())
                    .map_err(|e| LatticeError::InvalidArgs(e.to_string()))?;
                Ok(self.session.execute(SessionCommand::Redo {
                    buffer_id: BufferId::from(args.buffer_id),
                }))
            }
            "reconcile" => {
                #[derive(Deserialize)]
                struct ReconcileArgs {
                    scope: String,
                    mode: String,
                }
                let args: ReconcileArgs = serde_json::from_value(invocation.args.clone())
                    .map_err(|e| LatticeError::InvalidArgs(e.to_string()))?;

                // Convert string to ReconcileScope
                let scope = match args.scope.as_str() {
                    "full" => ReconcileScope::Full,
                    _ => ReconcileScope::Full, // Default to Full for now
                };

                // Convert string to ReconcileMode
                let mode = match args.mode.as_str() {
                    "fast" => ReconcileMode::FastMetadata,
                    "verify" => ReconcileMode::VerifyContent,
                    _ => ReconcileMode::FastMetadata, // Default to FastMetadata
                };

                Ok(self
                    .session
                    .execute(SessionCommand::Reconcile { scope, mode }))
            }
            "resolve_conflict" => {
                #[derive(Deserialize)]
                struct ResolveArgs {
                    path: PathBuf,
                    resolution: String,
                }
                let args: ResolveArgs = serde_json::from_value(invocation.args.clone())
                    .map_err(|e| LatticeError::InvalidArgs(e.to_string()))?;

                let resolution = match args.resolution.as_str() {
                    "keep_mine" => crate::session::ConflictResolution::KeepMine,
                    "reload" => crate::session::ConflictResolution::Reload,
                    "save_as" => crate::session::ConflictResolution::SaveAs {
                        new_path: PathBuf::new(),
                    },
                    "merge" => crate::session::ConflictResolution::Merge,
                    _ => {
                        return Err(LatticeError::InvalidArgs(
                            "Invalid resolution strategy".to_string(),
                        ));
                    }
                };

                Ok(self.session.execute(SessionCommand::ResolveConflict {
                    path: args.path,
                    resolution,
                }))
            }
            _ => Err(LatticeError::UnknownCommand(
                invocation.command_id.to_string(),
            )),
        }
    }

    /// Convert a SessionResult to an ExecutionResult.
    fn convert_session_result(
        &self,
        result: SessionResult,
        request_id: RequestId,
    ) -> ExecutionResult {
        match result {
            SessionResult::Success(cmd_result) => {
                let data = match cmd_result {
                    crate::session::CommandResult::NoteOpened {
                        buffer_id,
                        revision,
                    } => {
                        serde_json::json!({
                            "buffer_id": buffer_id.as_u64(),
                            "revision": revision.as_u64(),
                        })
                    }
                    crate::session::CommandResult::NoteClosed => serde_json::Value::Null,
                    crate::session::CommandResult::EditsApplied {
                        buffer_id,
                        new_revision,
                    } => {
                        serde_json::json!({
                            "buffer_id": buffer_id.as_u64(),
                            "revision": new_revision.as_u64(),
                        })
                    }
                    crate::session::CommandResult::NoteSaved {
                        path,
                        hash,
                        epoch,
                        generation,
                    } => {
                        serde_json::json!({
                            "path": path.to_string_lossy(),
                            "hash": hex::encode(hash),
                            "epoch": epoch,
                            "generation": generation,
                        })
                    }
                    crate::session::CommandResult::ReconcileComplete { summary } => {
                        serde_json::json!({
                            "added": summary.added,
                            "changed": summary.changed,
                            "removed": summary.removed,
                        })
                    }
                    crate::session::CommandResult::UndoPerformed {
                        buffer_id,
                        new_revision,
                    } => {
                        serde_json::json!({
                            "buffer_id": buffer_id.as_u64(),
                            "revision": new_revision.as_u64(),
                        })
                    }
                    crate::session::CommandResult::RedoPerformed {
                        buffer_id,
                        new_revision,
                    } => {
                        serde_json::json!({
                            "buffer_id": buffer_id.as_u64(),
                            "revision": new_revision.as_u64(),
                        })
                    }
                    crate::session::CommandResult::ConflictResolved => serde_json::Value::Null,
                    crate::session::CommandResult::RecoveryRestored {
                        buffer_id,
                        revision,
                    } => serde_json::json!({
                        "buffer_id": buffer_id.as_u64(),
                        "revision": revision.as_u64(),
                    }),
                };
                ExecutionResult::Local(LocalResult::Success(LocalSuccess::new(request_id, data)))
            }
            SessionResult::Pending { request_id: req_id } => {
                ExecutionResult::Pending { request_id: req_id }
            }
            SessionResult::Superseded {
                request_id: req_id,
                by_request,
            } => ExecutionResult::Local(LocalResult::Error(LocalError::with_request(
                "superseded",
                format!("Save request {req_id} was superseded by request {by_request}"),
                req_id,
            ))),
            SessionResult::Error { message } => ExecutionResult::Local(LocalResult::Error(
                LocalError::new("session_error", message),
            )),
            SessionResult::Conflict { path } => {
                ExecutionResult::Local(LocalResult::Error(LocalError::new(
                    "conflict",
                    format!("Conflict detected: {}", path.to_string_lossy()),
                )))
            }
            SessionResult::MergeReady { path, result, .. } => {
                let data = serde_json::json!({
                    "path": path.to_string_lossy(),
                    "merged": result.merged,
                    "conflicts": result.conflicts.iter().map(|conflict| {
                        serde_json::json!({
                            "base_start_line": conflict.base_start_line,
                            "base_end_line": conflict.base_end_line,
                            "mine": conflict.mine,
                            "theirs": conflict.theirs,
                        })
                    }).collect::<Vec<_>>(),
                });
                ExecutionResult::Local(LocalResult::Success(LocalSuccess::new(request_id, data)))
            }
            SessionResult::ActionRequired { code, data } => ExecutionResult::Local(
                LocalResult::Error(LocalError::new(code, data.unwrap_or_default())),
            ),
        }
    }

    /// Get a revisioned read view of a buffer.
    ///
    /// Returns the current text and metadata for a buffer at a specific revision.
    pub fn get_buffer_view(
        &self,
        _buffer_id: BufferId,
        _revision: Option<Revision>,
    ) -> Result<BufferView, LatticeError> {
        // For now, this is a stub - full implementation requires BufferManager access
        Err(LatticeError::NotImplemented("get_buffer_view".to_string()))
    }

    /// Query the index.
    ///
    /// Note: This will be fully implemented when flavite-index is available.
    pub fn query(&self, _query: &crate::Query) -> Result<QueryResult, LatticeError> {
        Err(LatticeError::NotImplemented("query".to_string()))
    }
}

/// A revisioned read view of a buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferView {
    /// Buffer ID.
    pub buffer_id: u64,
    /// Buffer revision.
    pub revision: u64,
    /// The text content.
    pub text: String,
    /// The base hash.
    pub base_hash: Vec<u8>,
    /// Current state.
    pub state: String,
}

/// Result of a query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    /// Note references matching the query.
    pub notes: Vec<NoteResult>,
    /// Epoch.
    pub epoch: u64,
    /// Generation.
    pub generation: u64,
    /// Whether there are more results.
    pub has_more: bool,
}

/// A note in query results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteResult {
    /// Note ID.
    pub note_id: i64,
    /// Path.
    pub path: String,
    /// Title.
    pub title: String,
}

/// Errors for Lattice operations.
#[derive(Debug, Error)]
pub enum LatticeError {
    /// The vault is already open.
    #[error("Vault already open")]
    AlreadyOpen,

    /// Invalid command arguments.
    #[error("Invalid arguments: {0}")]
    InvalidArgs(String),

    /// Unknown command.
    #[error("Unknown command: {0}")]
    UnknownCommand(String),

    /// Feature not yet implemented.
    #[error("Not implemented: {0}")]
    NotImplemented(String),

    /// Session error.
    #[error("Session error: {0}")]
    SessionError(#[from] SessionError),

    /// Vault open error.
    #[error("Vault open error: {0}")]
    VaultOpenError(String),

    /// JSON serialization error.
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
}

/// Session module re-export for convenience.
pub mod session {
    pub use flavite_vault::session::{
        CommandResult, ConflictResolution, SessionCommand, SessionError,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lattice_creation() {
        // This test will be expanded once Session can be properly constructed
        // For now, we just verify the types compile
    }

    #[test]
    fn buffer_view_serialization() {
        let view = BufferView {
            buffer_id: 1,
            revision: 1,
            text: "test content".to_string(),
            base_hash: vec![1, 2, 3],
            state: "clean".to_string(),
        };

        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("test content"));
    }

    #[test]
    fn query_result_serialization() {
        let result = QueryResult {
            notes: vec![NoteResult {
                note_id: 1,
                path: "/test/note.md".to_string(),
                title: "Test Note".to_string(),
            }],
            epoch: 1,
            generation: 1,
            has_more: false,
        };

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("Test Note"));
    }
}
