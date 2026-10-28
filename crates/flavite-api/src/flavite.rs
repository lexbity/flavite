//! Flavite facade — owns Session and command registry (§4.5, D16).
//!
//! This module provides the main public API facade that owns:
//! - The headless Session (from flavite-vault)
//! - The command registry
//! - Revisioned read views
//! - Event bus
//!
//! All external consumers (GUI, CLI, future plugins) interact with the core
//! system through this facade.

use crate::command::{
    ArgSchema, CommandDispatchError, CommandHandler, CommandRegistry, CommandSpec, Invocation,
};
use crate::event::Event;
use crate::result::{
    ExecutionResult, LocalError, LocalResult, LocalSuccess, RejectedResult, RejectionReason,
};
use crate::types::{ApiVaultRef, RequestId};
use crossbeam::channel::{Receiver, Sender, unbounded};
use flavite_buffer::revision::{BufferId, Revision};
use flavite_vault::open::Vault;
use flavite_vault::session::{Session, SessionCommand, SessionError, SessionResult};
use flavite_vault::types::{Epoch, Generation, VaultConfig, VaultIdentity};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Re-export vault types for API consumers.
pub use flavite_vault::types::{ReconcileMode, ReconcileScope};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditOpArgs {
    range: (usize, usize),
    replacement: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyEditsArgs {
    buffer_id: u64,
    revision: u64,
    ops: Vec<EditOpArgs>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BufferRevisionArgs {
    buffer_id: u64,
    revision: u64,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReconcileArgs {
    scope: String,
    mode: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveArgs {
    path: PathBuf,
    resolution: String,
    new_path: Option<PathBuf>,
}

fn decode<T: serde::de::DeserializeOwned>(invocation: &Invocation) -> Result<T, String> {
    serde_json::from_value(invocation.args.clone()).map_err(|error| error.to_string())
}

/// Arguments for the note-open commands: a bare vault-relative path, or an
/// object form (`{"path": …}`) so hosts can attach navigation metadata
/// (target, anchor, by_id, create_if_unresolved) alongside the path.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OpenArgs {
    Path(PathBuf),
    Object { path: PathBuf },
}

fn handle_open(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let path = match decode::<OpenArgs>(invocation)? {
        OpenArgs::Path(path) | OpenArgs::Object { path } => path,
    };
    Ok(session.execute(SessionCommand::OpenNote { path }))
}
fn handle_close(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let path = decode(invocation)?;
    Ok(session.execute(SessionCommand::CloseNote { path }))
}
fn handle_save(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let path = decode(invocation)?;
    Ok(session.execute(SessionCommand::SaveNote { path }))
}
fn handle_edits(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let args: ApplyEditsArgs = decode(invocation)?;
    use flavite_buffer::undo::EditOp;
    use flavite_syntax::text::{Span, TextByte};
    let ops = args
        .ops
        .into_iter()
        .map(|op| {
            EditOp::new(
                Span::new(TextByte(op.range.0), TextByte(op.range.1)),
                op.replacement,
            )
        })
        .collect();
    Ok(session.execute(SessionCommand::ApplyEdits {
        buffer_id: BufferId::from(args.buffer_id),
        revision: Revision::new(args.revision),
        ops,
    }))
}
fn handle_undo(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let args: BufferRevisionArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::Undo {
        buffer_id: BufferId::from(args.buffer_id),
        revision: Revision::new(args.revision),
    }))
}
fn handle_redo(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let args: BufferRevisionArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::Redo {
        buffer_id: BufferId::from(args.buffer_id),
        revision: Revision::new(args.revision),
    }))
}
fn handle_reconcile(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let args: ReconcileArgs = decode(invocation)?;
    let scope = match args.scope.as_str() {
        "full" => ReconcileScope::Full,
        _ => return Err("scope must be `full`".into()),
    };
    let mode = match args.mode.as_str() {
        "fast" => ReconcileMode::FastMetadata,
        "verify" => ReconcileMode::VerifyContent,
        _ => return Err("mode must be `fast` or `verify`".into()),
    };
    Ok(session.execute(SessionCommand::Reconcile { scope, mode }))
}
fn handle_resolve(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let args: ResolveArgs = decode(invocation)?;
    let resolution = match args.resolution.as_str() {
        "keep_mine" => crate::session::ConflictResolution::KeepMine,
        "reload" => crate::session::ConflictResolution::Reload,
        "save_as" => crate::session::ConflictResolution::SaveAs {
            new_path: args.new_path.ok_or("save_as requires new_path")?,
        },
        "merge" => crate::session::ConflictResolution::Merge,
        _ => return Err("resolution must be keep_mine, reload, save_as, or merge".into()),
    };
    Ok(session.execute(SessionCommand::ResolveConflict {
        path: args.path,
        resolution,
    }))
}

type HandlerRegistration = (&'static str, &'static str, ArgSchema, CommandHandler);

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

