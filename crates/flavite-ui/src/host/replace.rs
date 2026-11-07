//! Vault source Find and sealed replacement preview host state (RFC-0006
//! S07, D-08/D-09).
//!
//! Service state lives here, not in the view: source mode is explicitly
//! distinct from full-text (D-05), the frozen scope/truncation is visible,
//! typed exclusions are surfaced, and the sealed preview exposes candidate
//! rows with exact before/after hashes plus selection. Everything runs
//! through ReadNotes facade commands; nothing here ever applies a
//! replacement (Apply is S08).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use flavite_api::{ExecutionResult, Invocation, LocalResult, OperationCompletion, Origin};

/// Debounce window before an edited query is admitted.
pub const REPLACE_DEBOUNCE: Duration = Duration::from_millis(150);
/// Maximum submitted query/replacement length (NFR-8).
const MAX_QUERY_BYTES: usize = 8192;
const MAX_REPLACEMENT_BYTES: usize = 64 * 1024;
/// Matches per page (D-07 default 50).
const PAGE_LEN: usize = 50;

/// Which S07 panel a Replace tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplacePanelMode {
    /// Vault source find (literal/regex over saved Markdown source).
    #[default]
    Find,
    /// Sealed replacement preview (non-applying).
    Preview,
}

/// One find row (presentation projection).
#[derive(Debug, Clone, PartialEq)]
pub struct FindRowView {
    pub ordinal: u64,
    pub path: String,
    pub title: String,
    pub note_matches: u64,
    pub matches: Vec<(u64, u64)>,
}

/// Sealed preview plan summary (header only; never note bodies).
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewPlanView {
    pub scope: (u64, u64),
    pub matches: u64,
    pub matched_notes: u64,
    pub excluded: u64,
    pub drafts: u64,
    pub payload_bytes: u64,
    pub expires_at_unix_ms: u64,
    pub selection_revision: Option<u64>,
    pub selection_digest: Option<String>,
}

/// One candidate row.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateView {
    pub item_id: u64,
    pub path: String,
    pub path_native: String,
    pub match_count: u64,
    pub output_bytes: u64,
    pub no_change: bool,
    pub before_hash: String,
    pub after_hash: String,
    /// `"saved"` or `"draft"` (RFC-0006 S09).
    pub source_kind: String,
}

/// One durable replacement job summary (RFC-0006 S08/S09).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobView {
    pub job_id: String,
    pub phase: String,
    pub selected: u64,
    pub saved: u64,
    pub index_pending: u64,
    pub conflicts: u64,
    pub applied_drafts: u64,
    pub stale: u64,
    pub needs_recovery: u64,
    pub not_started: u64,
    /// S09 revert-epoch outcomes.
    pub reverted: u64,
    pub reverted_drafts: u64,
}

/// Per-tab Replace state.
#[derive(Debug, Default)]
pub struct ReplaceTabState {
    pub mode: ReplacePanelMode,
    pub query: String,
    pub replacement: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
    /// Opt-in draft inclusion (RFC-0006 S09/D-09).
    pub include_drafts: bool,
    edited_at: Option<Instant>,
    epoch: u64,
    sequence: u64,
    pub updating: bool,
    pub error: Option<String>,
    /// Source-find session.
    pub finder: Option<String>,
    pub snapshot: Option<(u64, u64)>,
    pub total_notes: Option<u64>,
    pub total_matches: Option<u64>,
    pub truncated_scope: bool,
    pub exclusions: Vec<(String, String)>,
    pub rows: Vec<FindRowView>,
    next_note_index: Option<u64>,
    next_match_offset: Option<u64>,
    /// Sealed preview.
    pub preview_id: Option<String>,
    pub plan: Option<PreviewPlanView>,
    pub candidates: Vec<CandidateView>,
    pub selected: BTreeMap<String, bool>,
    pub selection_ready: bool,
    /// The in-flight selection action (applied on success).
    pending_selection: Option<SelectAction>,
    /// RFC-0006 S08: durable job bound to this tab's preview.
    pub job: Option<JobView>,
    /// The in-flight apply/cancel/resume action id.
    job_action_request: Option<u64>,
}

impl ReplaceTabState {
    /// Whether a durable apply may run (sealed + selected + idle).
    #[must_use]
    pub fn can_apply(&self) -> bool {
        self.preview_id.is_some()
            && self.selection_ready
            && self.job.is_none()
            && self.job_action_request.is_none()
    }

    /// Whether cancel/resume is available.
    #[must_use]
    pub fn can_cancel(&self) -> bool {
        self.job_action_request.is_none() && self.job.is_some()
    }

    /// Whether a guarded Revert is available: the job finished (or paused/
    /// cancelled) and applied at least one item, and it is not already a
    /// completed revert epoch.
    #[must_use]
    pub fn can_revert(&self) -> bool {
        if self.job_action_request.is_some() {
            return false;
        }
        let Some(job) = &self.job else {
            return false;
        };
        if job.reverted > 0 || job.reverted_drafts > 0 {
            return false;
        }
        let applied = job.saved + job.index_pending + job.conflicts + job.applied_drafts;
        applied > 0
            && matches!(
                job.phase.as_str(),
                "completed" | "partial" | "cancelled" | "paused" | "needs-recovery"
            )
    }

