//! Flavite facade — owns Session and command registry (§4.5, D16).
//!
//! This module provides the main public API facade that owns:
//! - The headless Session (from flavite-vault)
//! - The command registry
//! - Revisioned read views
//!
//! All external consumers (GUI, CLI, future plugins) interact with the core
//! system through this facade.

use crate::command::{
    ArgSchema, CommandDispatchError, CommandHandler, CommandRegistry, CommandSpec, Invocation,
};
use crate::result::{
    ExecutionResult, LocalError, LocalResult, LocalSuccess, RejectedResult, RejectionReason,
};
use crate::types::{ApiVaultRef, RequestId};
use flavite_buffer::revision::{BufferId, Revision};
use flavite_vault::open::Vault;
use flavite_vault::session::{Session, SessionCommand, SessionError, SessionResult};
use flavite_vault::sync::{MutexExt, WAIT_TICK, wait_result_recovering};
use flavite_vault::types::{Epoch, Generation, VaultConfig, VaultIdentity};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use thiserror::Error;

/// Re-export vault types for API consumers.
pub use flavite_vault::types::{ReconcileMode, ReconcileScope};

/// RFC-0004 §6.7 (NFR-10): the host-turn budget guard. Every [`Flavite::execute`]
/// turn is measured in debug builds; a turn over [`TURN_BUDGET`] emits a
/// `tracing::warn` naming the command, rate-limited to one per second.
/// Release builds compile the measurement away entirely — the overhead
/// policy is zero beyond the two compares the optimizer folds into the
/// call site's absence.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub mod turn_budget {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    /// The turn budget (§6.7): a host turn over this wall emits a
    /// rate-limited warn.
    pub const TURN_BUDGET: Duration = Duration::from_millis(2);

    /// Rate-limit window: at most one budget warn per second.
    const RATE_LIMIT: Duration = Duration::from_secs(1);

    /// Millis-since-epoch of the last emitted (or decided) warn, on a
    /// process-start anchor so the state is a single lock-free atomic.
    /// `0` means "never warned" (millis-since-epoch 0 is only the first
    /// millisecond of process life; a warn then is indistinguishable from
    /// never — harmless by one rate window).
    static LAST_WARN_MILLIS: AtomicU64 = AtomicU64::new(0);
    static EPOCH: OnceLock<Instant> = OnceLock::new();

    fn millis_since_epoch() -> u64 {
        let epoch = EPOCH.get_or_init(Instant::now);
        u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn millis_since_last_warn() -> u64 {
        let last = LAST_WARN_MILLIS.load(Ordering::Relaxed);
        if last == 0 {
            // Never warned: the elapsed window is unbounded.
            u64::MAX
        } else {
            // Stored values are `epoch + 1`, so the "never warned" sentinel
            // is unambiguous even in the first millisecond of process life.
            millis_since_epoch().saturating_sub(last.saturating_sub(1))
        }
    }

    /// The rate-limited decision, exposed as the test hook (§6.7: warn
    /// emission asserted through the decision, not a subscriber): `true`
    /// when a turn of `elapsed` exceeds the budget AND the rate window has
    /// elapsed — and records the window when it does.
    #[doc(hidden)]
    pub fn warn_decision(elapsed: Duration) -> bool {
        if elapsed <= TURN_BUDGET {
            return false;
        }
        let since_last = millis_since_last_warn();
        if since_last < RATE_LIMIT.as_millis().min(u128::from(u64::MAX)) as u64 {
            return false;
        }
        LAST_WARN_MILLIS.store(millis_since_epoch().saturating_add(1), Ordering::Relaxed);
        true
    }

    /// Test reset for the rate-window state.
    #[doc(hidden)]
    pub fn reset_for_test() {
        LAST_WARN_MILLIS.store(0, Ordering::Relaxed);
    }

    /// Measure one host turn (debug builds only).
    pub fn observe(command_id: &'static str, started: Instant) {
        let elapsed = started.elapsed();
        if warn_decision(elapsed) {
            tracing::warn!(
                target: "flavite_api::turn_budget",
                command_id,
                elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                "host turn exceeded budget"
            );
        }
    }
}

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
struct RenameArgs {
    from: PathBuf,
    to: PathBuf,
    strategy: Option<RenameStrategyArg>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RenameStrategyArg {
    Rewrite,
    Alias,
    #[serde(rename = "none")]
    NoRewrite,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveArgs {
    path: PathBuf,
    resolution: String,
    new_path: Option<PathBuf>,
}

pub(crate) fn decode<T: serde::de::DeserializeOwned>(invocation: &Invocation) -> Result<T, String> {
    serde_json::from_value(invocation.args.clone()).map_err(|error| error.to_string())
}

/// Arguments for the note-open commands: a bare vault-relative path, or an
/// object form (`{"path": …}`) so hosts can attach navigation metadata
/// (target, anchor, by_id, create_if_unresolved) alongside the path.
/// `policy` distinguishes opening existing notes from creating missing
/// ones; the default keeps the established create-if-missing behavior.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OpenArgs {
    Path(PathBuf),
    Object {
        path: PathBuf,
        #[serde(default)]
        policy: OpenPolicyArg,
    },
}

/// Wire form of [`flavite_vault::session::OpenPolicy`].
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OpenPolicyArg {
    #[default]
    CreateIfMissing,
    ExistingOnly,
}

impl From<OpenPolicyArg> for flavite_vault::session::OpenPolicy {
    fn from(policy: OpenPolicyArg) -> Self {
        match policy {
            OpenPolicyArg::CreateIfMissing => Self::CreateIfMissing,
            OpenPolicyArg::ExistingOnly => Self::ExistingOnly,
        }
    }
}

fn handle_property(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let args: crate::PropertyEditArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::EditStructure {
        buffer_id: args.buffer_id.into(),
        revision: Revision::new(args.revision),
        edit: flavite_vault::properties::StructureEdit::Property {
            key: args.key,
            value: args.value,
        },
    }))
}
fn handle_task(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let args: crate::TaskEditArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::EditStructure {
        buffer_id: args.buffer_id.into(),
        revision: Revision::new(args.revision),
        edit: flavite_vault::properties::StructureEdit::Task {
            start: args.start,
            checked: args.checked,
        },
    }))
}
fn handle_open(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let (path, policy) = match decode::<OpenArgs>(invocation)? {
        OpenArgs::Path(path) => (path, OpenPolicyArg::default()),
        OpenArgs::Object { path, policy } => (path, policy),
    };
    Ok(session.execute(SessionCommand::OpenNote {
        path,
        policy: policy.into(),
    }))
}
fn handle_close(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    crate::documents::handle_close(session, invocation)
}
fn handle_save(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let path = decode(invocation)?;
    Ok(session.execute(SessionCommand::SaveNote { path }))
}
fn handle_create(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let path = decode(invocation)?;
    Ok(session.execute(SessionCommand::CreateNote { path }))
}
fn handle_link_prepare(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: crate::links::LinkPrepareArgs = decode(invocation)?;
    let policy = match args.stable_link_policy.as_str() {
        "disabled" => flavite_vault::link::StableLinkPolicy::Disabled,
        "prefer_stable" => flavite_vault::link::StableLinkPolicy::PreferStable,
        other => {
            return Err(format!(
                "stable_link_policy must be `disabled` or `prefer_stable`, got {other:?}"
            ));
        }
    };
    let request = flavite_vault::link::LinkPrepareRequest {
        intent: flavite_vault::link::LinkIntent {
            client: flavite_vault::link::ClientToken(args.intent.client),
            sequence: args.intent.sequence,
        },
        buffer_id: args.source.buffer_id,
        revision: args.source.revision,
        caret: args.source.caret,
        target_path: args.target_path,
        expected_target: Some(args.expected_target),
        stable_link_policy: policy,
        removal: None,
    };
    Ok(session.execute(SessionCommand::LinkPrepare {
        request,
        request_id: invocation.request_id,
    }))
}

fn handle_link_prepare_remove(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: crate::links::LinkRemovalArgs = decode(invocation)?;
    if args.expected_source.len() > 8192 {
        return Err("occurrence exceeds 8 KiB".into());
    }
    Ok(session.execute(SessionCommand::LinkPrepare {
        request_id: invocation.request_id,
        request: flavite_vault::link::LinkPrepareRequest {
            intent: flavite_vault::link::LinkIntent {
                client: flavite_vault::link::ClientToken(args.intent.client),
                sequence: args.intent.sequence,
            },
            buffer_id: args.source.buffer_id,
            revision: args.source.revision,
            caret: args.start,
            target_path: args.target_path,
            expected_target: None,
            stable_link_policy: flavite_vault::link::StableLinkPolicy::Disabled,
            removal: Some(flavite_vault::link::RemovalRequest {
                start: args.start,
                end: args.end,
                expected_source: args.expected_source,
                epoch: args.epoch,
                target_entity_id: args.target_entity_id,
                target_is_attachment: args.target_is_attachment,
                target_is_unresolved: args.target_is_unresolved,
            }),
        },
    }))
}
fn handle_link_remove(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let args: crate::links::LinkInsertArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::LinkRemove {
        intent: flavite_vault::link::LinkIntent {
            client: flavite_vault::link::ClientToken(args.intent.client),
            sequence: args.intent.sequence,
        },
        plan_id: args.plan_id,
    }))
}