/// A handle to the Flavite API.
///
/// This is the main entry point for interacting with a vault through the
/// public API. It owns the Session and provides typed command dispatch,
/// revisioned read views, and event subscriptions.
#[derive(Debug)]
#[allow(dead_code)]
pub struct Flavite {
    /// Vault identity.
    identity: VaultIdentity,
    /// The underlying Session.
    session: Arc<Session>,
    /// Command registry.
    command_registry: CommandRegistry,
    /// Serializes commands and worker completion processing on the host turn.
    ingress: Mutex<()>,
    /// Public request IDs corresponding to Session worker IDs.
    operation_ids: Mutex<HashMap<RequestId, RequestId>>,
    /// Public operations still awaiting a terminal result.
    pending_requests: Mutex<HashSet<RequestId>>,
    /// Completions collected while waiting for another operation.
    terminal_backlog: Mutex<HashMap<RequestId, ExecutionResult>>,
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
    /// Keeps root ownership and actor resources alive for `open_root` callers.
    owned_vault: Option<Vault>,
}

impl Flavite {
    /// Open a vault and create a Flavite facade.
    ///
    /// # Arguments
    /// * `identity` - The vault identity
    /// * `session` - The headless Session
    /// * `config` - Vault configuration
    ///
    /// # Returns
    /// A new Flavite facade, or an error if the vault cannot be opened.
    pub fn open(
        identity: VaultIdentity,
        session: Session,
        _config: VaultConfig,
    ) -> Result<Self, FlaviteError> {
        let vault_ref = ApiVaultRef::new(identity.uuid, identity.root.clone());

        let (event_sender, event_receiver) = unbounded();

        let flavite = Self {
            identity,
            session: Arc::new(session),
            command_registry: CommandRegistry::new(),
            ingress: Mutex::new(()),
            operation_ids: Mutex::new(HashMap::new()),
            pending_requests: Mutex::new(HashSet::new()),
            terminal_backlog: Mutex::new(HashMap::new()),
            event_sender,
            _event_receiver: event_receiver,
            subscribers: Arc::new(Mutex::new(Vec::new())),
            vault_ref,
            read_only: false,
            owned_vault: None,
        };

        // Register built-in commands
        flavite.register_builtin_commands()?;

        Ok(flavite)
    }

    /// Create a new Flavite with a custom session builder.
    ///
    /// This is useful for testing or when you need to customize the session.
    pub fn with_session(
        session: Session,
        vault_ref: ApiVaultRef,
        _config: VaultConfig,
    ) -> Result<Self, FlaviteError> {
        let (event_sender, event_receiver) = unbounded();

        let flavite = Self {
            identity: VaultIdentity {
                uuid: vault_ref.uuid,
                root: vault_ref.root.clone(),
                root_key: Vec::new(),
            },
            session: Arc::new(session),
            command_registry: CommandRegistry::new(),
            ingress: Mutex::new(()),
            operation_ids: Mutex::new(HashMap::new()),
            pending_requests: Mutex::new(HashSet::new()),
            terminal_backlog: Mutex::new(HashMap::new()),
            event_sender,
            _event_receiver: event_receiver,
            subscribers: Arc::new(Mutex::new(Vec::new())),
            vault_ref,
            read_only: false,
            owned_vault: None,
        };

        flavite.register_builtin_commands()?;

        Ok(flavite)
    }

    /// Create a new Flavite with a shared Session reference.
    ///
    /// This is useful when the Session is owned by another component
    /// (e.g., the Vault in the CLI integration).
    pub fn with_vault(
        identity: VaultIdentity,
        session: Arc<Session>,
        _config: VaultConfig,
    ) -> Result<Self, FlaviteError> {
        let vault_ref = ApiVaultRef::new(identity.uuid, identity.root.clone());
        let (event_sender, event_receiver) = unbounded();

        let flavite = Self {
            identity,
            session,
            command_registry: CommandRegistry::new(),
            ingress: Mutex::new(()),
            operation_ids: Mutex::new(HashMap::new()),
            pending_requests: Mutex::new(HashSet::new()),
            terminal_backlog: Mutex::new(HashMap::new()),
            event_sender,
            _event_receiver: event_receiver,
            subscribers: Arc::new(Mutex::new(Vec::new())),
            vault_ref,
            read_only: false,
            owned_vault: None,
        };

        flavite.register_builtin_commands()?;

        Ok(flavite)
    }

    /// Open a vault and construct the public facade while retaining ownership
    /// guards for the lifetime of the facade.
    pub fn open_root(root: PathBuf, config: VaultConfig) -> Result<Self, FlaviteError> {
        let vault = Vault::open(root, Box::new(flavite_fs::OsFs::new()), config.clone())
            .map_err(|error| FlaviteError::VaultOpenError(error.to_string()))?;
        let mut flavite = Self::with_vault(vault.identity().clone(), vault.session_arc(), config)?;
        flavite.owned_vault = Some(vault);
        Ok(flavite)
    }