    /// Apply a job-status completion to this tab's state.
    fn apply_job_status(&mut self, status: serde_json::Value) {
        let phase = status
            .get("phase")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let totals = status.get("totals").cloned().unwrap_or_default();
        self.job = Some(JobView {
            phase,
            selected: totals
                .get("selected")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            saved: totals
                .get("saved")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            index_pending: totals
                .get("index_pending")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            conflicts: totals
                .get("conflicts")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            applied_drafts: totals
                .get("applied_drafts")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            stale: totals
                .get("stale")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            needs_recovery: totals
                .get("needs_recovery")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            not_started: totals
                .get("not_started")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            reverted: totals
                .get("reverted")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            reverted_drafts: totals
                .get("reverted_drafts")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            ..JobView::default()
        });
    }
}

impl ReplaceTabState {
    #[must_use]
    pub fn has_next(&self) -> bool {
        self.next_note_index.is_some() || self.next_match_offset.is_some()
    }

    #[must_use]
    pub fn selected_count(&self) -> usize {
        self.selected.values().filter(|selected| **selected).count()
    }

    fn clear_find(&mut self) {
        self.rows.clear();
        self.snapshot = None;
        self.total_notes = None;
        self.total_matches = None;
        self.truncated_scope = false;
        self.exclusions.clear();
        self.next_note_index = None;
        self.next_match_offset = None;
    }

    fn clear_preview(&mut self) {
        self.preview_id = None;
        self.plan = None;
        self.candidates.clear();
        self.selected.clear();
        self.selection_ready = false;
        self.pending_selection = None;
        self.job = None;
        self.job_action_request = None;
    }

    /// Apply a completed selection to the candidate flags.
    fn apply_selection(&mut self, selection: &SelectAction) {
        match selection {
            SelectAction::All => {
                for selected in self.selected.values_mut() {
                    *selected = true;
                }
            }
            SelectAction::None => {
                for selected in self.selected.values_mut() {
                    *selected = false;
                }
            }
            SelectAction::Keys(keys) => {
                for key in keys {
                    if let Some(selected) = self.selected.get_mut(key) {
                        *selected = true;
                    }
                }
            }
        }
    }
}

/// Host-owned per-tab replace states plus pending request correlation.
#[derive(Debug, Default)]
pub struct ReplaceHost {
    pub tabs: BTreeMap<flavite_workspace::TabId, ReplaceTabState>,
    find_requests: BTreeMap<u64, (flavite_workspace::TabId, u64)>,
    preview_requests: BTreeMap<u64, (flavite_workspace::TabId, u64)>,
    select_requests: BTreeMap<u64, flavite_workspace::TabId>,
    /// RFC-0006 S08: job command requests (apply/cancel/resume/status).
    job_requests: BTreeMap<u64, flavite_workspace::TabId>,
}

impl ReplaceHost {
    /// The render state for one tab (its own or the shared default).
    #[must_use]
    pub fn view_state(&self, tab: flavite_workspace::TabId) -> &ReplaceTabState {
        match self.tabs.get(&tab) {
            Some(state) => state,
            None => {
                static EMPTY: std::sync::OnceLock<ReplaceTabState> = std::sync::OnceLock::new();
                EMPTY.get_or_init(ReplaceTabState::default)
            }
        }
    }

    /// Drop the state of a closed tab; its sessions are closed best-effort.
    pub fn detach(
        &mut self,
        tab: flavite_workspace::TabId,
        flavite: Option<&flavite_api::Flavite>,
    ) {
        let Some(state) = self.tabs.remove(&tab) else {
            return;
        };
        self.find_requests.retain(|_, (owner, _)| *owner != tab);
        self.preview_requests.retain(|_, (owner, _)| *owner != tab);
        self.select_requests.retain(|_, owner| *owner != tab);
        self.job_requests.retain(|_, owner| *owner != tab);
        let Some(api) = flavite else {
            return;
        };
        if let Some(finder) = state.finder {
            submit_close(api, "source_find.close", finder);
        }
        if let Some(preview) = state.preview_id {
            // Preview close drops the live session reference; the store TTL
            // sweeps the spool. (No close command in S07.)
            let _ = preview;
        }
    }

    /// Record an edited field; outcomes from older epochs are rejected on
    /// arrival.
    pub fn edited(&mut self, tab: flavite_workspace::TabId, field: ReplaceField, value: String) {
        let state = self.tabs.entry(tab).or_default();
        state.epoch = state.epoch.wrapping_add(1);
        state.edited_at = Some(Instant::now());
        state.updating = false;
        state.error = None;
        match field {
            ReplaceField::Query => {
                state.query = value;
                state.clear_find();
                state.clear_preview();
            }
            ReplaceField::Replacement => {
                state.replacement = value;
                state.clear_preview();
            }
        }
    }