fn handle_link_insert(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    // The raw edit shape is rejected explicitly: no ambiguous union with
    // the old link.insert payload (RFC §9.1).
    if invocation.args.get("ops").is_some() {
        return Err(Flavite::raw_link_insert_rejection().to_owned());
    }
    let args: crate::links::LinkInsertArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::LinkInsert {
        intent: flavite_vault::link::LinkIntent {
            client: flavite_vault::link::ClientToken(args.intent.client),
            sequence: args.intent.sequence,
        },
        plan_id: args.plan_id,
    }))
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
fn handle_rename(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let args: RenameArgs = decode(invocation)?;
    let strategy = args.strategy.map(|strategy| match strategy {
        RenameStrategyArg::Rewrite => flavite_vault::types::RenameStrategy::Rewrite,
        RenameStrategyArg::Alias => flavite_vault::types::RenameStrategy::Alias,
        RenameStrategyArg::NoRewrite => flavite_vault::types::RenameStrategy::None,
    });
    Ok(session.execute(SessionCommand::RenameNote {
        from: args.from,
        to: args.to,
        strategy,
    }))
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
/// Wire form of the local-history list request (W10).
#[derive(Debug, Deserialize)]
struct HistoryArgs {
    path: PathBuf,
    #[serde(default = "default_history_limit")]
    limit: usize,
}

fn default_history_limit() -> usize {
    100
}

fn handle_history(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let args: HistoryArgs = decode(invocation)?;
    let entries = session.note_history(&args.path, args.limit)?;
    Ok(SessionResult::Success(
        crate::session::CommandResult::NoteHistory {
            path: args.path,
            entries,
        },
    ))
}

/// Wire form of the local-history restore request (W10).
#[derive(Debug, Deserialize)]
struct RestoreHistoryArgs {
    path: PathBuf,
    buffer_id: u64,
    revision: u64,
    hash: String,
}

fn handle_restore_history(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: RestoreHistoryArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::RestoreHistory {
        path: args.path,
        buffer_id: BufferId::from(args.buffer_id),
        revision: Revision::new(args.revision),
        hash: args.hash,
    }))
}

type HandlerRegistration = (&'static str, &'static str, ArgSchema, CommandHandler);
/// Outstanding-notice ledger capacity (NFR-18). The host's per-poll
/// credit budget is derived from this single constant.
pub const MAX_NOTICE_ACKS: usize = 256;

/// Stable token for one notice presentation acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NoticeId(pub u64);

/// Event delivered to a host. Pulses have no token; lossless notices retain
/// theirs until the host confirms that they were presented.
#[derive(Debug, Clone, PartialEq)]
pub struct EventDelivery {
    /// Stable acknowledgement token for notices, absent for pulses.
    pub notice_id: Option<NoticeId>,
    /// Typed event payload.
    pub event: crate::Event,
}

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
/// revisioned read views, and typed command dispatch.
#[derive(Debug)]
pub struct Flavite {
    /// The underlying Session.
    pub(crate) session: Arc<Session>,
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
    /// Wake edge for [`Flavite::wait_for_operation`], paired with
    /// `terminal_backlog` — NOT with the Session's completion queue: the
    /// api tier's mutex is the backlog, so condvar duplication across the
    /// two tiers is deliberate correctness, not drift (Q5/FR-9: a condvar
    /// shared across different mutexes would be a bug).
    backlog_cv: Condvar,
    /// Notices delivered to consumers, retained until presentation ack.
    notice_acks: Mutex<
        std::collections::BTreeMap<NoticeId, (crate::Event, flavite_vault::channels::Notice)>,
    >,
    next_notice_id: AtomicU64,
    /// The API vault reference.
    vault_ref: ApiVaultRef,
    /// Whether the vault is read-only.
    pub(crate) read_only: bool,
    /// Keeps root ownership and actor resources alive for `open_root` callers.
    owned_vault: Option<Vault>,
    /// Bounded asynchronous context query service (W04).
    pub(crate) queries: crate::query_service::QueryService,
    /// Device-local user state (W06); `None` without an OS data directory.
    pub(crate) user_state: Option<flavite_vault::user_state::UserStateStore>,
    /// Vault-relative user state (graph pins; W07/TD-11) behind the `Fs`.
    pub(crate) vault_user_state: flavite_vault::user_state::VaultUserStateStore,
    /// One graph base service per vault (W07/TD-12): the only graph-path
    /// owner of an index read handle.
    pub(crate) graph: Arc<crate::graph_service::GraphService>,
    /// Monotonic sequence for the reserved catalog context (RFC-0004 D9):
    /// every `submit_catalog` ticket supersedes the previous pending one.
    pub(crate) catalog_sequence: AtomicU64,
}