    /// Register built-in commands.
    ///
    /// Each handler is registered under its bare id and, where consumers
    /// (editor widget, UI shell, palette) predate the naming cleanup, under
    /// the legacy dotted alias as well. Both ids resolve to the same handler
    /// and schema.
    fn register_builtin_commands(&self) -> Result<(), FlaviteError> {
        let registrations: [HandlerRegistration; 15] = [
            (
                "open_note",
                "Open note",
                ArgSchema::typed::<OpenArgs>(),
                handle_open,
            ),
            (
                "note.open",
                "Open note",
                ArgSchema::typed::<OpenArgs>(),
                handle_open,
            ),
            (
                "close_note",
                "Close note",
                ArgSchema::typed::<PathBuf>(),
                handle_close,
            ),
            (
                "note.close",
                "Close note",
                ArgSchema::typed::<PathBuf>(),
                handle_close,
            ),
            (
                "save_note",
                "Save note",
                ArgSchema::typed::<PathBuf>(),
                handle_save,
            ),
            (
                "note.save",
                "Save note",
                ArgSchema::typed::<PathBuf>(),
                handle_save,
            ),
            (
                "apply_edits",
                "Apply edits",
                ArgSchema::typed::<ApplyEditsArgs>(),
                handle_edits,
            ),
            (
                "link.insert",
                "Insert link",
                ArgSchema::typed::<ApplyEditsArgs>(),
                handle_edits,
            ),
            (
                "undo",
                "Undo",
                ArgSchema::typed::<BufferRevisionArgs>(),
                handle_undo,
            ),
            (
                "buffer.undo",
                "Undo",
                ArgSchema::typed::<BufferRevisionArgs>(),
                handle_undo,
            ),
            (
                "redo",
                "Redo",
                ArgSchema::typed::<BufferRevisionArgs>(),
                handle_redo,
            ),
            (
                "buffer.redo",
                "Redo",
                ArgSchema::typed::<BufferRevisionArgs>(),
                handle_redo,
            ),
            (
                "reconcile",
                "Reconcile vault",
                ArgSchema::typed::<ReconcileArgs>(),
                handle_reconcile,
            ),
            (
                "vault.reconcile",
                "Refresh vault",
                ArgSchema::typed::<ReconcileArgs>(),
                handle_reconcile,
            ),
            (
                "resolve_conflict",
                "Resolve conflict",
                ArgSchema::typed::<ResolveArgs>(),
                handle_resolve,
            ),
        ];
        for (id, title, args, handler) in registrations {
            let spec = if matches!(id, "open_note" | "note.open") {
                CommandSpec::new(id, title, args)
            } else {
                CommandSpec::new(id, title, args).write()
            };
            self.command_registry
                .register(spec, handler)
                .map_err(|error| FlaviteError::Registration(format!("{error:?}")))?;
        }
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

    /// Drain lossy pulse events through the public host boundary.
    pub fn drain_pulses(&self) -> Vec<flavite_vault::channels::Pulse> {
        self.session.drain_pulses()
    }

    /// Drain lossless notices through the public host boundary.
    pub fn drain_notices(&self) -> Vec<flavite_vault::channels::Notice> {
        self.session.drain_notices()
    }

    /// Acknowledge notice presentation. This does not complete commands or journal work.
    pub fn acknowledge_notice(&self, notice: &flavite_vault::channels::Notice) {
        self.session.acknowledge_notice(notice);
    }

    /// Return a revision-independent snapshot of indexed note titles, aliases,
    /// and unconflicted stable IDs for autocomplete consumers.
    pub fn completion_candidates(&self) -> Result<Vec<crate::CompletionCandidate>, FlaviteError> {
        self.session
            .completion_candidates()
            .map_err(FlaviteError::IndexError)
    }

    /// Read the versioned vault settings document through the API boundary.
    pub fn read_settings(&self) -> Result<Option<String>, FlaviteError> {
        let path = self.vault_ref.root.join(".flavite").join("settings.toml");
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(FlaviteError::SettingsIo(
                "settings.toml must not be a symlink".into(),
            ));
        }
        match std::fs::read_to_string(path) {
            Ok(contents) => Ok(Some(contents)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(FlaviteError::SettingsIo(error.to_string())),
        }
    }

    /// Atomically write the versioned vault settings document through the API boundary.
    pub fn write_settings(&self, contents: &str) -> Result<(), FlaviteError> {
        let directory = self.vault_ref.root.join(".flavite");
        std::fs::create_dir_all(&directory)
            .map_err(|error| FlaviteError::SettingsIo(error.to_string()))?;
        let path = directory.join("settings.toml");
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(FlaviteError::SettingsIo(
                "settings.toml must not be a symlink".into(),
            ));
        }
        flavite_fs::atomic::atomic_write(&flavite_fs::OsFs::new(), &path, contents.as_bytes())
            .map(|_| ())
            .map_err(|error| FlaviteError::SettingsIo(error.to_string()))
    }