    /// Toggle regex/case/whole-word; re-admits the current query.
    pub fn toggle(&mut self, tab: flavite_workspace::TabId, flag: ReplaceFlag) {
        let state = self.tabs.entry(tab).or_default();
        match flag {
            ReplaceFlag::Regex => state.regex = !state.regex,
            ReplaceFlag::CaseSensitive => state.case_sensitive = !state.case_sensitive,
            ReplaceFlag::WholeWord => state.whole_word = !state.whole_word,
            ReplaceFlag::IncludeDrafts => state.include_drafts = !state.include_drafts,
        }
        if !state.query.trim().is_empty() {
            state.edited_at = Some(Instant::now() - REPLACE_DEBOUNCE);
            state.epoch = state.epoch.wrapping_add(1);
            state.clear_find();
            state.clear_preview();
        }
    }

    /// Switch the panel mode.
    pub fn set_mode(&mut self, tab: flavite_workspace::TabId, mode: ReplacePanelMode) {
        let state = self.tabs.entry(tab).or_default();
        state.mode = mode;
    }

    /// Submit the focused query immediately.
    pub fn submit_now(&mut self, tab: flavite_workspace::TabId) {
        if let Some(state) = self.tabs.get_mut(&tab) {
            state.edited_at = Some(Instant::now() - REPLACE_DEBOUNCE);
        }
    }

    /// Submit due find searches (only in Find mode; preview runs explicitly).
    pub fn admit_due(&mut self, flavite: Option<&flavite_api::Flavite>) {
        let due: Vec<flavite_workspace::TabId> = self
            .tabs
            .iter()
            .filter(|(_, state)| {
                state.mode == ReplacePanelMode::Find
                    && state
                        .edited_at
                        .is_some_and(|edited| edited.elapsed() >= REPLACE_DEBOUNCE)
            })
            .map(|(tab, _)| *tab)
            .collect();
        for tab in due {
            self.submit_find(tab, flavite);
        }
    }