/// Build the per-vault attached services shared by every constructor.
///
/// RFC-0005 S1: `index_watch` is the watch half of the writer-side
/// generation broadcast when the vault was opened with
/// [`flavite_vault::Vault::open_with_generation_signal`] (the
/// [`Flavite::open_root`] production path). The graph service stores it for
/// its S5 event loop; until then it only keeps the channel open.
fn attach_services(
    uuid: uuid::Uuid,
    root: &std::path::Path,
    session: &Arc<Session>,
    index_watch: Option<Arc<flavite_index::GenerationWatch>>,
) -> (
    flavite_vault::user_state::VaultUserStateStore,
    Arc<crate::graph_service::GraphService>,
) {
    let vault_user_state = flavite_vault::user_state::VaultUserStateStore::new(
        Box::new(flavite_fs::OsFs::new()),
        root,
    );
    let graph = crate::graph_service::GraphService::start_with_index_watch(
        crate::doctor::cache_db_path(uuid),
        Some(Arc::clone(session)),
        index_watch,
    );
    (vault_user_state, graph)
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
        let vault_uuid = vault_ref.uuid.to_string();

        let session = Arc::new(session);
        let (vault_user_state, graph) =
            attach_services(vault_ref.uuid, &identity.root, &session, None);
        let queries = crate::query_service::QueryService::new(
            Arc::clone(&session),
            uuid::Uuid::now_v7().as_u128(),
            Arc::clone(&graph),
        );
        let flavite = Self {
            session,
            command_registry: CommandRegistry::new(),
            ingress: Mutex::new(()),
            operation_ids: Mutex::new(HashMap::new()),
            pending_requests: Mutex::new(HashSet::new()),
            terminal_backlog: Mutex::new(HashMap::new()),
            backlog_cv: Condvar::new(),
            notice_acks: Mutex::new(std::collections::BTreeMap::new()),
            next_notice_id: AtomicU64::new(1),
            catalog_sequence: AtomicU64::new(1),
            vault_ref,
            read_only: false,
            owned_vault: None,
            queries,
            user_state: flavite_vault::user_state::UserStateStore::open(
                Box::new(flavite_fs::OsFs::new()),
                &vault_uuid,
            )
            .ok(),
            vault_user_state,
            graph,
        };

        // Register built-in commands
        flavite.register_builtin_commands()?;

        Ok(flavite)
    }

    /// Build the device-local user-state store for a vault, `None` when the
    /// OS data directory is unavailable (the host reports that honestly).
    /// Create a new Flavite with a custom session builder.
    ///
    /// This is useful for testing or when you need to customize the session.
    pub fn with_session(
        session: Session,
        vault_ref: ApiVaultRef,
        _config: VaultConfig,
    ) -> Result<Self, FlaviteError> {
        let vault_uuid = vault_ref.uuid.to_string();
        let session = Arc::new(session);
        let (vault_user_state, graph) =
            attach_services(vault_ref.uuid, &vault_ref.root, &session, None);
        let queries = crate::query_service::QueryService::new(
            Arc::clone(&session),
            uuid::Uuid::now_v7().as_u128(),
            Arc::clone(&graph),
        );
        let flavite = Self {
            session,
            command_registry: CommandRegistry::new(),
            ingress: Mutex::new(()),
            operation_ids: Mutex::new(HashMap::new()),
            pending_requests: Mutex::new(HashSet::new()),
            terminal_backlog: Mutex::new(HashMap::new()),
            backlog_cv: Condvar::new(),
            notice_acks: Mutex::new(std::collections::BTreeMap::new()),
            next_notice_id: AtomicU64::new(1),
            catalog_sequence: AtomicU64::new(1),
            vault_ref,
            read_only: false,
            owned_vault: None,
            queries,
            user_state: flavite_vault::user_state::UserStateStore::open(
                Box::new(flavite_fs::OsFs::new()),
                &vault_uuid,
            )
            .ok(),
            vault_user_state,
            graph,
        };

        flavite.register_builtin_commands()?;

        Ok(flavite)
    }

    /// Create a new Flavite with a shared Session reference.
    ///
    /// This is useful when the Session is owned by another component
    /// (e.g., the Vault in the CLI integration). The graph service starts
    /// without an index watch here: hosts that need the event spine use
    /// [`Flavite::open_root`], which wires the vault's generation broadcast
    /// into the service.
    pub fn with_vault(
        identity: VaultIdentity,
        session: Arc<Session>,
        config: VaultConfig,
    ) -> Result<Self, FlaviteError> {
        Self::with_vault_inner(identity, session, config, None)
    }

    fn with_vault_inner(
        identity: VaultIdentity,
        session: Arc<Session>,
        _config: VaultConfig,
        index_watch: Option<Arc<flavite_index::GenerationWatch>>,
    ) -> Result<Self, FlaviteError> {
        let vault_uuid = identity.uuid.to_string();
        let vault_ref = ApiVaultRef::new(identity.uuid, identity.root.clone());
        let (vault_user_state, graph) =
            attach_services(vault_ref.uuid, &identity.root, &session, index_watch);
        let queries = crate::query_service::QueryService::new(
            Arc::clone(&session),
            uuid::Uuid::now_v7().as_u128(),
            Arc::clone(&graph),
        );
        let flavite = Self {
            session,
            command_registry: CommandRegistry::new(),
            ingress: Mutex::new(()),
            operation_ids: Mutex::new(HashMap::new()),
            pending_requests: Mutex::new(HashSet::new()),
            terminal_backlog: Mutex::new(HashMap::new()),
            backlog_cv: Condvar::new(),
            queries,
            user_state: flavite_vault::user_state::UserStateStore::open(
                Box::new(flavite_fs::OsFs::new()),
                &vault_uuid,
            )
            .ok(),
            vault_user_state,
            graph,
            notice_acks: Mutex::new(std::collections::BTreeMap::new()),
            next_notice_id: AtomicU64::new(1),
            catalog_sequence: AtomicU64::new(1),
            vault_ref,
            read_only: false,
            owned_vault: None,
        };

        flavite.register_builtin_commands()?;

        Ok(flavite)
    }

    /// Open a vault and construct the public facade while retaining ownership
    /// guards for the lifetime of the facade.
    ///
    /// This is the production wiring for RFC-0005's event spine: the
    /// generation broadcast is constructed here (the only place owning both
    /// sides), the sender is handed into the vault's index actor — every
    /// committed structural generation publishes from the apply spine's
    /// commit sites (FR-1) — and the watch is held by the graph service's
    /// constructor seam.
    pub fn open_root(root: PathBuf, config: VaultConfig) -> Result<Self, FlaviteError> {
        Self::open_root_with_data_dir_and_signal(root, config, None)
    }

    /// Open a vault root with an explicit OS data directory (S8/M3); see
    /// [`Flavite::open_root`] and `flavite_vault::open::Vault::open_with_data_dir`.
    /// Freshness rides the graph service's sweep (no generation broadcast).
    pub fn open_root_with_data_dir(
        root: PathBuf,
        config: VaultConfig,
        data_dir: Option<PathBuf>,
    ) -> Result<Self, FlaviteError> {
        Self::open_root_with_data_dir_and_signal(root, config, data_dir)
    }

    /// Open a vault root with an explicit OS data directory *and* the
    /// writer-side generation broadcast (RFC-0005 S1 + S8/M3 union). The
    /// broadcast is constructed here (the only place owning both sides):
    /// the sender is handed into the vault's index actor — every committed
    /// structural generation publishes from the apply spine's commit sites
    /// (FR-1) — and the watch is held by the graph service's constructor
    /// seam.
    pub fn open_root_with_data_dir_and_signal(
        root: PathBuf,
        config: VaultConfig,
        data_dir: Option<PathBuf>,
    ) -> Result<Self, FlaviteError> {
        let (generation_signal, generation_watch) = flavite_index::GenerationSignal::new();
        let vault = Vault::open_with_data_dir_and_generation_signal(
            root,
            Box::new(flavite_fs::OsFs::new()),
            config.clone(),
            data_dir,
            generation_signal,
        )
        .map_err(|error| FlaviteError::VaultOpenError(error.to_string()))?;
        let mut flavite = Self::with_vault_inner(
            vault.identity().clone(),
            vault.session_arc(),
            config,
            Some(Arc::new(generation_watch)),
        )?;
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
        let registrations: Vec<HandlerRegistration> = vec![
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
                "note.close",
                "Close note",
                ArgSchema::typed::<crate::documents::CloseArgs>(),
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
                "property.set",
                "Set note property",
                ArgSchema::typed::<crate::PropertyEditArgs>(),
                handle_property,
            ),
            (
                "task.toggle",
                "Toggle note task",
                ArgSchema::typed::<crate::TaskEditArgs>(),
                handle_task,
            ),
            (
                "apply_edits",
                "Apply edits",
                ArgSchema::typed::<ApplyEditsArgs>(),
                handle_edits,
            ),
            (
                "link.prepare",
                "Prepare link insertion",
                ArgSchema::typed::<crate::links::LinkPrepareArgs>(),
                handle_link_prepare,
            ),
            (
                "link.prepare_remove",
                "Prepare connection removal",
                ArgSchema::typed::<crate::links::LinkRemovalArgs>(),
                handle_link_prepare_remove,
            ),
            (
                "link.remove",
                "Remove connection occurrence",
                ArgSchema::typed::<crate::links::LinkInsertArgs>(),
                handle_link_remove,
            ),
            (
                "link.insert",
                "Insert link",
                ArgSchema::custom(crate::links::validate_link_insert_args),
                handle_link_insert,
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
                "note.rename",
                "Rename note",
                ArgSchema::typed::<RenameArgs>(),
                handle_rename,
            ),
            (
                "resolve_conflict",
                "Resolve conflict",
                ArgSchema::typed::<ResolveArgs>(),
                handle_resolve,
            ),
            (
                "note.create",
                "Create note",
                ArgSchema::typed::<PathBuf>(),
                handle_create,
            ),
            (
                "note.history",
                "Note history",
                ArgSchema::typed::<HistoryArgs>(),
                handle_history,
            ),
            (
                "note.restore_history",
                "Restore history snapshot",
                ArgSchema::typed::<RestoreHistoryArgs>(),
                handle_restore_history,
            ),
            (
                "recovery.restore",
                "Restore retained recovery snapshot",
                ArgSchema::typed::<crate::recovery::RecoveryNoteArgs>(),
                crate::recovery::handle_restore,
            ),
            (
                "recovery.discard",
                "Discard retained recovery snapshot",
                ArgSchema::typed::<crate::recovery::RecoveryNoteArgs>(),
                crate::recovery::handle_discard,
            ),
            (
                "recovery.flush",
                "Durably retain unsaved changes now",
                ArgSchema::typed::<crate::recovery::FlushRecoveryArgs>(),
                crate::recovery::handle_flush,
            ),
            (
                "note.discard_edits",
                "Discard unsaved edits (restores disk content)",
                ArgSchema::typed::<crate::recovery::RecoveryNoteArgs>(),
                crate::documents::handle_discard_edits,
            ),
            (
                "conflict.apply_merge",
                "Apply a confirmed conflict merge",
                ArgSchema::typed::<crate::documents::ApplyMergeArgs>(),
                crate::documents::handle_apply_merge,
            ),
        ];
        for (id, title, args, handler) in registrations {
            let spec = if matches!(id, "open_note" | "note.open" | "note.history") {
                CommandSpec::new(id, title, args).requires(crate::Capability::ReadNotes)
            } else {
                CommandSpec::new(id, title, args)
                    .write()
                    .requires(crate::Capability::WriteNotes {
                        scope: crate::WriteScope::Vault,
                    })
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

    /// Advance accepted Session work and publish terminal completions. Hosts
    /// call this regularly; notice acknowledgement never gates this pump.
    pub fn pump(&self) {
        self.session.pump();
    }

    /// Whether a canonical note path currently has a shared Session buffer.
    #[must_use]
    pub fn is_note_open(&self, path: &std::path::Path) -> bool {
        self.session.is_open(path)
    }

    /// Poll typed events with notice credits. Pulses are lossy hints and do
    /// not consume credits. At most `min(credits, available capacity)` notices
    /// are removed from the Session; outstanding notices are never evicted or
    /// auto-acknowledged when the host stops presenting them.
    pub fn poll_events(&self, notice_credits: usize) -> Vec<EventDelivery> {
        use flavite_vault::channels::{Notice, Pulse};

        let available = MAX_NOTICE_ACKS.saturating_sub(self.notice_acks.lock_recovering().len());
        let notice_limit = notice_credits.min(available);
        let mut events = Vec::new();
        for event in self.session.drain_events() {
            if let Some(event) = Self::session_event_to_api(&self.vault_ref, event) {
                events.push(EventDelivery {
                    notice_id: None,
                    event,
                });
            }
        }
        for pulse in self.session.drain_pulses() {
            let event = match pulse {
                Pulse::VaultChanged {
                    epoch,
                    generation,
                    summary,
                } => crate::Event::VaultChanged {
                    vault: self.vault_ref.clone(),
                    epoch: epoch.value(),
                    generation: generation.value(),
                    added: summary.added,
                    changed: summary.changed,
                    removed: summary.removed,
                },
                Pulse::ReconcileProgress { done, total } => {
                    crate::Event::ReconcileProgress { done, total }
                }
                Pulse::Invalidate { note } => crate::Event::Invalidate {
                    vault: self.vault_ref.clone(),
                    note_path: note.path.to_string_lossy().into_owned(),
                },
            };
            events.push(EventDelivery {
                notice_id: None,
                event,
            });
        }
        for notice in self.session.drain_notices_limit(notice_limit) {
            let event = match &notice {
                Notice::RenameSuggested {
                    from,
                    to,
                    tier,
                    inbound,
                } => crate::Event::RenameSuggested {
                    vault: self.vault_ref.clone(),
                    from: from.clone(),
                    to: to.clone(),
                    tier: tier.as_str().into(),
                    inbound: *inbound,
                },
                Notice::Conflict { note } => crate::Event::Conflict {
                    vault: self.vault_ref.clone(),
                    note_path: note.path.to_string_lossy().into_owned(),
                },
                Notice::RecoveryOffer { note } => crate::Event::RecoveryOffer {
                    vault: self.vault_ref.clone(),
                    note_path: note.path.to_string_lossy().into_owned(),
                },
                Notice::ActionRequired { code, note } => crate::Event::ActionRequired {
                    vault: self.vault_ref.clone(),
                    code: (*code).into(),
                    note_path: note.as_ref().map(|n| n.path.to_string_lossy().into_owned()),
                },
                Notice::LinksBroken { from, count } => crate::Event::LinksBroken {
                    vault: self.vault_ref.clone(),
                    from: from.clone(),
                    count: *count,
                },
            };
            let id = NoticeId(self.next_notice_id.fetch_add(1, Ordering::Relaxed));
            self.notice_acks
                .lock_recovering()
                .insert(id, (event.clone(), notice));
            events.push(EventDelivery {
                notice_id: Some(id),
                event,
            });
        }
        events
    }

    /// Map a vault `SessionEvent` onto the api-tier event surface. Variants
    /// the vault already delivers through dedicated channels (pulses /
    /// notices) are filtered here; `Panicked` MUST surface so UI/CLI hosts
    /// observe contained session panics without importing vault types
    /// (FR-8). Pure function of `(vault_ref, event)` — testable directly.
    fn session_event_to_api(
        vault: &ApiVaultRef,
        event: flavite_vault::session::SessionEvent,
    ) -> Option<crate::Event> {
        use flavite_vault::session::SessionEvent;
        match event {
            SessionEvent::ExternalChangeDetected { path, deleted } => {
                Some(crate::Event::ExternalChangeDetected {
                    vault: vault.clone(),
                    note_path: path.to_string_lossy().into_owned(),
                    deleted,
                })
            }
            SessionEvent::Panicked {
                message,
                panics_total,
            } => Some(crate::Event::SessionPanicked {
                vault: vault.clone(),
                message,
                panics_total,
            }),
            _ => None,
        }
    }

    /// Acknowledge a notice only after its host presentation is visible.
    /// This never completes a command or advances journal work.
    pub fn acknowledge_notice(&self, id: NoticeId) -> bool {
        let mut pending = self.notice_acks.lock_recovering();
        if let Some((_, notice)) = pending.remove(&id) {
            self.session.acknowledge_notice(&notice);
            return true;
        }
        false
    }

    /// Inject a typed notice as if the core had raised it. Test-only seam
    /// for host-boundary notice-ledger conformance; not part of the API.
    #[doc(hidden)]
    pub fn inject_notice_for_test(&self, code: &'static str) {
        self.session
            .notice_channel()
            .send(flavite_vault::channels::Notice::ActionRequired { code, note: None });
    }

    /// The bounded asynchronous context query service (W04).
    #[must_use]
    pub const fn query_service(&self) -> &crate::query_service::QueryService {
        &self.queries
    }

    /// Current number of notice credits held by the host.
    #[must_use]
    pub fn outstanding_notice_count(&self) -> usize {
        self.notice_acks.lock_recovering().len()
    }

    /// Ask the vault actor to perform its throttled focus verification.
    pub fn focus(&self) -> Result<(), FlaviteError> {
        self.session.focus().map_err(FlaviteError::SessionOperation)
    }

    /// Cumulative drift observations from the vault actor.
    #[must_use]
    pub fn drift_found_total(&self) -> u64 {
        self.session.drift_found_total()
    }

    /// Return a revision-independent snapshot of indexed note titles, aliases,
    /// and unconflicted stable IDs for autocomplete consumers.
    pub fn completion_candidates(&self) -> Result<Vec<crate::CompletionCandidate>, FlaviteError> {
        self.session
            .completion_candidates()
            .map_err(FlaviteError::IndexError)
    }

    /// Resolve an editor link using the vault index's canonical resolver.
    pub fn resolve_target(
        &self,
        target_key: impl Into<String>,
        raw_target: impl Into<String>,
        is_embed: bool,
        source_path_key: Option<String>,
    ) -> Result<flavite_index::resolver::Resolution, FlaviteError> {
        self.session
            .resolve_target(
                target_key.into(),
                raw_target.into(),
                is_embed,
                source_path_key,
            )
            .map_err(FlaviteError::IndexError)
    }

    /// Resolve a note-link spelling through the canonical index resolver.
    /// The facade computes the resolver key so GUI callers never duplicate
    /// syntax normalization policy.
    pub fn resolve_note_target(
        &self,
        raw_target: impl Into<String>,
        source_path_key: Option<String>,
    ) -> Result<flavite_index::resolver::Resolution, FlaviteError> {
        let raw_target = raw_target.into();
        self.resolve_target(
            flavite_syntax::text::fold_key(&raw_target),
            raw_target,
            false,
            source_path_key,
        )
    }

    /// Decode a resolved entity's native path bytes into the vault-relative
    /// path it selects. Pure decode through the vault filesystem identity
    /// codec: no filesystem or database access on the calling thread.
    pub fn decode_note_path(
        &self,
        path_native: Vec<u8>,
    ) -> Result<std::path::PathBuf, FlaviteError> {
        self.session
            .decode_graph_paths(vec![path_native])
            .map_err(FlaviteError::IndexError)?
            .pop()
            .ok_or_else(|| FlaviteError::IndexError("native path decode produced no path".into()))
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
        #[cfg(debug_assertions)]
        let turn_started = std::time::Instant::now();
        // `&'static str`: the id outlives the moved invocation.
        #[cfg(debug_assertions)]
        let command_id = invocation.command_id;
        let result = self.execute_turn(invocation);
        // NFR-10: debug builds measure every host turn; release pays nothing.
        #[cfg(debug_assertions)]
        turn_budget::observe(command_id, turn_started);
        result
    }

    /// The turn body: ingress-serialized validation + dispatch + conversion.
    fn execute_turn(&self, invocation: Invocation) -> ExecutionResult {
        let _turn = self.ingress.lock_recovering();
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
        if let Some(required) = self
            .command_registry
            .get_spec(invocation.command_id)
            .and_then(|spec| spec.required_capability)
            && !invocation.capabilities.iter().any(|granted| {
                granted.covers(&required, self.command_target(&invocation).as_deref())
            })
        {
            return ExecutionResult::Rejected(
                RejectedResult::with_request(
                    RejectionReason::CapabilityDenied,
                    invocation.request_id,
                )
                .with_message(format!("missing capability: {}", required.description())),
            );
        }
        if matches!(
            invocation.command_id,
            "link.prepare" | "link.prepare_remove" | "link.insert" | "link.remove"
        ) && !invocation
            .capabilities
            .iter()
            .any(|cap| cap.covers(&crate::Capability::ReadNotes, None))
        {
            return ExecutionResult::Rejected(
                RejectedResult::with_request(
                    RejectionReason::CapabilityDenied,
                    invocation.request_id,
                )
                .with_message("Reading the connection target requires ReadNotes"),
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
        // The write policy is a property of the opened vault, never of the
        // caller's preconditions (TD-17): a read-only vault rejects every
        // write-requiring command regardless of requested exceptions.
        if self.read_only && requires_write {
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

    fn command_target(&self, invocation: &Invocation) -> Option<PathBuf> {
        let actual_source = match invocation.command_id {
            "link.prepare" | "link.prepare_remove" => invocation
                .args
                .get("source")
                .and_then(|source| source.get("buffer_id"))
                .and_then(serde_json::Value::as_u64)
                .map(BufferId::from),
            "property.set" | "task.toggle" | "apply_edits" | "undo" | "redo" => invocation
                .args
                .get("buffer_id")
                .and_then(serde_json::Value::as_u64)
                .map(BufferId::from),
            "link.insert" | "link.remove" => {
                serde_json::from_value::<crate::links::LinkInsertArgs>(invocation.args.clone())
                    .ok()
                    .and_then(|args| {
                        self.session.link_plan_source(
                            flavite_vault::link::LinkIntent {
                                client: flavite_vault::link::ClientToken(args.intent.client),
                                sequence: args.intent.sequence,
                            },
                            args.plan_id,
                        )
                    })
            }
            _ => None,
        };
        if matches!(
            invocation.command_id,
            "link.prepare"
                | "link.prepare_remove"
                | "link.insert"
                | "link.remove"
                | "property.set"
                | "task.toggle"
                | "apply_edits"
                | "undo"
                | "redo"
        ) {
            return actual_source.and_then(|id| self.session.buffer_path(id));
        }

        let path = invocation.args.as_str().map(PathBuf::from).or_else(|| {
            invocation
                .args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
        });
        path.or_else(|| {
            invocation.preconditions.buffer_id.and_then(|buffer| {
                self.get_buffer_view(
                    BufferId::from(buffer),
                    invocation
                        .preconditions
                        .expected_revision
                        .map(Revision::new),
                )
                .ok()
                .map(|view| view.path)
            })
        })
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
            SessionResult::Success(crate::session::CommandResult::NoteCreated { .. }) => {
                "note.create"
            }
            SessionResult::Success(crate::session::CommandResult::NoteHistory { .. }) => {
                "note.history"
            }
            SessionResult::Success(crate::session::CommandResult::HistoryRestored { .. }) => {
                "note.restore_history"
            }
            SessionResult::Success(crate::session::CommandResult::NoteClosed) => "close_note",
            SessionResult::Success(crate::session::CommandResult::EditsApplied { .. }) => {
                "apply_edits"
            }
            SessionResult::Success(crate::session::CommandResult::LinkPrepared { .. }) => {
                "link.prepare"
            }
            SessionResult::Success(crate::session::CommandResult::LinkInserted { .. }) => {
                "link.insert"
            }
            SessionResult::LinkRejected { .. } => "link.insert",
            SessionResult::Success(crate::session::CommandResult::NoteSaved { .. }) => "save_note",
            SessionResult::Success(crate::session::CommandResult::ReconcileComplete { .. }) => {
                "reconcile"
            }
            SessionResult::Success(crate::session::CommandResult::NoteRenamed { .. }) => {
                "rename_note"
            }
            SessionResult::Success(crate::session::CommandResult::UndoPerformed { .. }) => "undo",
            SessionResult::Success(crate::session::CommandResult::RedoPerformed { .. }) => "redo",
            SessionResult::Success(crate::session::CommandResult::ConflictResolved) => {
                "resolve_conflict"
            }
            SessionResult::Success(crate::session::CommandResult::RecoveryRestored { .. }) => {
                "recovery.restore"
            }
            SessionResult::Success(crate::session::CommandResult::RecoveryFlushed { .. }) => {
                "recovery.flush"
            }
            SessionResult::Success(crate::session::CommandResult::EditsDiscarded { .. }) => {
                "note.discard_edits"
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
                    crate::session::CommandResult::LinkPrepared { prepared } => {
                        serde_json::json!({
                            "plan_id": prepared.plan_id.to_string(),
                            "expires_in_ms": prepared.expires_in_ms,
                            "serialized_link": prepared.serialized_link,
                            "target_display": prepared.target_display,
                            "target_path": prepared.target_path,
                            "validated_epoch": prepared.validated_epoch,
                            "validated_generation": prepared.validated_generation,
                            "source": {
                                "buffer_id": prepared.source.buffer_id,
                                "revision": prepared.source.revision,
                                "caret": prepared.source.caret,
                            },
                            "requires_confirmation": prepared.requires_confirmation,
                        })
                    }
                    crate::session::CommandResult::LinkInserted {
                        buffer_id,
                        new_revision,
                    } => {
                        serde_json::json!({
                            "buffer_id": buffer_id.as_u64(),
                            "revision": new_revision.as_u64(),
                        })
                    }
                    crate::session::CommandResult::NoteClosed => serde_json::Value::Null,
                    crate::session::CommandResult::NoteCreated {
                        buffer_id,
                        revision,
                        path,
                    } => {
                        serde_json::json!({
                            "buffer_id": buffer_id.as_u64(),
                            "revision": revision.as_u64(),
                            "path": path,
                        })
                    }
                    crate::session::CommandResult::NoteHistory { path, entries } => {
                        serde_json::json!({
                            "path": path,
                            "entries": entries
                                .into_iter()
                                .map(|entry| serde_json::json!({
                                    "hash": entry.hash,
                                    "timestamp_secs": entry.timestamp_secs,
                                    "reason": entry.reason.as_str(),
                                    "pins": entry.pins,
                                }))
                                .collect::<Vec<_>>(),
                        })
                    }
                    crate::session::CommandResult::HistoryRestored {
                        buffer_id,
                        new_revision,
                    } => {
                        serde_json::json!({
                            "buffer_id": buffer_id.as_u64(),
                            "revision": new_revision.as_u64(),
                        })
                    }
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
                    crate::session::CommandResult::NoteRenamed { txn_id, from, to } => {
                        serde_json::json!({
                            "txn_id": txn_id,
                            "from": from.to_string_lossy(),
                            "to": to.to_string_lossy(),
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
                    }
                    | crate::session::CommandResult::RecoveryFlushed {
                        buffer_id,
                        revision,
                        ..
                    } => serde_json::json!({
                        "buffer_id": buffer_id.as_u64(),
                        "revision": revision.as_u64(),
                    }),
                    crate::session::CommandResult::EditsDiscarded { buffer_id } => {
                        serde_json::json!({ "buffer_id": buffer_id.as_u64() })
                    }
                };
                ExecutionResult::Local(LocalResult::Success(LocalSuccess::with_command(
                    request_id, command_id, data,
                )))
            }
            SessionResult::Pending {
                request_id: session_id,
            } => {
                self.operation_ids
                    .lock_recovering()
                    .insert(session_id, request_id);
                self.pending_requests.lock_recovering().insert(request_id);
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
            SessionResult::LinkRejected { reason } => ExecutionResult::Local(LocalResult::Error(
                LocalError::with_request(reason.code(), reason.repair_action(), request_id),
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
                    .lock_recovering()
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
    ///
    /// This is the only way completions enter the api tier, so it wakes any
    /// parked [`Flavite::wait_for_operation`] when it returns a non-empty
    /// batch. The notify fires AFTER conversion: a woken waiter observes
    /// fully converted results (pending/id bookkeeping done), never a
    /// half-installed one. Waiters additionally re-check under the same
    /// backlog mutex they park on, so the edge cannot be lost.
    pub fn drain_operation_completions(&self) -> Vec<OperationCompletion> {
        let _turn = self.ingress.lock_recovering();
        let completions: Vec<_> = self
            .session
            .drain_operation_completions()
            .into_iter()
            .map(|completion| {
                let request_id = self
                    .operation_ids
                    .lock_recovering()
                    .remove(&completion.request_id)
                    .unwrap_or(completion.request_id);
                self.pending_requests.lock_recovering().remove(&request_id);
                let result = self.convert_session_result(completion.result, request_id);
                OperationCompletion { request_id, result }
            })
            .collect();
        if !completions.is_empty() {
            self.backlog_cv.notify_all();
        }
        completions
    }

    /// Wait for one admitted operation to reach a terminal result.
    ///
    /// Parks on the backlog's own condvar (Q5/S7): this loop performs no
    /// sleeping poll. The park predicate watches BOTH delivery signals —
    /// "my result is not yet in the backlog" AND "the operation is still
    /// pending" — evaluated under the same mutexes the producers mutate, so
    /// neither a sibling waiter's backlog insert nor another thread's
    /// `drain_operation_completions` (which removes the pending mark) can
    /// be missed: a state change before the park is visible to the
    /// predicate, a change after is delivered by the notify. The 25 ms
    /// tick remains only the absolute ceiling. The `Rejected{Unavailable}`
    /// semantics for unknown/already-drained ids are byte-for-byte the
    /// former ones.
    pub fn wait_for_operation(&self, request_id: RequestId) -> ExecutionResult {
        loop {
            if let Some(result) = self.terminal_backlog.lock_recovering().remove(&request_id) {
                return result;
            }
            if !self
                .pending_requests
                .lock_recovering()
                .contains(&request_id)
            {
                return ExecutionResult::Rejected(
                    RejectedResult::with_request(RejectionReason::Unavailable, request_id)
                        .with_message("operation is not pending or has already been drained"),
                );
            }
            let mut parked_backlog = false;
            for completion in self.drain_operation_completions() {
                if completion.request_id == request_id {
                    return completion.result;
                }
                self.terminal_backlog
                    .lock_recovering()
                    .insert(completion.request_id, completion.result);
                parked_backlog = true;
            }
            if parked_backlog {
                // The drain-level notify fired BEFORE these sibling results
                // were visible in the backlog; this one closes that window
                // for every other parked waiter.
                self.backlog_cv.notify_all();
            }
            let backlog = self.terminal_backlog.lock_recovering();
            let _waited = wait_result_recovering(self.backlog_cv.wait_timeout_while(
                backlog,
                WAIT_TICK,
                |backlog| {
                    !backlog.contains_key(&request_id)
                        && self
                            .pending_requests
                            .lock_recovering()
                            .contains(&request_id)
                },
            ));
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
            .shared_buffer_snapshot(buffer_id, revision)
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
            text: snapshot.text.to_string(),
            base_hash: snapshot
                .base_hash
                .map_or_else(Vec::new, |hash| hash.to_vec()),
            state: snapshot.state,
            path: snapshot.path,
        })
    }

    /// Get a shared immutable view for in-process consumers such as multiple
    /// editor tabs. Serialized API consumers continue to use [`BufferView`].
    pub fn get_shared_buffer_view(
        &self,
        buffer_id: BufferId,
        revision: Option<Revision>,
    ) -> Result<SharedBufferView, FlaviteError> {
        let snapshot = self
            .session
            .shared_buffer_snapshot(buffer_id, revision)
            .map_err(|error| match error {
                flavite_vault::session::BufferSnapshotError::NotFound(id) => {
                    FlaviteError::BufferNotFound(id)
                }
                flavite_vault::session::BufferSnapshotError::Stale { expected, actual } => {
                    FlaviteError::StaleBufferView { expected, actual }
                }
            })?;
        Ok(SharedBufferView {
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

    /// Current index and watcher summary for CLI and diagnostics.
    pub fn vault_info(&self) -> Result<VaultInfo, FlaviteError> {
        let version = self.session.version().map_err(FlaviteError::IndexError)?;
        let db_path = crate::doctor::cache_db_path(self.vault_ref.uuid);
        let (note_count, tantivy_generation) = if db_path.exists() {
            let connection = rusqlite::Connection::open_with_flags(
                &db_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .map_err(|error| FlaviteError::IndexError(error.to_string()))?;
            let count = u64::try_from(connection.query_row(
                "SELECT count(*) FROM notes",
                [],
                |row| row.get::<_, i64>(0),
            )?)
            .map_err(|error| FlaviteError::IndexError(error.to_string()))?;
            let committed = connection
                .query_row(
                    "SELECT value FROM meta WHERE key='tantivy_committed_generation'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            (count, committed)
        } else {
            (0, 0)
        };
        Ok(VaultInfo {
            note_count,
            epoch: version.epoch.value(),
            generation: version.generation.value(),
            tantivy_generation,
            watcher_enabled: self
                .owned_vault
                .as_ref()
                .is_some_and(Vault::watcher_enabled),
            drift_found_total: self.drift_found_total(),
            autosave_failures_total: self.session.autosave_failures_total(),
            rename_items_skipped_total: self.session.rename_items_skipped_total(),
        })
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
                .map(|row| NoteResult {
                    note_id: row.note_id,
                    path: row.path,
                    title: row.title,
                    excerpt: row.excerpt,
                })
                .collect(),
            epoch: page.epoch,
            generation: page.generation,
            has_more,
        })
    }

    /// Query the index on behalf of a capability-bearing API consumer.
    pub fn query_as(
        &self,
        capabilities: &[crate::Capability],
        query: &crate::Query,
        limit: usize,
    ) -> Result<QueryResult, FlaviteError> {
        if !capabilities
            .iter()
            .any(|grant| grant.covers(&crate::Capability::ReadNotes, None))
        {
            return Err(FlaviteError::CapabilityDenied("ReadNotes"));
        }
        self.query_with_limit(query, limit)
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
    pub state: crate::BufferStatus,
    /// Vault-relative path used by save and close commands.
    pub path: PathBuf,
}

/// In-process immutable revision shared by all views of one Session buffer.
#[derive(Debug, Clone)]
pub struct SharedBufferView {
    /// Buffer ID.
    pub buffer_id: u64,
    /// Buffer revision.
    pub revision: u64,
    /// Shared normalized text.
    pub text: Arc<str>,
    /// Base hash.
    pub base_hash: Vec<u8>,
    /// Typed canonical buffer state.
    pub state: crate::BufferStatus,
    /// Vault-relative path.
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
    /// Bounded saved-content excerpt around the first matched term (W10);
    /// empty for structural-only results. Dirty edits are absent by design.
    #[serde(default)]
    pub excerpt: String,
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

    /// Vault operation could not be submitted.
    #[error("Vault operation failed: {0}")]
    SessionOperation(String),

    /// A consumer attempted an API operation without its declared capability.
    #[error("missing capability: {0}")]
    CapabilityDenied(&'static str),

    /// Session error.
    #[error("Session error: {0}")]
    SessionError(#[from] SessionError),

    /// Vault open error.
    #[error("Vault open error: {0}")]
    VaultOpenError(String),
    /// Device-local user state failed (W06).
    #[error("User state error: {0}")]
    UserState(#[from] flavite_vault::user_state::UserStateError),

    /// JSON serialization error.
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
    /// SQLite read failed.
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Session module re-export for convenience.
pub mod session {
    pub use flavite_vault::session::{
        CommandResult, ConflictResolution, OpenPolicy, SessionCommand, SessionError, SessionResult,
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

/// Read-only status reported by `flavite info` and diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VaultInfo {
    /// Indexed Markdown note count.
    pub note_count: u64,
    /// Index epoch.
    pub epoch: u64,
    /// Structural index generation.
    pub generation: u64,
    /// Last committed full-text search generation.
    pub tantivy_generation: u64,
    /// Whether filesystem watching was enabled for this open.
    pub watcher_enabled: bool,
    /// Cumulative drift count observed by reconciliation.
    pub drift_found_total: u64,
    /// Cumulative autosave failures observed by the Session host pump.
    pub autosave_failures_total: u64,
    /// Cumulative rename journal items skipped by the actor.
    pub rename_items_skipped_total: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FR-8 passthrough: a vault `SessionEvent::Panicked` must convert onto
    /// the api event surface so hosts see it without importing vault types.
    /// Removing the `Panicked` arm in `session_event_to_api` fails this.
    #[test]
    fn session_panicked_event_surfaces_on_api_tier() {
        use flavite_vault::session::SessionEvent;
        let vault = ApiVaultRef::new(uuid::Uuid::new_v4(), PathBuf::from("/test/vault"));

        let mapped = Flavite::session_event_to_api(
            &vault,
            SessionEvent::Panicked {
                message: "disk write tripped an assert".into(),
                panics_total: 3,
            },
        );
        assert!(
            matches!(
                mapped,
                Some(crate::Event::SessionPanicked {
                    ref message,
                    panics_total: 3,
                    ..
                }) if message == "disk write tripped an assert"
            ),
            "Panicked must surface: {mapped:?}"
        );
        assert_eq!(
            mapped.unwrap().event_type(),
            "session_panicked",
            "event_type must name the new variant"
        );

        // Existing externally observable behavior preserved (FR-13): the
        // change-detected mapping is untouched, other variants stay on
        // their dedicated channels (filtered here as before).
        let external = Flavite::session_event_to_api(
            &vault,
            SessionEvent::ExternalChangeDetected {
                path: PathBuf::from("/test/vault/note.md"),
                deleted: false,
            },
        );
        assert!(matches!(
            external,
            Some(crate::Event::ExternalChangeDetected { .. })
        ));
        assert!(
            Flavite::session_event_to_api(
                &vault,
                SessionEvent::RecoveryAvailable {
                    path: PathBuf::from("/test/vault/note.md"),
                },
            )
            .is_none()
        );
    }

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

    fn await_initial_reconcile(flavite: &Flavite) {
        let request_id = match flavite.session.execute(SessionCommand::Reconcile {
            scope: flavite_vault::ReconcileScope::Full,
            mode: flavite_vault::ReconcileMode::VerifyContent,
        }) {
            SessionResult::Pending { request_id } => request_id,
            result => panic!("reconcile was not admitted: {result:?}"),
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            flavite.pump();
            if let Some(result) = flavite
                .session
                .drain_operation_completions()
                .into_iter()
                .find(|completion| completion.request_id == request_id)
            {
                assert!(matches!(result.result, SessionResult::Success(_)));
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "initial reconcile timed out; session backlog={:?} api pending={:?}",
                flavite
                    .session
                    .drain_operation_completions()
                    .into_iter()
                    .map(|completion| completion.request_id)
                    .collect::<Vec<_>>(),
                flavite.pending_requests.lock_recovering().len(),
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn note_rename_is_registered_typed_and_requires_write_capability() {
        let (flavite, _root, _temp) = test_flavite();
        let spec = flavite.command_registry().get_spec("note.rename").unwrap();
        assert!(spec.requires_write);
        assert_eq!(
            spec.required_capability,
            Some(crate::Capability::WriteNotes {
                scope: crate::WriteScope::Vault
            })
        );
        let valid = serde_json::json!({
            "from": "note.md",
            "to": "renamed.md",
            "strategy": "alias"
        });
        spec.args.validate(&valid).unwrap();
        assert!(
            spec.args
                .validate(&serde_json::json!({
                    "from": "note.md",
                    "to": "renamed.md",
                    "strategy": "invalid"
                }))
                .is_err()
        );

        let result = flavite.execute(Invocation::new(
            71,
            "note.rename",
            valid,
            crate::Origin::Plugin,
        ));
        assert!(matches!(
            result,
            ExecutionResult::Rejected(crate::RejectedResult {
                reason: crate::RejectionReason::CapabilityDenied,
                ..
            })
        ));
    }

    #[test]
    fn notice_acknowledgement_ledger_is_bounded_and_ackable() {
        let (flavite, _root, _temp) = test_flavite();
        for _ in 0..(MAX_NOTICE_ACKS + 5) {
            flavite.session.notice_channel().send(
                flavite_vault::channels::Notice::ActionRequired {
                    code: "ledger-test",
                    note: None,
                },
            );
        }
        let first = flavite.poll_events(usize::MAX);
        assert_eq!(first.len(), MAX_NOTICE_ACKS);
        assert_eq!(flavite.outstanding_notice_count(), MAX_NOTICE_ACKS);
        let blocked = flavite.poll_events(usize::MAX);
        assert!(blocked.iter().all(|delivery| delivery.notice_id.is_none()));
        assert_eq!(flavite.outstanding_notice_count(), MAX_NOTICE_ACKS);
        for delivery in first {
            assert!(flavite.acknowledge_notice(delivery.notice_id.unwrap()));
        }
        let remainder = flavite.poll_events(usize::MAX);
        assert_eq!(remainder.len(), 5);
        for delivery in remainder {
            assert!(flavite.acknowledge_notice(delivery.notice_id.unwrap()));
        }
        assert_eq!(flavite.outstanding_notice_count(), 0);
    }

    #[test]
    fn buffer_view_serialization() {
        let view = BufferView {
            buffer_id: 1,
            revision: 1,
            text: "test content".to_string(),
            base_hash: vec![1, 2, 3],
            state: crate::BufferStatus::Clean,
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
        let opened = match flavite.session.execute(SessionCommand::OpenNote {
            path: root.join("note.md"),
            policy: flavite_vault::session::OpenPolicy::CreateIfMissing,
        }) {
            SessionResult::Pending { request_id } => flavite.session.wait_for_operation(request_id),
            result => result,
        };
        let buffer_id = match opened {
            SessionResult::Success(crate::session::CommandResult::NoteOpened {
                buffer_id, ..
            }) => buffer_id,
            other => panic!("unexpected open result: {other:?}"),
        };
        let initial = flavite.get_buffer_view(buffer_id, None).unwrap();
        assert_eq!(initial.text, "hello");
        assert_eq!(initial.path, root.join("note.md"));
        assert_eq!(initial.state, crate::BufferStatus::Clean);

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
        assert_eq!(current.state, crate::BufferStatus::Dirty);
    }

    #[test]
    fn query_result_serialization() {
        let result = QueryResult {
            notes: vec![NoteResult {
                note_id: 1,
                path: "/test/note.md".to_string(),
                title: "Test Note".to_string(),
                excerpt: String::new(),
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
        await_initial_reconcile(&flavite);
        let page = flavite
            .query_with_limit(&crate::Query::Text("hello".into()), 10)
            .unwrap();
        assert_eq!(page.notes.len(), 1);
        assert_eq!(page.notes[0].path, "note.md");
        assert_eq!(page.generation, 1);
        assert!(!page.has_more);
    }

    #[test]
    fn query_as_enforces_read_notes_capability() {
        let (flavite, _root, _temp) = test_flavite();
        await_initial_reconcile(&flavite);
        assert!(matches!(
            flavite.query_as(&[], &crate::Query::Text("hello".into()), 10),
            Err(FlaviteError::CapabilityDenied("ReadNotes"))
        ));
        assert_eq!(
            flavite
                .query_as(
                    &[crate::Capability::ReadNotes],
                    &crate::Query::Text("hello".into()),
                    10
                )
                .unwrap()
                .notes
                .len(),
            1
        );
    }

    #[test]
    fn public_event_drain_maps_external_change_and_conflict() {
        use flavite_buffer::manager::BufferManager;
        use flavite_fs::{Fs, MemFs};
        use flavite_index::IndexActor;
        use flavite_vault::actor::{VaultActorBuilder, event_source_channel};
        use flavite_vault::channels::ChannelSet;
        use flavite_vault::coalesce::WatcherEvent;
        use flavite_vault::session::Session;
        use std::time::{Duration, Instant};
        let fs = MemFs::new();
        let root = PathBuf::from("/test/event-vault");
        fs.create_dir_all(&root).unwrap();
        fs.write_new(&root.join("note.md"), b"before").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("event-index.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let identity = VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![7, 8, 9]);
        let (watch_tx, watch_rx) = event_source_channel();
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs.clone()) as Box<dyn Fs + Send + Sync>)
            .with_identity(identity.clone())
            .with_index(IndexActor::new(&db, 1).unwrap())
            .with_event_source(watch_rx)
            .build()
            .unwrap()
            .start()
            .unwrap();
        actor
            .wait_until_ready(Duration::from_secs(10))
            .expect("initial reconcile completes before external-change assertions");
        let channels = ChannelSet::new();
        let session = Session::new(
            identity.clone(),
            actor,
            BufferManager::new(),
            channels.pulse,
            channels.notice,
            VaultConfig::default(),
        );
        let flavite = Flavite::open(identity, session, VaultConfig::default()).unwrap();
        let opened = match flavite.session.execute(SessionCommand::OpenNote {
            path: PathBuf::from("note.md"),
            policy: flavite_vault::session::OpenPolicy::CreateIfMissing,
        }) {
            SessionResult::Pending { request_id } => {
                let completion = flavite.session.wait_for_operation(request_id);
                match completion {
                    SessionResult::Success(crate::session::CommandResult::NoteOpened {
                        buffer_id,
                        ..
                    }) => serde_json::json!({ "buffer_id": buffer_id.as_u64() }),
                    other => panic!("unexpected open completion: {other:?}"),
                }
            }
            SessionResult::Success(_) => {
                unreachable!("direct Session opens are async")
            }
            other => panic!("unexpected open result: {other:?}"),
        };
        let buffer_id = opened["buffer_id"].as_u64().expect("buffer id");
        let view = flavite.get_buffer_view(buffer_id.into(), None).unwrap();
        flavite.session.execute(SessionCommand::ApplyEdits {
            buffer_id: buffer_id.into(),
            revision: Revision::new(view.revision),
            ops: vec![flavite_buffer::undo::EditOp::new(
                flavite_syntax::text::Span::new(
                    flavite_syntax::text::TextByte(6),
                    flavite_syntax::text::TextByte(6),
                ),
                " mine".into(),
            )],
        });
        fs.write_new(&root.join(".external"), b"external").unwrap();
        fs.rename_replace(&root.join(".external"), &root.join("note.md"))
            .unwrap();
        watch_tx
            .send(WatcherEvent::Modified(root.join("note.md")))
            .unwrap();
        // The initial scan is complete above, so this waits only for the
        // post-open watcher event being tested.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut events = Vec::new();
        while Instant::now() < deadline {
            flavite.pump();
            events.extend(flavite.poll_events(256));
            if events.iter().any(|delivery| {
                matches!(delivery.event, crate::Event::ExternalChangeDetected { .. })
            }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            events.iter().any(|delivery| matches!(&delivery.event, crate::Event::ExternalChangeDetected { note_path, deleted: false, .. } if note_path.ends_with("note.md"))),
            "{events:?}; backlog={:?} notices={:?}",
            flavite.drain_operation_completions().len(),
            flavite.session.drain_notices().len(),
        );
        flavite
            .session
            .notice_channel()
            .send(flavite_vault::channels::Notice::Conflict {
                note: flavite_buffer::manager::NoteRef::new(root.join("note.md")),
            });
        events.extend(flavite.poll_events(256));
        assert!(events.iter().any(|delivery| matches!(&delivery.event, crate::Event::Conflict { note_path, .. } if note_path.ends_with("note.md"))), "{events:?}");
    }

    #[test]
    fn typed_dispatch_covers_validation_read_only_and_unknown_commands() {
        use crate::command::Invocation;
        use crate::types::Origin;

        let (mut flavite, root, _temp) = test_flavite();
        let denied = flavite.execute(
            Invocation::new(9, "note.open", serde_json::json!("note.md"), Origin::Plugin)
                .with_capabilities(Vec::new()),
        );
        assert!(matches!(
            denied,
            ExecutionResult::Rejected(RejectedResult {
                reason: RejectionReason::CapabilityDenied,
                request_id: Some(9),
                ..
            })
        ));
        let scoped_denial = flavite.execute(
            Invocation::new(
                10,
                "save_note",
                serde_json::json!("outside.md"),
                Origin::Plugin,
            )
            .with_capabilities(vec![crate::Capability::WriteNotes {
                scope: crate::WriteScope::PathPrefix("notes".into()),
            }]),
        );
        assert!(matches!(
            scoped_denial,
            ExecutionResult::Rejected(RejectedResult {
                reason: RejectionReason::CapabilityDenied,
                request_id: Some(10),
                ..
            })
        ));
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

        let opened = match flavite.execute(Invocation::new(
            5,
            "open_note",
            serde_json::to_value(root.join("note.md")).unwrap(),
            Origin::Gui,
        )) {
            result @ ExecutionResult::Local(_) => result,
            ExecutionResult::Pending { request_id } => flavite.wait_for_operation(request_id),
            other => panic!("unexpected open result: {other:?}"),
        };
        let data = opened.into_success().unwrap().data;
        let buffer_id = data["buffer_id"].as_u64().unwrap();
        let closed = flavite.execute(Invocation::new(
            6,
            "note.close",
            serde_json::json!({ "path": root.join("note.md"), "policy": "clean" }),
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
        let opened = match flavite.execute(Invocation::new(
            10,
            "open_note",
            serde_json::to_value(root.join("note.md")).unwrap(),
            Origin::Gui,
        )) {
            result @ ExecutionResult::Local(_) => result,
            ExecutionResult::Pending { request_id } => flavite.wait_for_operation(request_id),
            other => panic!("unexpected open result: {other:?}"),
        };
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

    /// S7 concurrency net (R3): a waiter and an external drainer race for
    /// one admitted operation. The api ledger must deliver the terminal
    /// result to exactly ONE consumer — either the waiter returns the real
    /// success (its own drain won) or the drainer holds it and the waiter
    /// gets the pinned `Rejected{Unavailable}` ("not pending or has already
    /// been drained"). Two successes, zero successes, or a hang fails.
    #[test]
    fn flavite_wait_delivers_exactly_once_under_concurrent_drain() {
        use crate::command::Invocation;
        use crate::types::Origin;
        use std::time::Duration;
        let (flavite, _root, _temp) = test_flavite();
        await_initial_reconcile(&flavite);

        let request_id = match flavite.execute(Invocation::new(
            45,
            "reconcile",
            serde_json::json!({"scope": "full", "mode": "verify"}),
            Origin::Cli,
        )) {
            ExecutionResult::Pending { request_id } => request_id,
            other => panic!("expected a pending reconcile: {other:?}"),
        };
        let started = std::time::Instant::now();
        let flavite_ref = &flavite;
        let waiter_done = std::sync::Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
        let (waited, drainer_holds, woke_after) = std::thread::scope(|scope| {
            let done_stamp = std::sync::Arc::clone(&waiter_done);
            let waiter = scope.spawn(move || {
                let result = flavite_ref.wait_for_operation(request_id);
                *done_stamp.lock().unwrap() = Some(std::time::Instant::now());
                result
            });
            let mut drainer_holds = false;
            let mut won_at = None;
            while !drainer_holds && !waiter.is_finished() {
                if started.elapsed() > Duration::from_secs(5) {
                    panic!(
                        "concurrent drain/wait stalled; pending={:?} events={:?}",
                        flavite_ref.pending_requests.lock_recovering().len(),
                        flavite_ref.session.drain_events().len(),
                    );
                }
                drainer_holds = flavite_ref
                    .drain_operation_completions()
                    .iter()
                    .any(|completion| completion.request_id == request_id);
                if drainer_holds {
                    won_at = Some(std::time::Instant::now());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let waited = waiter.join().expect("waiter thread");
            // When the drainer won, the waiter's next exit must come from
            // the drain's notify (µs), not from waking out at the tick
            // ceiling: measure it. (Without the notify edge the same test
            // measured ~20 ms — the tick — against this 5 ms bound.)
            let woke_after = match won_at {
                Some(at) => {
                    let stamp = waiter_done
                        .lock()
                        .unwrap()
                        .expect("the waiter completed before the join");
                    Some(if stamp > at {
                        stamp - at
                    } else {
                        Duration::ZERO
                    })
                }
                None => None,
            };
            (waited, drainer_holds, woke_after)
        });
        let waiter_holds = matches!(waited, ExecutionResult::Local(LocalResult::Success(_)));
        assert!(
            waiter_holds ^ drainer_holds,
            "terminal result must reach exactly one consumer: waiter={waited:?} drainer_holds={drainer_holds}"
        );
        if drainer_holds {
            assert!(
                matches!(
                    &waited,
                    ExecutionResult::Rejected(RejectedResult {
                        reason: RejectionReason::Unavailable,
                        ..
                    })
                ),
                "the losing waiter must get the pinned unavailable rejection, got {waited:?}"
            );
            let woke_after = woke_after.expect("drainer won, so the wake was timed");
            assert!(
                woke_after < Duration::from_millis(5),
                "the losing waiter woke on the tick, not the drain notify: {woke_after:?} (tick ceiling {WAIT_TICK:?})"
            );
        }
    }

    /// S7 pinned semantics (t2): an unknown id and an already-drained id
    /// both return `Rejected{Unavailable}` immediately — they never park
    /// behind the tick. These semantics are load-bearing for the backlog
    /// model, so nobody "simplifies" the wait into delegation later.
    #[test]
    fn flavite_wait_rejects_unknown_and_drained_immediately() {
        use std::time::Duration;
        let (flavite, root, _temp) = test_flavite();
        await_initial_reconcile(&flavite);

        // (a) Unknown id: immediate.
        let started = std::time::Instant::now();
        let unknown = flavite.wait_for_operation(999_999);
        let took = started.elapsed();
        assert!(
            matches!(
                &unknown,
                ExecutionResult::Rejected(RejectedResult {
                    reason: RejectionReason::Unavailable,
                    ..
                })
            ),
            "{unknown:?}"
        );
        assert!(
            took < Duration::from_millis(10),
            "unknown id must not park: took {took:?} (tick ceiling is {WAIT_TICK:?})"
        );

        // (b) Already-drained id: real pending op, drained away, then wait.
        use crate::command::Invocation;
        use crate::types::Origin;
        let request_id = match flavite.execute(Invocation::new(
            46,
            "reconcile",
            serde_json::json!({"scope": "full", "mode": "verify"}),
            Origin::Cli,
        )) {
            ExecutionResult::Pending { request_id } => request_id,
            other => panic!("expected a pending reconcile: {other:?}"),
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut delivered = false;
        while !delivered {
            delivered = flavite
                .drain_operation_completions()
                .iter()
                .any(|completion| completion.request_id == request_id);
            assert!(
                std::time::Instant::now() < deadline,
                "reconcile never completed; events={:?}",
                flavite.session.drain_events()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let started = std::time::Instant::now();
        let after_drain = flavite.wait_for_operation(request_id);
        assert!(
            matches!(
                &after_drain,
                ExecutionResult::Rejected(RejectedResult {
                    reason: RejectionReason::Unavailable,
                    ..
                })
            ),
            "{after_drain:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(10),
            "the drained exit must not park: {started:?}"
        );
        let _ = root;
    }

    /// S7 pin (t3, the anti-delegation proof): a waiter servicing one id
    /// relocates its batch siblings into the backlog instead of dropping
    /// them — the very reason the api tier cannot delegate to
    /// `Session::wait_for_operation` (Q5). Deterministic through the
    /// supersede path: the third save admission SYNCHRONOUSLY records the
    /// second save's `Superseded` completion into the Session queue before
    /// the third save's worker can finish, so the waiting third operation's
    /// batch provably carries the superseded sibling first, and it must
    /// still be retrievable afterwards.
    #[test]
    fn flavite_backlog_preserves_sibling_completions() {
        use crate::command::Invocation;
        use crate::types::Origin;
        let (flavite, root, _temp) = test_flavite();
        await_initial_reconcile(&flavite);
        let note = root.join("note.md");

        let opened = match flavite.execute(Invocation::new(
            50,
            "open_note",
            serde_json::to_value(&note).unwrap(),
            Origin::Gui,
        )) {
            result @ ExecutionResult::Local(_) => result,
            ExecutionResult::Pending { request_id } => flavite.wait_for_operation(request_id),
            other => panic!("unexpected open result: {other:?}"),
        };
        opened.into_success().expect("open succeeds");
        let note_value = serde_json::json!(note.to_string_lossy().into_owned());

        // Three admissions with no pump between them: save #1 becomes the
        // active flight, #2 the coalesced queued successor, #3 replaces the
        // queued slot — recording `Superseded{#2 by #3}` synchronously.
        for id in 51..=53 {
            match flavite.execute(Invocation::new(
                id,
                "save_note",
                note_value.clone(),
                Origin::Gui,
            )) {
                ExecutionResult::Pending { request_id } => {
                    assert_eq!(request_id, id, "api request id is the invocation id");
                }
                other => panic!("expected pending save {id}: {other:?}"),
            }
        }
        let third = flavite.wait_for_operation(53);
        assert!(
            matches!(&third, ExecutionResult::Local(LocalResult::Success(_))),
            "third save must terminate successfully: {third:?}"
        );

        // The superseded sibling survived the relocation into the backlog.
        let superseded = flavite.wait_for_operation(52);
        assert!(
            matches!(
                superseded,
                ExecutionResult::Superseded {
                    request_id: 52,
                    by_request: 53,
                }
            ),
            "backlog must preserve the unrelated sibling completion: {superseded:?}"
        );
        // …to exactly one consumer: it is no longer in the drain batch.
        assert!(
            !flavite
                .drain_operation_completions()
                .iter()
                .any(|completion| completion.request_id == 52),
            "a backlog-served result must not be delivered twice"
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