    /// Execute a typed command through the registry.
    pub fn execute(&self, invocation: Invocation) -> ExecutionResult {
        let _turn = self.ingress.lock().unwrap();
        if let Err(error) = self
            .command_registry
            .validate(invocation.command_id, &invocation.args)
        {
            let (reason, message) = match error {
                crate::command::CommandValidationError::Unavailable(id) => (
                    RejectionReason::Unavailable,
                    format!("command unavailable: {id}"),
                ),
                crate::command::CommandValidationError::InvalidArgs(message) => {
                    (RejectionReason::InvalidArgs, message)
                }
            };
            return ExecutionResult::Rejected(
                RejectedResult::with_request(reason, invocation.request_id).with_message(message),
            );
        }
        if let Some(buffer_id) = invocation.preconditions.buffer_id {
            let revision = invocation.preconditions.expected_revision;
            match self.get_buffer_view(BufferId::from(buffer_id), revision.map(Revision::new)) {
                Ok(_) => {}
                Err(FlaviteError::BufferNotFound(_)) => {
                    return ExecutionResult::Rejected(RejectedResult::with_request(
                        RejectionReason::BufferNotFound,
                        invocation.request_id,
                    ));
                }
                Err(FlaviteError::StaleBufferView { .. }) => {
                    return ExecutionResult::Rejected(RejectedResult::with_request(
                        RejectionReason::StaleRevision,
                        invocation.request_id,
                    ));
                }
                Err(error) => {
                    return ExecutionResult::Rejected(
                        RejectedResult::with_request(
                            RejectionReason::Unsupported,
                            invocation.request_id,
                        )
                        .with_message(error.to_string()),
                    );
                }
            }
        }
        let requires_write = self
            .command_registry
            .get_spec(invocation.command_id)
            .is_some_and(|spec| spec.requires_write)
            || invocation.preconditions.requires_write;
        if self.read_only && requires_write && !invocation.preconditions.allow_read_only {
            return ExecutionResult::Rejected(RejectedResult::with_request(
                RejectionReason::ReadOnly,
                invocation.request_id,
            ));
        }
        let result = self.dispatch_command(&invocation);

        // Convert SessionResult to ExecutionResult
        match result {
            Ok(session_result) => {
                self.convert_session_result(session_result, invocation.request_id)
            }
            Err(error) => {
                let (reason, message) = match error {
                    CommandDispatchError::Unavailable(message) => {
                        (RejectionReason::Unavailable, message)
                    }
                    CommandDispatchError::InvalidArgs(message)
                    | CommandDispatchError::Handler(message) => {
                        (RejectionReason::InvalidArgs, message)
                    }
                };
                ExecutionResult::Rejected(
                    RejectedResult::with_request(reason, invocation.request_id)
                        .with_message(message),
                )
            }
        }
    }

    /// Dispatch a command to the appropriate handler.
    fn dispatch_command(
        &self,
        invocation: &Invocation,
    ) -> Result<SessionResult, CommandDispatchError> {
        self.command_registry.dispatch(&self.session, invocation)
    }