    /// Start (or restart) a vault source find.
    pub fn submit_find(
        &mut self,
        tab: flavite_workspace::TabId,
        flavite: Option<&flavite_api::Flavite>,
    ) {
        let Some(api) = flavite else {
            return;
        };
        let (query, regex, case_sensitive, whole_word, epoch) = {
            let Some(state) = self.tabs.get_mut(&tab) else {
                return;
            };
            if state.mode != ReplacePanelMode::Find {
                return;
            }
            if state.query.trim().is_empty() {
                state.updating = false;
                state.error = None;
                return;
            }
            if state.query.len() > MAX_QUERY_BYTES {
                state.updating = false;
                state.error = Some("Query is too long".into());
                return;
            }
            state.sequence = state.sequence.saturating_add(1);
            state.edited_at = None;
            state.updating = true;
            state.error = None;
            (
                state.query.clone(),
                state.regex,
                state.case_sensitive,
                state.whole_word,
                state.epoch,
            )
        };
        let request_id = api.command_registry().next_request_id();
        let result = api.execute(Invocation::new(
            request_id,
            "source_find.start",
            serde_json::json!({
                "query": query,
                "mode": if regex { "regex" } else { "literal" },
                "case_sensitive": case_sensitive,
                "whole_word": whole_word,
            }),
            Origin::Gui,
        ));
        match result {
            ExecutionResult::Pending { request_id } => {
                self.find_requests.insert(request_id, (tab, epoch));
            }
            ExecutionResult::Local(LocalResult::Error(error)) => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.updating = false;
                    state.error = Some(error.message);
                }
            }
            ExecutionResult::Rejected(rejected) => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.updating = false;
                    state.error = Some(
                        rejected
                            .message
                            .unwrap_or_else(|| "Source find was refused".into()),
                    );
                }
            }
            _ => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.updating = false;
                    state.error = Some("Source find is stale; search again".into());
                }
            }
        }
    }

    /// One continuation page.
    pub fn next_page(
        &mut self,
        tab: flavite_workspace::TabId,
        flavite: Option<&flavite_api::Flavite>,
    ) {
        let Some(api) = flavite else {
            return;
        };
        let (finder, note_index, match_offset, epoch) = {
            let Some(state) = self.tabs.get_mut(&tab) else {
                return;
            };
            let Some(finder) = state.finder.clone() else {
                return;
            };
            state.sequence = state.sequence.saturating_add(1);
            state.updating = true;
            (
                finder,
                state.rows.last().map(|row| row.ordinal).unwrap_or(0),
                state.next_match_offset.unwrap_or(0),
                state.epoch,
            )
        };
        let request_id = api.command_registry().next_request_id();
        let result = api.execute(Invocation::new(
            request_id,
            "source_find.page",
            serde_json::json!({
                "finder_id": finder,
                "note_index": note_index,
                "match_offset": match_offset,
                "page_len": PAGE_LEN,
            }),
            Origin::Gui,
        ));
        match result {
            ExecutionResult::Pending { request_id } => {
                self.find_requests.insert(request_id, (tab, epoch));
            }
            _ => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.updating = false;
                    state.error = Some("Source find page is busy; try again".into());
                }
            }
        }
    }

    /// Run a sealed replacement preview from the current find options.
    pub fn run_preview(
        &mut self,
        tab: flavite_workspace::TabId,
        flavite: Option<&flavite_api::Flavite>,
    ) {
        let Some(api) = flavite else {
            return;
        };
        let (query, replacement, regex, case_sensitive, whole_word, include_drafts, epoch) = {
            let Some(state) = self.tabs.get_mut(&tab) else {
                return;
            };
            if state.query.trim().is_empty() {
                state.error = Some("Query is empty".into());
                return;
            }
            if state.query.len() > MAX_QUERY_BYTES {
                state.error = Some("Query is too long".into());
                return;
            }
            if state.replacement.len() > MAX_REPLACEMENT_BYTES {
                state.error = Some("Replacement is too long".into());
                return;
            }
            // Bump the epoch: any in-flight find outcome is stale from here
            // on and is dropped on arrival.
            state.epoch = state.epoch.wrapping_add(1);
            state.sequence = state.sequence.saturating_add(1);
            state.updating = true;
            state.error = None;
            state.clear_preview();
            (
                state.query.clone(),
                state.replacement.clone(),
                state.regex,
                state.case_sensitive,
                state.whole_word,
                state.include_drafts,
                state.epoch,
            )
        };
        let request_id = api.command_registry().next_request_id();
        let result = api.execute(Invocation::new(
            request_id,
            "replace.preview",
            serde_json::json!({
                "query": query,
                "replacement": replacement,
                "mode": if regex { "regex" } else { "literal" },
                "case_sensitive": case_sensitive,
                "whole_word": whole_word,
                "include_drafts": include_drafts,
            }),
            Origin::Gui,
        ));
        match result {
            ExecutionResult::Pending { request_id } => {
                self.preview_requests.insert(request_id, (tab, epoch));
            }
            ExecutionResult::Local(LocalResult::Error(error)) => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.updating = false;
                    state.error = Some(error.message);
                }
            }
            ExecutionResult::Rejected(rejected) => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.updating = false;
                    state.error = Some(
                        rejected
                            .message
                            .unwrap_or_else(|| "Preview was refused".into()),
                    );
                }
            }
            _ => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.updating = false;
                    state.error = Some("Preview is stale; run it again".into());
                }
            }
        }
    }

    /// Record a selection (all/none/keys).
    pub fn select(
        &mut self,
        tab: flavite_workspace::TabId,
        selection: SelectAction,
        flavite: Option<&flavite_api::Flavite>,
    ) {
        let Some(api) = flavite else {
            return;
        };
        let preview_id = {
            let Some(state) = self.tabs.get_mut(&tab) else {
                return;
            };
            let Some(preview_id) = state.preview_id.clone() else {
                return;
            };
            state.updating = true;
            state.error = None;
            state.pending_selection = Some(selection.clone());
            preview_id
        };
        let value = match selection {
            SelectAction::All => serde_json::json!("all"),
            SelectAction::None => serde_json::json!("none"),
            SelectAction::Keys(keys) => serde_json::json!({ "keys": keys }),
        };
        let request_id = api.command_registry().next_request_id();
        let result = api.execute(Invocation::new(
            request_id,
            "replace.select",
            serde_json::json!({ "preview_id": preview_id, "select": value }),
            Origin::Gui,
        ));
        match result {
            ExecutionResult::Pending { request_id } => {
                self.select_requests.insert(request_id, tab);
            }
            ExecutionResult::Local(LocalResult::Success(success)) => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.updating = false;
                    let data = &success.data;
                    let revision = data
                        .get("selection_revision")
                        .and_then(serde_json::Value::as_u64);
                    if let Some(plan) = &mut state.plan {
                        plan.selection_revision = revision;
                        plan.selection_digest = data
                            .get("selection_digest")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned);
                    }
                    state.selection_ready = revision.is_some();
                    if let Some(selection) = state.pending_selection.take() {
                        state.apply_selection(&selection);
                    }
                    state.error = None;
                }
            }
            ExecutionResult::Local(LocalResult::Error(error)) => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.updating = false;
                    state.error = Some(error.message);
                }
            }
            _ => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.updating = false;
                    state.error = Some("Selection was refused".into());
                }
            }
        }
    }

    /// Route one drained completion; true when it belonged to replace.
    pub fn observe(&mut self, completion: &OperationCompletion) -> bool {
        if let Some((tab, epoch)) = self.find_requests.remove(&completion.request_id) {
            let Some(state) = self.tabs.get_mut(&tab) else {
                return true;
            };
            if epoch != state.epoch {
                return true;
            }
            match &completion.result {
                ExecutionResult::Local(LocalResult::Success(success)) => {
                    let data = &success.data;
                    state.finder = data
                        .get("finder_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    state.snapshot = data
                        .get("snapshot")
                        .and_then(serde_json::Value::as_object)
                        .map(|snapshot| {
                            (
                                snapshot
                                    .get("epoch")
                                    .and_then(serde_json::Value::as_u64)
                                    .unwrap_or(0),
                                snapshot
                                    .get("generation")
                                    .and_then(serde_json::Value::as_u64)
                                    .unwrap_or(0),
                            )
                        });
                    state.total_notes = data.get("total_notes").and_then(serde_json::Value::as_u64);
                    state.total_matches = data
                        .get("total_matches")
                        .and_then(serde_json::Value::as_u64);
                    state.truncated_scope = data
                        .get("truncated_scope")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    state.exclusions = data
                        .get("exclusions")
                        .and_then(serde_json::Value::as_array)
                        .map(|rows| {
                            rows.iter()
                                .filter_map(|row| {
                                    Some((
                                        row.get("path")?.as_str()?.to_owned(),
                                        row.get("reason")?.as_str()?.to_owned(),
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    install_find_page(state, data.get("page"));
                    state.updating = false;
                    state.error = None;
                }
                ExecutionResult::Local(LocalResult::Error(error)) => {
                    state.updating = false;
                    state.error = Some(error.message.clone());
                }
                ExecutionResult::Rejected(rejected) => {
                    state.updating = false;
                    state.error = Some(
                        rejected
                            .message
                            .clone()
                            .unwrap_or_else(|| "Source find was refused".into()),
                    );
                }
                ExecutionResult::ActionRequired { code, .. } => {
                    state.updating = false;
                    state.error = Some(format!("Source find needs attention: {code}"));
                }
                _ => {
                    state.updating = false;
                    state.error = Some("Source find result is stale; search again".into());
                }
            }
            return true;
        }
        if let Some((tab, epoch)) = self.preview_requests.remove(&completion.request_id) {
            let Some(state) = self.tabs.get_mut(&tab) else {
                return true;
            };
            if epoch != state.epoch {
                return true;
            }
            match &completion.result {
                ExecutionResult::Local(LocalResult::Success(success)) => {
                    let data = &success.data;
                    state.preview_id = data
                        .get("preview_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    state.plan =
                        data.get("plan")
                            .and_then(serde_json::Value::as_object)
                            .map(|plan| {
                                let scope = plan
                                    .get("scope_version")
                                    .and_then(serde_json::Value::as_object);
                                let totals =
                                    plan.get("totals").and_then(serde_json::Value::as_object);
                                PreviewPlanView {
                                    scope: (
                                        scope
                                            .and_then(|s| s.get("epoch"))
                                            .and_then(serde_json::Value::as_u64)
                                            .unwrap_or(0),
                                        scope
                                            .and_then(|s| s.get("generation"))
                                            .and_then(serde_json::Value::as_u64)
                                            .unwrap_or(0),
                                    ),
                                    matches: totals
                                        .and_then(|t| t.get("matches"))
                                        .and_then(serde_json::Value::as_u64)
                                        .unwrap_or(0),
                                    matched_notes: totals
                                        .and_then(|t| t.get("matched"))
                                        .and_then(serde_json::Value::as_u64)
                                        .unwrap_or(0),
                                    excluded: totals
                                        .and_then(|t| t.get("excluded"))
                                        .and_then(serde_json::Value::as_u64)
                                        .unwrap_or(0),
                                    drafts: totals
                                        .and_then(|t| t.get("drafts"))
                                        .and_then(serde_json::Value::as_u64)
                                        .unwrap_or(0),
                                    payload_bytes: totals
                                        .and_then(|t| t.get("payload_bytes"))
                                        .and_then(serde_json::Value::as_u64)
                                        .unwrap_or(0),
                                    expires_at_unix_ms: plan
                                        .get("expires_at_unix_ms")
                                        .and_then(serde_json::Value::as_u64)
                                        .unwrap_or(0),
                                    selection_revision: plan
                                        .get("selection_revision")
                                        .and_then(serde_json::Value::as_u64),
                                    selection_digest: plan
                                        .get("selection_digest")
                                        .and_then(serde_json::Value::as_str)
                                        .map(str::to_owned),
                                }
                            });
                    install_preview_page(state, data.get("page"));
                    state.updating = false;
                    state.error = None;
                }
                ExecutionResult::Local(LocalResult::Error(error)) => {
                    state.updating = false;
                    state.error = Some(error.message.clone());
                }
                ExecutionResult::Rejected(rejected) => {
                    state.updating = false;
                    state.error = Some(
                        rejected
                            .message
                            .clone()
                            .unwrap_or_else(|| "Preview was refused".into()),
                    );
                }
                ExecutionResult::ActionRequired { code, .. } => {
                    state.updating = false;
                    state.error = Some(format!("Preview needs attention: {code}"));
                }
                _ => {
                    state.updating = false;
                    state.error = Some("Preview result is stale; run it again".into());
                }
            }
            return true;
        }
        if let Some(tab) = self.job_requests.remove(&completion.request_id) {
            let Some(state) = self.tabs.get_mut(&tab) else {
                return true;
            };
            state.job_action_request = None;
            match &completion.result {
                ExecutionResult::Local(LocalResult::Success(success)) => {
                    let data = &success.data;
                    let status = data.get("status").cloned().unwrap_or_default();
                    state.apply_job_status(status);
                    let job_id = data
                        .get("job_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    if let Some(job) = &mut state.job
                        && let Some(job_id) = &job_id
                    {
                        job.job_id.clone_from(job_id);
                    }
                    state.error = None;
                }
                ExecutionResult::Local(LocalResult::Error(error)) => {
                    state.error = Some(error.message.clone());
                }
                ExecutionResult::Rejected(rejected) => {
                    state.error = Some(
                        rejected
                            .message
                            .clone()
                            .unwrap_or_else(|| "Replacement job was refused".into()),
                    );
                }
                ExecutionResult::ActionRequired { code, .. } => {
                    state.error = Some(format!("Replacement job needs attention: {code}"));
                }
                _ => {
                    state.error = Some("Replacement job result is stale".into());
                }
            }
            return true;
        }
        if let Some(tab) = self.select_requests.remove(&completion.request_id) {
            let Some(state) = self.tabs.get_mut(&tab) else {
                return true;
            };
            state.updating = false;
            match &completion.result {
                ExecutionResult::Local(LocalResult::Success(success)) => {
                    let data = &success.data;
                    let revision = data
                        .get("selection_revision")
                        .and_then(serde_json::Value::as_u64);
                    if let Some(plan) = &mut state.plan {
                        plan.selection_revision = revision;
                        plan.selection_digest = data
                            .get("selection_digest")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned);
                    }
                    state.selection_ready = revision.is_some();
                    if let Some(selection) = state.pending_selection.take() {
                        state.apply_selection(&selection);
                    }
                    state.error = None;
                }
                ExecutionResult::Local(LocalResult::Error(error)) => {
                    state.error = Some(error.message.clone());
                }
                ExecutionResult::Rejected(rejected) => {
                    state.error = Some(
                        rejected
                            .message
                            .clone()
                            .unwrap_or_else(|| "Selection was refused".into()),
                    );
                }
                _ => {
                    state.error = Some("Selection is stale; try again".into());
                }
            }
            return true;
        }
        false
    }

    /// Start the durable apply for this tab's sealed preview (RFC-0006
    /// S08).
    pub fn apply(&mut self, tab: flavite_workspace::TabId, flavite: Option<&flavite_api::Flavite>) {
        let Some(api) = flavite else {
            return;
        };
        let (preview_id, revision) = {
            let Some(state) = self.tabs.get_mut(&tab) else {
                return;
            };
            if !state.can_apply() {
                return;
            }
            let Some(preview_id) = state.preview_id.clone() else {
                return;
            };
            let revision = state
                .plan
                .as_ref()
                .and_then(|plan| plan.selection_revision)
                .unwrap_or(0);
            if revision == 0 {
                state.error = Some("Select notes before applying".into());
                return;
            }
            (preview_id, revision)
        };
        let idempotency_key = job_key();
        let request_id = api.command_registry().next_request_id();
        let result = api.execute(Invocation::new(
            request_id,
            "replace.apply",
            serde_json::json!({
                "preview_id": preview_id,
                "selection_revision": revision,
                "idempotency_key": idempotency_key,
            }),
            Origin::Gui,
        ));
        match result {
            ExecutionResult::Pending { request_id } => {
                self.job_requests.insert(request_id, tab);
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.job_action_request = Some(request_id);
                    state.error = None;
                }
            }
            ExecutionResult::Local(LocalResult::Success(success)) => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.apply_job_status(success.data.get("status").cloned().unwrap_or_default());
                    if let Some(job) = &mut state.job
                        && let Some(job_id) = success
                            .data
                            .get("job_id")
                            .and_then(serde_json::Value::as_str)
                    {
                        job.job_id.clone_from(&job_id.to_owned());
                    }
                }
            }
            ExecutionResult::Local(LocalResult::Error(error)) => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.error = Some(error.message);
                }
            }
            ExecutionResult::Rejected(rejected) => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.error = Some(
                        rejected
                            .message
                            .unwrap_or_else(|| "Replacement was refused".into()),
                    );
                }
            }
            ExecutionResult::ActionRequired { code, .. } => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.error = Some(format!("Replacement needs attention: {code}"));
                }
            }
            _ => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.error = Some("Replacement result is stale".into());
                }
            }
        }
    }

    /// Cancel the tab's durable job (stops admission of the next item).
    pub fn cancel_job(
        &mut self,
        tab: flavite_workspace::TabId,
        flavite: Option<&flavite_api::Flavite>,
    ) {
        self.submit_job_command(tab, flavite, "replace.cancel", "job_id");
    }

    /// Start a guarded Revert action epoch for the tab's job (RFC-0006
    /// S09/D-11). Available once the job finished (completed/partial/
    /// cancelled/paused) and it applied at least one item.
    pub fn revert_job(
        &mut self,
        tab: flavite_workspace::TabId,
        flavite: Option<&flavite_api::Flavite>,
    ) {
        let Some(api) = flavite else {
            return;
        };
        let job_id = {
            let Some(state) = self.tabs.get_mut(&tab) else {
                return;
            };
            if !state.can_revert() {
                return;
            }
            let Some(job_id) = state.job.as_ref().map(|job| job.job_id.clone()) else {
                return;
            };
            state.job_action_request = Some(u64::MAX);
            job_id
        };
        let idempotency_key = job_key();
        let request_id = api.command_registry().next_request_id();
        let result = api.execute(Invocation::new(
            request_id,
            "replace.revert",
            serde_json::json!({
                "job_id": job_id,
                "idempotency_key": idempotency_key,
            }),
            Origin::Gui,
        ));
        self.handle_job_response(tab, result);
    }

    /// Explicitly resume the tab's durable job.
    pub fn resume_job(
        &mut self,
        tab: flavite_workspace::TabId,
        flavite: Option<&flavite_api::Flavite>,
    ) {
        let Some(api) = flavite else {
            return;
        };
        let job_id = {
            let Some(state) = self.tabs.get_mut(&tab) else {
                return;
            };
            if !state.can_cancel() {
                return;
            }
            let Some(job_id) = state.job.as_ref().map(|job| job.job_id.clone()) else {
                return;
            };
            state.job_action_request = Some(u64::MAX);
            job_id
        };
        let idempotency_key = job_key();
        let request_id = api.command_registry().next_request_id();
        let result = api.execute(Invocation::new(
            request_id,
            "replace.resume",
            serde_json::json!({
                "job_id": job_id,
                "idempotency_key": idempotency_key,
            }),
            Origin::Gui,
        ));
        self.handle_job_response(tab, result);
    }

    fn submit_job_command(
        &mut self,
        tab: flavite_workspace::TabId,
        flavite: Option<&flavite_api::Flavite>,
        command: &'static str,
        key_field: &'static str,
    ) {
        let Some(api) = flavite else {
            return;
        };
        let job_id = {
            let Some(state) = self.tabs.get_mut(&tab) else {
                return;
            };
            if !state.can_cancel() {
                return;
            }
            let Some(job_id) = state.job.as_ref().map(|job| job.job_id.clone()) else {
                return;
            };
            state.job_action_request = Some(u64::MAX);
            job_id
        };
        let request_id = api.command_registry().next_request_id();
        let mut args = serde_json::Map::new();
        args.insert(key_field.to_owned(), serde_json::Value::String(job_id));
        let result = api.execute(Invocation::new(
            request_id,
            command,
            serde_json::Value::Object(args),
            Origin::Gui,
        ));
        self.handle_job_response(tab, result);
    }

    fn handle_job_response(&mut self, tab: flavite_workspace::TabId, result: ExecutionResult) {
        match result {
            ExecutionResult::Pending { request_id } => {
                self.job_requests.insert(request_id, tab);
            }
            ExecutionResult::Local(LocalResult::Success(success)) => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.job_action_request = None;
                    state.apply_job_status(success.data.get("status").cloned().unwrap_or_default());
                    let job_id = success
                        .data
                        .get("job_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    if let Some(job) = &mut state.job
                        && let Some(job_id) = job_id
                    {
                        job.job_id.clone_from(&job_id);
                    }
                    state.error = None;
                }
            }
            ExecutionResult::Local(LocalResult::Error(error)) => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.job_action_request = None;
                    state.error = Some(error.message);
                }
            }
            ExecutionResult::Rejected(rejected) => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.job_action_request = None;
                    state.error = Some(
                        rejected
                            .message
                            .unwrap_or_else(|| "Replacement job was refused".into()),
                    );
                }
            }
            ExecutionResult::ActionRequired { code, .. } => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.job_action_request = None;
                    state.error = Some(format!("Replacement job needs attention: {code}"));
                }
            }
            _ => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.job_action_request = None;
                    state.error = Some("Replacement job result is stale".into());
                }
            }
        }
    }
}