    /// Convert a SessionResult to an ExecutionResult.
    fn convert_session_result(
        &self,
        result: SessionResult,
        request_id: RequestId,
    ) -> ExecutionResult {
        let command_id = match &result {
            SessionResult::Success(crate::session::CommandResult::NoteOpened { .. }) => "open_note",
            SessionResult::Success(crate::session::CommandResult::NoteClosed) => "close_note",
            SessionResult::Success(crate::session::CommandResult::EditsApplied { .. }) => {
                "apply_edits"
            }
            SessionResult::Success(crate::session::CommandResult::NoteSaved { .. }) => "save_note",
            SessionResult::Success(crate::session::CommandResult::ReconcileComplete { .. }) => {
                "reconcile"
            }
            SessionResult::Success(crate::session::CommandResult::UndoPerformed { .. }) => "undo",
            SessionResult::Success(crate::session::CommandResult::RedoPerformed { .. }) => "redo",
            SessionResult::Success(crate::session::CommandResult::ConflictResolved) => {
                "resolve_conflict"
            }
            SessionResult::Success(crate::session::CommandResult::RecoveryRestored { .. }) => {
                "restore_recovery"
            }
            SessionResult::MergeReady { .. }
            | SessionResult::Conflict { .. }
            | SessionResult::ActionRequired { .. } => "resolve_conflict",
            SessionResult::Pending { .. }
            | SessionResult::Busy
            | SessionResult::StaleRevision { .. }
            | SessionResult::Superseded { .. }
            | SessionResult::Error { .. } => "",
        };
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
                        buffer_id,
                        saved_revision,
                        path,
                        hash,
                        epoch,
                        generation,
                    } => {
                        serde_json::json!({
                            "path": path.to_string_lossy(),
                            "buffer_id": buffer_id.as_u64(),
                            "saved_revision": saved_revision.as_u64(),
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
                ExecutionResult::Local(LocalResult::Success(LocalSuccess::with_command(
                    request_id, command_id, data,
                )))
            }
            SessionResult::Pending {
                request_id: session_id,
            } => {
                self.operation_ids
                    .lock()
                    .unwrap()
                    .insert(session_id, request_id);
                self.pending_requests.lock().unwrap().insert(request_id);
                ExecutionResult::Pending { request_id }
            }
            SessionResult::Busy => ExecutionResult::Rejected(
                RejectedResult::with_request(RejectionReason::Busy, request_id)
                    .with_message("operation queue is at capacity"),
            ),
            SessionResult::StaleRevision {
                buffer_id,
                expected,
                actual,
            } => ExecutionResult::Rejected(
                RejectedResult::with_request(RejectionReason::StaleRevision, request_id)
                    .with_message(format!(
                        "buffer {} has revision {}; command expected {}",
                        buffer_id.as_u64(),
                        actual.as_u64(),
                        expected.as_u64()
                    )),
            ),
            SessionResult::Error { message } => ExecutionResult::Local(LocalResult::Error(
                LocalError::with_request("session_error", message, request_id),
            )),
            SessionResult::Conflict { path } => {
                ExecutionResult::Local(LocalResult::Error(LocalError::with_request(
                    "conflict",
                    format!("Conflict detected: {}", path.to_string_lossy()),
                    request_id,
                )))
            }
            SessionResult::MergeReady {
                path,
                buffer_id,
                revision,
                disk_hash,
                result,
            } => {
                let data = serde_json::json!({
                    "path": path.to_string_lossy(),
                    "buffer_id": buffer_id,
                    "revision": revision,
                    "disk_hash": hex::encode(disk_hash),
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
                ExecutionResult::Local(LocalResult::Success(LocalSuccess::with_command(
                    request_id, command_id, data,
                )))
            }
            SessionResult::ActionRequired { code, data } => ExecutionResult::ActionRequired {
                request_id,
                code: code.to_string(),
                data: data.map(serde_json::Value::String),
            },
            SessionResult::Superseded { by_request, .. } => {
                let by_request = self
                    .operation_ids
                    .lock()
                    .unwrap()
                    .get(&by_request)
                    .copied()
                    .unwrap_or(by_request);
                ExecutionResult::Superseded {
                    request_id,
                    by_request,
                }
            }
        }
    }

    /// Drain lossless terminal results for admitted asynchronous operations.
    /// Results are correlated to the original public invocation ID.
    pub fn drain_operation_completions(&self) -> Vec<OperationCompletion> {
        let _turn = self.ingress.lock().unwrap();
        self.session
            .drain_operation_completions()
            .into_iter()
            .map(|completion| {
                let request_id = self
                    .operation_ids
                    .lock()
                    .unwrap()
                    .remove(&completion.request_id)
                    .unwrap_or(completion.request_id);
                self.pending_requests.lock().unwrap().remove(&request_id);
                let result = self.convert_session_result(completion.result, request_id);
                OperationCompletion { request_id, result }
            })
            .collect()
    }

    /// Wait for one admitted operation to reach a terminal result.
    pub fn wait_for_operation(&self, request_id: RequestId) -> ExecutionResult {
        loop {
            if let Some(result) = self.terminal_backlog.lock().unwrap().remove(&request_id) {
                return result;
            }
            if !self.pending_requests.lock().unwrap().contains(&request_id) {
                return ExecutionResult::Rejected(
                    RejectedResult::with_request(RejectionReason::Unavailable, request_id)
                        .with_message("operation is not pending or has already been drained"),
                );
            }
            for completion in self.drain_operation_completions() {
                if completion.request_id == request_id {
                    return completion.result;
                }
                self.terminal_backlog
                    .lock()
                    .unwrap()
                    .insert(completion.request_id, completion.result);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Get a revisioned read view of a buffer.
    ///
    /// Returns the current text and metadata for a buffer at a specific revision.
    pub fn get_buffer_view(
        &self,
        buffer_id: BufferId,
        revision: Option<Revision>,
    ) -> Result<BufferView, FlaviteError> {
        let snapshot = self
            .session
            .buffer_snapshot(buffer_id, revision)
            .map_err(|error| match error {
                flavite_vault::session::BufferSnapshotError::NotFound(id) => {
                    FlaviteError::BufferNotFound(id)
                }
                flavite_vault::session::BufferSnapshotError::Stale { expected, actual } => {
                    FlaviteError::StaleBufferView { expected, actual }
                }
            })?;
        Ok(BufferView {
            buffer_id: snapshot.buffer_id.as_u64(),
            revision: snapshot.revision.as_u64(),
            text: snapshot.text,
            base_hash: snapshot
                .base_hash
                .map_or_else(Vec::new, |hash| hash.to_vec()),
            state: snapshot.state,
            path: snapshot.path,
        })
    }

    /// Query the index.
    ///
    /// Query the structural index and Tantivy search view.
    pub fn query(&self, query: &crate::Query) -> Result<QueryResult, FlaviteError> {
        self.query_with_limit(query, 100)
    }

    /// Query the index with an explicit result limit.
    pub fn query_with_limit(
        &self,
        query: &crate::Query,
        limit: usize,
    ) -> Result<QueryResult, FlaviteError> {
        let page = self
            .session
            .search_notes(query.clone(), limit.saturating_add(1))
            .map_err(FlaviteError::IndexError)?;
        let has_more = page.items.len() > limit;
        let rows = page.items.into_iter().take(limit);
        Ok(QueryResult {
            notes: rows
                .map(|(note_id, path, title)| NoteResult {
                    note_id,
                    path,
                    title,
                })
                .collect(),
            epoch: page.epoch,
            generation: page.generation,
            has_more,
        })
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
    /// Vault-relative path used by save and close commands.
    pub path: PathBuf,
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

/// Errors for Flavite operations.
#[derive(Debug, Error)]
pub enum FlaviteError {
    /// The vault is already open.
    #[error("Vault already open")]
    AlreadyOpen,

    /// Invalid command arguments.
    #[error("Invalid arguments: {0}")]
    InvalidArgs(String),

    /// Command registration failed.
    #[error("command registration failed: {0}")]
    Registration(String),

    /// Unknown command.
    #[error("Unknown command: {0}")]
    UnknownCommand(String),

    /// Feature not yet implemented.
    #[error("Not implemented: {0}")]
    NotImplemented(String),

    /// The requested revision is no longer current.
    #[error("stale buffer view: expected revision {expected}, current revision is {actual}")]
    StaleBufferView { expected: u64, actual: u64 },

    /// The buffer identity is no longer open in this Session.
    #[error("buffer {0} is not open")]
    BufferNotFound(u64),

    /// The Session buffer manager lock was poisoned.
    #[error("buffer manager lock is poisoned")]
    BufferManagerPoisoned,

    /// The vault settings document could not be read or written.
    #[error("Settings I/O failed: {0}")]
    SettingsIo(String),

    /// Indexed lookup failed.
    #[error("Index error: {0}")]
    IndexError(String),

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
        CommandResult, ConflictResolution, SessionCommand, SessionError, SessionResult,
    };
}

/// Host-facing channel payloads for iced subscriptions and CLI adapters.
pub mod channels {
    pub use flavite_buffer::manager::NoteRef;
    pub use flavite_vault::channels::{Notice, Pulse, RenameTier};
}

/// Terminal completion for an asynchronous public command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OperationCompletion {
    /// Public invocation ID returned by `execute` when the command was pending.
    pub request_id: RequestId,
    /// Structured terminal result.
    pub result: ExecutionResult,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_flavite() -> (Flavite, PathBuf, tempfile::TempDir) {
        use flavite_buffer::manager::BufferManager;
        use flavite_fs::{Fs, MemFs};
        use flavite_index::IndexActor;
        use flavite_vault::actor::VaultActorBuilder;
        use flavite_vault::channels::ChannelSet;

        let fs = MemFs::new();
        let root = PathBuf::from("/test/flavite-api-vault");
        fs.create_dir_all(&root).unwrap();
        fs.write_new(&root.join("note.md"), b"hello").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("index.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let identity = VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![1, 2, 3]);
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs) as Box<dyn Fs + Send + Sync>)
            .with_identity(identity.clone())
            .with_index(IndexActor::new(&db, 1).unwrap())
            .build()
            .unwrap()
            .start()
            .unwrap();
        let channels = ChannelSet::new();
        let session = Session::new(
            identity.clone(),
            actor,
            BufferManager::new(),
            channels.pulse,
            channels.notice,
            VaultConfig::default(),
        );
        let flavite = Flavite::with_session(
            session,
            ApiVaultRef::new(identity.uuid, identity.root),
            VaultConfig::default(),
        )
        .unwrap();
        (flavite, root, temp)
    }

    #[test]
    fn flavite_creation() {
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
            path: PathBuf::from("note.md"),
        };

        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("test content"));
    }

    #[test]
    fn revisioned_buffer_view_reads_session_owned_buffer_and_rejects_old_revision() {
        use flavite_buffer::manager::BufferManager;
        use flavite_fs::{Fs, MemFs};
        use flavite_index::IndexActor;
        use flavite_vault::actor::VaultActorBuilder;
        use flavite_vault::channels::ChannelSet;
        use flavite_vault::session::Session;
        use flavite_vault::types::VaultIdentity;

        let fs = MemFs::new();
        let root = PathBuf::from("/test/buffer-view-vault");
        fs.create_dir_all(&root).unwrap();
        fs.write_new(&root.join("note.md"), b"hello").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("index.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let identity = VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![1, 2, 3]);
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs) as Box<dyn Fs + Send + Sync>)
            .with_identity(identity.clone())
            .with_index(IndexActor::new(&db, 1).unwrap())
            .build()
            .unwrap()
            .start()
            .unwrap();
        let channels = ChannelSet::new();
        let session = Session::new(
            identity.clone(),
            actor,
            BufferManager::new(),
            channels.pulse,
            channels.notice,
            VaultConfig::default(),
        );
        let flavite = Flavite::with_session(
            session,
            ApiVaultRef::new(identity.uuid, identity.root),
            VaultConfig::default(),
        )
        .unwrap();
        let opened = flavite.session.execute(SessionCommand::OpenNote {
            path: root.join("note.md"),
        });
        let buffer_id = match opened {
            SessionResult::Success(crate::session::CommandResult::NoteOpened {
                buffer_id, ..
            }) => buffer_id,
            other => panic!("unexpected open result: {other:?}"),
        };
        let initial = flavite.get_buffer_view(buffer_id, None).unwrap();
        assert_eq!(initial.text, "hello");
        assert_eq!(initial.path, root.join("note.md"));
        assert_eq!(initial.state, "Clean");

        flavite.session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision: Revision::new(initial.revision),
            ops: vec![flavite_buffer::undo::EditOp::new(
                flavite_syntax::text::Span::new(
                    flavite_syntax::text::TextByte(5),
                    flavite_syntax::text::TextByte(5),
                ),
                "!".into(),
            )],
        });
        assert!(matches!(
            flavite.get_buffer_view(buffer_id, Some(Revision::new(initial.revision))),
            Err(FlaviteError::StaleBufferView { .. })
        ));
        let current = flavite.get_buffer_view(buffer_id, None).unwrap();
        assert_eq!(current.text, "hello!");
        assert_eq!(current.state, "Dirty");
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

    #[test]
    fn query_facade_searches_notes_indexed_during_vault_open() {
        let (flavite, _root, _temp) = test_flavite();
        let page = flavite
            .query_with_limit(&crate::Query::Text("hello".into()), 10)
            .unwrap();
        assert_eq!(page.notes.len(), 1);
        assert_eq!(page.notes[0].path, "note.md");
        assert_eq!(page.generation, 1);
        assert!(!page.has_more);
    }

    #[test]
    fn typed_dispatch_covers_validation_read_only_and_unknown_commands() {
        use crate::command::Invocation;
        use crate::types::Origin;

        let (mut flavite, root, _temp) = test_flavite();
        let rejected = flavite.execute(Invocation::new(
            1,
            "write:save_note",
            serde_json::Value::Null,
            Origin::Gui,
        ));
        assert!(matches!(
            rejected,
            ExecutionResult::Rejected(RejectedResult {
                reason: RejectionReason::Unavailable,
                request_id: Some(1),
                ..
            })
        ));
        flavite.set_read_only(true);
        let rejected = flavite.execute(Invocation::new(
            2,
            "save_note",
            serde_json::to_value(root.join("note.md")).unwrap(),
            Origin::Gui,
        ));
        assert!(matches!(
            rejected,
            ExecutionResult::Rejected(RejectedResult {
                reason: RejectionReason::ReadOnly,
                ..
            })
        ));
        flavite.set_read_only(false);

        let invalid = flavite.execute(Invocation::new(
            3,
            "open_note",
            serde_json::json!({"bad": true}),
            Origin::Cli,
        ));
        assert!(matches!(
            invalid,
            ExecutionResult::Rejected(RejectedResult {
                reason: RejectionReason::InvalidArgs,
                request_id: Some(3),
                ..
            })
        ));

        let unknown = flavite.execute(Invocation::new(
            4,
            "does_not_exist",
            serde_json::Value::Null,
            Origin::Plugin,
        ));
        assert!(matches!(
            unknown,
            ExecutionResult::Rejected(RejectedResult {
                reason: RejectionReason::Unavailable,
                request_id: Some(4),
                ..
            })
        ));

        let opened = flavite.execute(Invocation::new(
            5,
            "open_note",
            serde_json::to_value(root.join("note.md")).unwrap(),
            Origin::Gui,
        ));
        let data = opened.into_success().unwrap().data;
        let buffer_id = data["buffer_id"].as_u64().unwrap();
        let closed = flavite.execute(Invocation::new(
            6,
            "close_note",
            serde_json::to_value(root.join("note.md")).unwrap(),
            Origin::Gui,
        ));
        assert!(closed.is_success());
        assert!(matches!(
            flavite.get_buffer_view(BufferId::from(buffer_id), None),
            Err(FlaviteError::BufferNotFound(_))
        ));
    }