/// Which edited field changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceField {
    Query,
    Replacement,
}

/// Which boolean option toggled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceFlag {
    Regex,
    CaseSensitive,
    WholeWord,
    /// Opt-in draft inclusion (RFC-0006 S09/D-09).
    IncludeDrafts,
}

/// Selection actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectAction {
    All,
    None,
    Keys(Vec<String>),
}

/// Submit a best-effort session close command.
/// A caller-generated 128-bit hex idempotency key (RFC-0006 S08).
fn job_key() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:032x}", now.as_nanos() ^ (std::process::id() as u128))
}

fn submit_close(api: &flavite_api::Flavite, command: &'static str, id: String) {
    let request_id = api.command_registry().next_request_id();
    let _ = api.execute(Invocation::new(
        request_id,
        command,
        serde_json::json!({ "finder_id": id }),
        Origin::Gui,
    ));
}

/// Install a find page (append-once by ordinal; replay never duplicates).
fn install_find_page(state: &mut ReplaceTabState, page: Option<&serde_json::Value>) {
    let Some(page) = page else {
        return;
    };
    let rows: Vec<FindRowView> = page
        .get("rows")
        .and_then(serde_json::Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some(FindRowView {
                        ordinal: row.get("ordinal")?.as_u64()?,
                        path: row.get("path")?.as_str()?.to_owned(),
                        title: row.get("title")?.as_str()?.to_owned(),
                        note_matches: row.get("note_matches")?.as_u64()?,
                        matches: row
                            .get("matches")?
                            .as_array()?
                            .iter()
                            .filter_map(|pair| {
                                Some((
                                    pair.as_array()?.first()?.as_u64()?,
                                    pair.as_array()?.get(1)?.as_u64()?,
                                ))
                            })
                            .collect(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let first_ordinal = rows.first().map(|row| row.ordinal);
    if let Some(ordinal) = first_ordinal {
        // Append once: if the first new row's ordinal already exists, this
        // is a replayed page.
        if state
            .rows
            .last()
            .is_some_and(|last| last.ordinal >= ordinal)
        {
            return;
        }
    }
    state.rows.extend(rows);
    state.next_note_index = page
        .get("next_note_index")
        .and_then(serde_json::Value::as_u64);
    state.next_match_offset = page
        .get("next_match_offset")
        .and_then(serde_json::Value::as_u64);
}

/// Install a preview candidate page and seed selection defaults (unselected).
fn install_preview_page(state: &mut ReplaceTabState, page: Option<&serde_json::Value>) {
    let Some(page) = page else {
        return;
    };
    let candidates: Vec<CandidateView> = page
        .get("rows")
        .and_then(serde_json::Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some(CandidateView {
                        item_id: row.get("item_id")?.as_u64()?,
                        path: row.get("path")?.as_str()?.to_owned(),
                        path_native: row.get("path_native")?.as_str()?.to_owned(),
                        match_count: row.get("match_count")?.as_u64()?,
                        output_bytes: row.get("output_bytes")?.as_u64()?,
                        no_change: row.get("no_change")?.as_bool()?,
                        before_hash: row.get("before")?.get("hash")?.as_str()?.to_owned(),
                        after_hash: row.get("after")?.get("hash")?.as_str()?.to_owned(),
                        source_kind: row.get("source_kind")?.as_str()?.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    for candidate in &candidates {
        if !state.selected.contains_key(&candidate.path_native) {
            state.selected.insert(candidate.path_native.clone(), false);
        }
    }
    state.candidates = candidates;
    state.selection_ready = state.selection_ready
        || state
            .plan
            .as_ref()
            .is_some_and(|plan| plan.selection_revision.is_some());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion_for(
        command: &str,
        data: serde_json::Value,
        request_id: u64,
    ) -> OperationCompletion {
        OperationCompletion {
            request_id,
            result: ExecutionResult::Local(LocalResult::Success(
                flavite_api::LocalSuccess::with_command(request_id, command, data),
            )),
        }
    }

    #[test]
    fn find_page_appends_once_and_never_duplicates() {
        let mut host = ReplaceHost::default();
        let tab = flavite_workspace::TabId::from_raw(1);
        host.tabs.insert(tab, ReplaceTabState::default());
        let data = serde_json::json!({
            "finder_id": "a".repeat(32),
            "snapshot": { "epoch": 1, "generation": 2 },
            "total_notes": 2,
            "total_matches": 3,
            "truncated_scope": false,
            "exclusions": [ { "path": "d.md", "reason": "dirty_buffer" } ],
            "page": {
                "rows": [
                    { "ordinal": 0, "path": "a.md", "title": "A", "note_matches": 2,
                      "matches": [[0,1],[2,3]] }
                ],
                "next_note_index": 1,
                "next_match_offset": null
            }
        });
        host.find_requests.insert(7, (tab, 0));
        assert!(host.observe(&completion_for("source_find.start", data.clone(), 7)));
        let state = &host.tabs[&tab];
        assert_eq!(state.rows.len(), 1);
        assert_eq!(state.total_matches, Some(3));
        assert_eq!(
            state.exclusions,
            vec![("d.md".to_owned(), "dirty_buffer".to_owned())]
        );
        assert!(state.has_next());
        // Replay of the same completion must not duplicate rows.
        host.find_requests.insert(7, (tab, state.epoch));
        assert!(host.observe(&completion_for("source_find.start", data, 7)));
        assert_eq!(host.tabs[&tab].rows.len(), 1);
    }

    #[test]
    fn stale_epoch_outcomes_are_dropped() {
        let mut host = ReplaceHost::default();
        let tab = flavite_workspace::TabId::from_raw(1);
        host.tabs.insert(tab, ReplaceTabState::default());
        host.tabs.get_mut(&tab).unwrap().epoch = 9;
        host.find_requests.insert(5, (tab, 4));
        let data = serde_json::json!({
            "finder_id": "a".repeat(32),
            "snapshot": { "epoch": 1, "generation": 1 },
            "total_notes": 1,
            "total_matches": 0,
            "truncated_scope": false,
            "exclusions": [],
            "page": { "rows": [], "next_note_index": null, "next_match_offset": null }
        });
        assert!(host.observe(&completion_for("source_find.start", data, 5)));
        assert!(host.tabs[&tab].rows.is_empty());
        assert!(host.tabs[&tab].finder.is_none());
    }
}