    #[test]
    fn typed_dispatch_applies_edits_and_supports_undo_redo_and_reconcile() {
        use crate::command::Invocation;
        use crate::types::Origin;

        let (flavite, root, _temp) = test_flavite();
        let opened = flavite.execute(Invocation::new(
            10,
            "open_note",
            serde_json::to_value(root.join("note.md")).unwrap(),
            Origin::Gui,
        ));
        let opened = opened.into_success().unwrap().data;
        let id = opened["buffer_id"].as_u64().unwrap();
        let revision = opened["revision"].as_u64().unwrap();

        let applied = flavite.execute(Invocation::new(
            11,
            "apply_edits",
            serde_json::json!({
                "buffer_id": id,
                "revision": revision,
                "ops": [{"range": [5, 5], "replacement": "!"}]
            }),
            Origin::Gui,
        ));
        let applied = applied.into_success().unwrap().data;
        assert_eq!(applied["revision"].as_u64().unwrap(), revision + 1);

        let undone = flavite.execute(Invocation::new(
            12,
            "undo",
            serde_json::json!({ "buffer_id": id, "revision": applied["revision"] }),
            Origin::Gui,
        ));
        let undone = undone.into_success().expect("undo succeeds").data;
        assert_eq!(
            flavite
                .get_buffer_view(BufferId::from(id), None)
                .unwrap()
                .text,
            "hello"
        );

        let redone = flavite.execute(Invocation::new(
            13,
            "redo",
            serde_json::json!({ "buffer_id": id, "revision": undone["revision"] }),
            Origin::Gui,
        ));
        assert!(redone.is_success(), "redo failed: {redone:?}");
        assert_eq!(
            flavite
                .get_buffer_view(BufferId::from(id), None)
                .unwrap()
                .text,
            "hello!"
        );

        let reconciled = flavite.execute(Invocation::new(
            14,
            "reconcile",
            serde_json::json!({"scope": "full", "mode": "verify"}),
            Origin::Cli,
        ));
        assert!(matches!(
            reconciled,
            ExecutionResult::Pending { request_id: 14 }
        ));
        let terminal = flavite.wait_for_operation(14);
        assert!(
            matches!(terminal, ExecutionResult::Local(LocalResult::Success(LocalSuccess { request_id: 14, command_id, .. })) if command_id == "reconcile")
        );
    }

    #[test]
    fn session_result_conversion_serializes_every_outcome_shape() {
        use crate::session::{CommandResult, SessionResult};
        use flavite_buffer::revision::{BufferId, Revision};
        use flavite_vault::types::{Epoch, Generation, ReconcileSummary};

        let (flavite, _root, _temp) = test_flavite();
        let buffer = BufferId::from(17);
        let revision = Revision::new(3);
        let successes = [
            CommandResult::NoteOpened {
                buffer_id: buffer,
                revision,
            },
            CommandResult::NoteClosed,
            CommandResult::EditsApplied {
                buffer_id: buffer,
                new_revision: revision,
            },
            CommandResult::NoteSaved {
                buffer_id: buffer,
                saved_revision: revision,
                path: PathBuf::from("a.md"),
                hash: vec![1, 2],
                epoch: Epoch::new(4),
                generation: Generation::new(5),
            },
            CommandResult::ReconcileComplete {
                summary: ReconcileSummary::new(1, 2, 3),
            },
            CommandResult::UndoPerformed {
                buffer_id: buffer,
                new_revision: revision,
            },
            CommandResult::RedoPerformed {
                buffer_id: buffer,
                new_revision: revision,
            },
            CommandResult::ConflictResolved,
            CommandResult::RecoveryRestored {
                buffer_id: buffer,
                revision,
            },
        ];
        for result in successes {
            assert!(
                flavite
                    .convert_session_result(SessionResult::Success(result), 20)
                    .is_success()
            );
        }

        assert!(matches!(
            flavite.convert_session_result(SessionResult::Pending { request_id: 21 }, 20),
            ExecutionResult::Pending { request_id: 20 }
        ));
        assert!(
            matches!(flavite.convert_session_result(SessionResult::Error { message: "failure".into() }, 20), ExecutionResult::Local(LocalResult::Error(LocalError { code, .. })) if code == "session_error")
        );
        assert!(
            matches!(flavite.convert_session_result(SessionResult::Conflict { path: PathBuf::from("a.md") }, 20), ExecutionResult::Local(LocalResult::Error(LocalError { code, .. })) if code == "conflict")
        );
        assert!(
            matches!(flavite.convert_session_result(SessionResult::ActionRequired { code: "confirm", data: Some("details".into()) }, 20), ExecutionResult::ActionRequired { request_id: 20, code, data: Some(serde_json::Value::String(message)) } if code == "confirm" && message == "details")
        );
        assert!(
            matches!(flavite.convert_session_result(SessionResult::ActionRequired { code: "confirm", data: None }, 20), ExecutionResult::ActionRequired { request_id: 20, code, data: None } if code == "confirm")
        );
        assert!(matches!(
            flavite.convert_session_result(
                SessionResult::Superseded {
                    request_id: 22,
                    by_request: 23
                },
                20
            ),
            ExecutionResult::Superseded {
                request_id: 20,
                by_request: 23
            }
        ));
        let merge = SessionResult::MergeReady {
            path: PathBuf::from("a.md"),
            buffer_id: buffer,
            revision,
            disk_hash: [9; 32],
            result: flavite_vault::merge::MergeResult {
                merged: None,
                conflicts: vec![flavite_vault::merge::MergeConflict {
                    base_start_line: 1,
                    base_end_line: 2,
                    mine: vec!["mine".into()],
                    theirs: vec!["theirs".into()],
                }],
            },
        };
        let converted = flavite
            .convert_session_result(merge, 24)
            .into_success()
            .unwrap();
        assert_eq!(converted.data["conflicts"][0]["mine"][0], "mine");
        assert_eq!(converted.data["disk_hash"].as_str().unwrap().len(), 64);
    }
}
