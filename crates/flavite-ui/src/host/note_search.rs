//! Literal note Find/Replace host (RFC-0006 S01).
//!
//! One host owns the per-view find state, the revision-keyed match pages,
//! and the replacement flow through the core: every replacement prepares a
//! plan (`text.prepare_replace`) bound to the exact buffer revision and
//! installs it (`text.apply_replace`) as one isolated, revision-guarded
//! Undo unit. The host never writes text itself; stale or expired plans
//! surface as user-visible errors without effects.
//!
//! S01 keeps the find-side match computation as a bounded host-local
//! leftmost scan over the open buffer snapshot (the API exposes no
//! find-spans command yet and the UI must not reach into the
//! source-search crate). S03 replaces this scan with the
//! flavite-text-search matcher over the worker protocol; until then the
//! loop below preserves the same leftmost, non-overlapping walk the
//! retired `str::match_indices` scan performed, with per-character simple
//! case folding for the default case-insensitive mode (D-05).

use crate::app::App;
use flavite_api::{ExecutionResult, Invocation, LocalResult, OperationCompletion, Origin};
use flavite_workspace::TabId;
use std::collections::BTreeMap;

/// Maximum spans kept per find page; the UI reports the cap honestly.
pub const MATCH_CAP: usize = 10_000;
/// S01 literal matching is case-insensitive by default (D-05).
const CASE_SENSITIVE: bool = false;
/// Typed find/replace inputs are bounded like every other host draft.
const INPUT_LIMIT: usize = 8192;
/// Shown when a prepared plan can no longer apply (stale/expired).
const STALE_MESSAGE: &str = "Source changed — search again";

/// Per-view find bar state (the shape the reading host used to own).
#[derive(Debug, Default, Clone)]
pub struct FindState {
    pub focused: bool,
    pub query: String,
    pub index: usize,
}

/// Per-view replace row state. `pending` holds the request id of the
/// in-flight `text.prepare_replace`; `match_count`/`plan_id` record the
/// last prepared plan; `last_error` carries the stale/expired refusal.
#[derive(Debug, Default, Clone)]
pub struct ReplaceState {
    pub focused: bool,
    pub query: String,
    pub replacement: String,
    pub pending: Option<u64>,
    pub last_error: Option<String>,
    pub match_count: Option<u64>,
    pub plan_id: Option<String>,
}

/// One cached match page, valid for exactly this buffer revision, query,
/// and matching mode. Spans are half-open byte ranges into the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedMatches {
    pub revision: u64,
    pub query: String,
    pub case_sensitive: bool,
    pub spans: Vec<(usize, usize)>,
    /// The scan stopped at [`MATCH_CAP`] because a further match exists.
    pub truncated: bool,
}

/// Host state: find bars, replace rows, cached match pages, and the
/// request-id to tab mapping for in-flight replacement preparations.
#[derive(Debug, Default)]
pub struct NoteSearchHost {
    pub finds: BTreeMap<TabId, FindState>,
    pub replaces: BTreeMap<TabId, ReplaceState>,
    /// `text.prepare_replace` request ids awaiting completion, per tab.
    pub preparations: BTreeMap<u64, TabId>,
    pub matches: BTreeMap<TabId, CachedMatches>,
}

impl NoteSearchHost {
    fn clear_tab(&mut self, tab: &TabId) {
        self.finds.remove(tab);
        self.replaces.remove(tab);
        self.matches.remove(tab);
        self.preparations.retain(|_, owner| owner != tab);
    }

    fn clear_replace(&mut self, tab: &TabId) {
        self.replaces.remove(tab);
        self.preparations.retain(|_, owner| owner != tab);
    }
}

/// Messages for the note search host. The legacy `Message::Find*` variants
/// map onto the `Find*` variants here at the routing site.
#[derive(Debug, Clone)]
pub enum NoteSearchMessage {
    FindOpen(TabId),
    FindClose(TabId),
    FindQuery { tab: TabId, query: String },
    FindNext { tab: TabId, forward: bool },
    ReplaceOpen(TabId),
    ReplaceClose(TabId),
    ReplaceQuery { tab: TabId, query: String },
    ReplaceText { tab: TabId, text: String },
    ReplaceNext(TabId),
    ReplaceAll(TabId),
}

/// Handle one note search message.
pub fn handle(app: &mut App, message: NoteSearchMessage) -> iced::Task<crate::app::Message> {
    match message {
        NoteSearchMessage::FindOpen(tab) => {
            // The editor widget yields keyboard focus to the find input by
            // acknowledging a blurred interaction first (as before S01).
            if crate::host::reading::is_edit(app, tab)
                && let Some(view) = app.note_editor_view(tab)
                && let Some(session) = app.workspace.tab(&tab)
            {
                let mut input = session.interaction().clone();
                input.buffer_id = Some(view.buffer_id);
                input.revision = view.revision;
                input.sequence = input.sequence.saturating_add(1);
                input.focused = false;
                let _ = app.workspace.acknowledge_interaction(input, &view.text);
            }
            app.note_search.finds.entry(tab).or_default().focused = true;
            app.refresh_semantics();
            return iced::widget::operation::focus(find_id(tab));
        }
        NoteSearchMessage::FindClose(tab) => {
            // Closing the find bar closes the replace row with it: the
            // replacement acts on the find query, and any in-flight
            // preparation is dropped without effects.
            app.note_search.clear_tab(&tab);
        }
        NoteSearchMessage::FindQuery { tab, query } => {
            if query.len() <= INPUT_LIMIT
                && let Some(find) = app.note_search.finds.get_mut(&tab)
            {
                find.focused = true;
                find.query = query.clone();
                find.index = 0;
                if let Some(replace) = app.note_search.replaces.get_mut(&tab) {
                    replace.query = query;
                    replace.match_count = None;
                    replace.plan_id = None;
                    replace.last_error = None;
                }
                navigate(app, tab);
            }
        }
        NoteSearchMessage::FindNext { tab, forward } => {
            refresh_matches(app, tab);
            let count = app
                .note_search
                .matches
                .get(&tab)
                .map_or(0, |cached| cached.spans.len());
            if count > 0
                && let Some(find) = app.note_search.finds.get_mut(&tab)
            {
                find.index = if forward {
                    (find.index + 1) % count
                } else {
                    (find.index + count - 1) % count
                };
                navigate(app, tab);
            }
        }
        NoteSearchMessage::ReplaceOpen(tab) => {
            if crate::host::reading::is_edit(app, tab)
                && let Some(view) = app.note_editor_view(tab)
                && let Some(session) = app.workspace.tab(&tab)
            {
                let mut input = session.interaction().clone();
                input.buffer_id = Some(view.buffer_id);
                input.revision = view.revision;
                input.sequence = input.sequence.saturating_add(1);
                input.focused = false;
                let _ = app.workspace.acknowledge_interaction(input, &view.text);
            }
            // The replace row rides on the find row and inherits its query,
            // so both rows always act on the same visible query.
            let query = app.note_search.finds.entry(tab).or_default().query.clone();
            let replace = app.note_search.replaces.entry(tab).or_default();
            replace.focused = true;
            replace.query = query;
            replace.last_error = None;
            app.refresh_semantics();
            return iced::widget::operation::focus(replace_id(tab));
        }
        NoteSearchMessage::ReplaceClose(tab) => {
            app.note_search.clear_replace(&tab);
        }
        NoteSearchMessage::ReplaceQuery { tab, query } => {
            if query.len() <= INPUT_LIMIT
                && let Some(replace) = app.note_search.replaces.get_mut(&tab)
            {
                replace.focused = true;
                replace.query = query.clone();
                replace.match_count = None;
                replace.plan_id = None;
                replace.last_error = None;
                if let Some(find) = app.note_search.finds.get_mut(&tab) {
                    find.query = query;
                    find.index = 0;
                }
                navigate(app, tab);
            }
        }
        NoteSearchMessage::ReplaceText { tab, text } => {
            if text.len() <= INPUT_LIMIT
                && let Some(replace) = app.note_search.replaces.get_mut(&tab)
            {
                replace.replacement = text;
                replace.last_error = None;
            }
        }
        NoteSearchMessage::ReplaceNext(tab) => prepare(app, tab, false),
        NoteSearchMessage::ReplaceAll(tab) => prepare(app, tab, true),
    }
    app.refresh_semantics();
    iced::Task::none()
}

/// The find page for a tab, refreshed against the live view. Read-only
/// callers (views, harness) get the stored page when it still describes
/// the current (revision, query, mode) and otherwise a fresh scan; the
/// stored page itself refreshes through message handling.
#[must_use]
pub fn matches(app: &App, tab: TabId) -> Option<CachedMatches> {
    let find = app.note_search.finds.get(&tab)?;
    let view = app.note_editor_view(tab)?;
    if let Some(cached) = app.note_search.matches.get(&tab)
        && cached.revision == view.revision
        && cached.query == find.query
        && cached.case_sensitive == CASE_SENSITIVE
    {
        return Some(cached.clone());
    }
    let (spans, truncated) = scan(&view.text, &find.query, CASE_SENSITIVE);
    Some(CachedMatches {
        revision: view.revision,
        query: find.query.clone(),
        case_sensitive: CASE_SENSITIVE,
        spans,
        truncated,
    })
}

/// Recompute and store the match page for the tab's current find state.
fn refresh_matches(app: &mut App, tab: TabId) {
    let Some(view) = app.note_editor_view(tab) else {
        app.note_search.matches.remove(&tab);
        return;
    };
    let Some(query) = app
        .note_search
        .finds
        .get(&tab)
        .map(|find| find.query.clone())
    else {
        return;
    };
    let (spans, truncated) = scan(&view.text, &query, CASE_SENSITIVE);
    app.note_search.matches.insert(
        tab,
        CachedMatches {
            revision: view.revision,
            query,
            case_sensitive: CASE_SENSITIVE,
            spans,
            truncated,
        },
    );
}

/// Navigation selects a display range through the workspace (never an
/// editor write) and records a content anchor for read-mode scrolling.
fn navigate(app: &mut App, tab: TabId) {
    refresh_matches(app, tab);
    let Some(view) = app.note_editor_view(tab) else {
        return;
    };
    let Some(find) = app.note_search.finds.get(&tab) else {
        return;
    };
    let Some(cached) = app.note_search.matches.get(&tab) else {
        return;
    };
    if cached.spans.is_empty() {
        return;
    }
    let (at, end) = cached.spans[find.index % cached.spans.len()];
    let _ = app
        .workspace
        .set_display_selection(tab, at, end, view.revision, &view.text);
    app.reading.pending_anchor.insert(tab, at);
}

/// Prepare one replacement plan: Replace next scopes the plan to the
/// match at/after the caret; Replace all prepares over the whole note.
/// The plan applies on completion (see [`completion`]).
fn prepare(app: &mut App, tab: TabId, all: bool) {
    let Some(state) = app.note_search.replaces.get(&tab) else {
        return;
    };
    if state.pending.is_some() {
        return; // one in-flight replacement per view
    }
    let Some(view) = app.note_editor_view(tab) else {
        return;
    };
    let Some(query) = app
        .note_search
        .finds
        .get(&tab)
        .map(|find| find.query.clone())
    else {
        return;
    };
    if query.is_empty() {
        app.status = "Type text to find before replacing".into();
        return;
    }
    let replacement = state.replacement.clone();
    refresh_matches(app, tab);
    let mut scope: Option<(usize, usize)> = None;
    if !all {
        let caret = app
            .workspace
            .tab(&tab)
            .and_then(|session| session.interaction().selections.first().copied())
            .map_or(0, |selection| selection.anchor.min(selection.head));
        let cached = app.note_search.matches.get(&tab);
        let next = cached
            .and_then(|cached| {
                cached
                    .spans
                    .iter()
                    .copied()
                    .find(|(start, _)| *start >= caret)
            })
            .or_else(|| cached.and_then(|cached| cached.spans.first().copied()));
        let Some(span) = next else {
            app.status = format!("No matches for \"{query}\"");
            return;
        };
        scope = Some(span);
    }
    let Some(api) = app.flavite.as_ref() else {
        return;
    };
    let request_id = api.command_registry().next_request_id();
    let result = api.execute(Invocation::new(
        request_id,
        "text.prepare_replace",
        serde_json::json!({
            "buffer_id": view.buffer_id,
            "revision": view.revision,
            "query": query,
            "replacement": replacement,
            "case_sensitive": CASE_SENSITIVE,
            "whole_word": false,
            "scope": scope,
        }),
        Origin::Gui,
    ));
    match result {
        ExecutionResult::Pending { request_id } => {
            app.operations.accept(request_id, "note.replace");
            app.note_search.preparations.insert(request_id, tab);
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.pending = Some(request_id);
                state.query = query;
                state.last_error = None;
            }
            app.status = "Preparing replacement…".into();
        }
        ExecutionResult::Local(LocalResult::Success(success)) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.query = query;
            }
            apply_plan(app, tab, &success.data);
        }
        ExecutionResult::Rejected(rejected) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(rejection_message(
                    rejected.reason == flavite_api::RejectionReason::StaleRevision,
                    rejected.message.as_deref(),
                ));
            }
            app.status = "Replacement was refused".into();
        }
        ExecutionResult::ActionRequired { code, .. } => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(plan_error(&code));
            }
            app.status = format!("Replacement needs attention: {code}");
        }
        ExecutionResult::Local(LocalResult::Error(error)) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(error.message.clone());
            }
            app.status = "Replacement failed".into();
        }
        ExecutionResult::Superseded { .. } | ExecutionResult::Local(LocalResult::Void) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some("The replacement was not prepared; search again".into());
            }
        }
    }
}

/// Install one prepared plan through the core: revision-guarded, one
/// isolated Undo unit. Failures record the user-visible repair without
/// any effects.
fn apply_plan(app: &mut App, tab: TabId, prepared: &serde_json::Value) {
    let Some(plan_id) = prepared
        .get("plan_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
    else {
        if let Some(state) = app.note_search.replaces.get_mut(&tab) {
            state.last_error = Some("The replacement plan was malformed; search again".into());
        }
        return;
    };
    let match_count = prepared
        .get("match_count")
        .and_then(serde_json::Value::as_u64);
    let Some(view) = app.note_editor_view(tab) else {
        // The tab lost its buffer while the plan was in flight.
        if let Some(state) = app.note_search.replaces.get_mut(&tab) {
            state.last_error = Some(STALE_MESSAGE.to_owned());
        }
        return;
    };
    let Some(api) = app.flavite.as_ref() else {
        return;
    };
    let result = api.execute(Invocation::new(
        api.command_registry().next_request_id(),
        "text.apply_replace",
        serde_json::json!({
            "buffer_id": view.buffer_id,
            "revision": view.revision,
            "plan_id": plan_id,
        }),
        Origin::Gui,
    ));
    let replaced = match_count.unwrap_or(0);
    match result {
        ExecutionResult::Local(LocalResult::Success(_)) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.plan_id = Some(plan_id);
                state.match_count = match_count;
                state.last_error = None;
            }
            app.workspace.invalidate_writing_target(view.buffer_id);
            app.linking.invalidate_if_source_changed(None);
            app.contexts.invalidate_all();
            app.reading.notes.remove(&tab);
            app.status = format!("Replaced {replaced} · unsaved · Undo restores it");
            app.reconcile_contexts();
            crate::host::reading::sync(app);
        }
        ExecutionResult::Rejected(rejected) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(rejection_message(
                    rejected.reason == flavite_api::RejectionReason::StaleRevision,
                    rejected.message.as_deref(),
                ));
            }
            app.status = "Replacement was refused".into();
        }
        ExecutionResult::ActionRequired { code, .. } => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(plan_error(&code));
            }
            app.status = format!("Replacement needs attention: {code}");
        }
        ExecutionResult::Local(LocalResult::Error(error)) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(error.message.clone());
            }
            app.status = "Replacement failed".into();
        }
        ExecutionResult::Superseded { .. } | ExecutionResult::Local(LocalResult::Void) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some("The replacement was not applied; search again".into());
            }
        }
        ExecutionResult::Pending { .. } => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some("The replacement is still applying".into());
            }
        }
    }
}

/// Route one drained completion for a replacement preparation. Returns
/// true when the completion belonged to this host: the plan applies to
/// the still-mounted view, or is dropped without effects when the row
/// closed or the tab disappeared while the worker ran.
pub fn completion(app: &mut App, completion: &OperationCompletion) -> bool {
    let Some(&tab) = app.note_search.preparations.get(&completion.request_id) else {
        return false;
    };
    app.note_search.preparations.remove(&completion.request_id);
    if app.workspace.tab(&tab).is_none() {
        // The tab closed while the worker ran: drop the stale completion
        // without applying anything to the closed buffer.
        return true;
    }
    if app
        .note_search
        .replaces
        .get(&tab)
        .is_none_or(|state| state.pending != Some(completion.request_id))
    {
        return true;
    }
    if let Some(state) = app.note_search.replaces.get_mut(&tab) {
        state.pending = None;
    }
    match &completion.result {
        ExecutionResult::Local(LocalResult::Success(success)) => {
            apply_plan(app, tab, &success.data)
        }
        ExecutionResult::Rejected(rejected) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(rejection_message(
                    rejected.reason == flavite_api::RejectionReason::StaleRevision,
                    rejected.message.as_deref(),
                ));
            }
            app.status = "Replacement was refused".into();
        }
        ExecutionResult::ActionRequired { code, .. } => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(plan_error(code));
            }
            app.status = format!("Replacement needs attention: {code}");
        }
        ExecutionResult::Local(LocalResult::Error(error)) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(error.message.clone());
            }
            app.status = "Replacement failed".into();
        }
        _ => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some("The replacement was not prepared; search again".into());
            }
        }
    }
    true
}

/// Retain host state only for live tabs; a closed tab drops its find bar,
/// replace row, cached page, and any in-flight preparation mapping.
pub fn sync(app: &mut App) {
    app.note_search
        .finds
        .retain(|tab, _| app.workspace.tab(tab).is_some());
    app.note_search
        .replaces
        .retain(|tab, _| app.workspace.tab(tab).is_some());
    app.note_search
        .matches
        .retain(|tab, _| app.workspace.tab(tab).is_some());
    app.note_search
        .preparations
        .retain(|_, tab| app.workspace.tab(tab).is_some());
}

/// Whether a find or replace input owns typing: the editor keyboard is
/// disabled while either is focused.
#[must_use]
pub fn blocks_typing(app: &App, tab: &TabId) -> bool {
    app.note_search
        .finds
        .get(tab)
        .is_some_and(|find| find.focused)
        || app
            .note_search
            .replaces
            .get(tab)
            .is_some_and(|replace| replace.focused)
}

/// The mounted find input id for a tab.
#[must_use]
pub fn find_id(tab: TabId) -> iced::widget::Id {
    iced::widget::Id::from(format!("find-current-{}", tab.raw()))
}

/// The mounted replacement input id for a tab.
#[must_use]
pub fn replace_id(tab: TabId) -> iced::widget::Id {
    iced::widget::Id::from(format!("replace-current-{}", tab.raw()))
}

fn rejection_message(stale: bool, message: Option<&str>) -> String {
    if stale {
        STALE_MESSAGE.to_owned()
    } else {
        format!("Replacement refused: {}", message.unwrap_or("no details"))
    }
}

fn plan_error(code: &str) -> String {
    if code == "text-plan-expired" {
        format!("{STALE_MESSAGE} (the prepared plan expired)")
    } else {
        format!("Replacement needs attention ({code})")
    }
}

/// One bounded leftmost literal scan over the snapshot text. The loop
/// preserves the leftmost, non-overlapping walk of the retired
/// `str::match_indices` scan; spans are recorded in original text bytes
/// (case folding can change byte widths, so span ends are never derived
/// from the query length). Also drives the read view's find highlights so
/// both surfaces fold identically.
pub(crate) fn scan(text: &str, query: &str, case_sensitive: bool) -> (Vec<(usize, usize)>, bool) {
    let mut spans = Vec::new();
    if query.is_empty() {
        return (spans, false);
    }
    if case_sensitive {
        let mut start = 0;
        while let Some(at) = text[start..].find(query) {
            let at = start + at;
            let end = at + query.len();
            if spans.len() == MATCH_CAP {
                return (spans, true);
            }
            spans.push((at, end));
            start = end;
        }
        return (spans, false);
    }
    let query_chars: Vec<char> = query.chars().collect();
    let mut start = 0;
    while start < text.len() {
        let mut at = start;
        let mut matched = true;
        for &expected in &query_chars {
            let Some(actual) = text[at..].chars().next() else {
                matched = false;
                break;
            };
            if !fold_eq(actual, expected) {
                matched = false;
                break;
            }
            at += actual.len_utf8();
        }
        if matched {
            if spans.len() == MATCH_CAP {
                return (spans, true);
            }
            spans.push((start, at));
            start = at;
        } else {
            start += text[start..].chars().next().map_or(1, char::len_utf8);
        }
    }
    (spans, false)
}

/// Simple case folding equality for one character pair.
fn fold_eq(actual: char, expected: char) -> bool {
    let mut actual_folded = actual.to_lowercase();
    let mut expected_folded = expected.to_lowercase();
    loop {
        match (actual_folded.next(), expected_folded.next()) {
            (None, None) => return true,
            (Some(a), Some(b)) if a == b => continue,
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_is_leftmost_non_overlapping_in_byte_offsets() {
        let (spans, truncated) = scan("ab ab ab", "ab", true);
        assert_eq!(spans, vec![(0, 2), (3, 5), (6, 8)]);
        assert!(!truncated);
        // Overlapping candidates never repeat: "aa" in "aaa" matches once.
        let (spans, _) = scan("aaa", "aa", true);
        assert_eq!(spans, vec![(0, 2)]);
        // Non-overlap advance is by match length, not one char.
        let (spans, _) = scan("aaaa", "aa", true);
        assert_eq!(spans, vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn scan_folds_case_and_keeps_original_byte_widths() {
        // Simple folding folds each char; ß never equals "ss" (D-05). The
        // second span starts at byte 9: "Straße" is 7 bytes (ß is 2), so
        // span ends are original text bytes, not query-derived.
        let (spans, _) = scan("Straße STRASSE", "TRA", false);
        assert_eq!(spans, vec![(1, 4), (9, 12)]);
        let (spans, _) = scan("Straße", "strasse", false);
        assert!(spans.is_empty(), "ß does not fold to ss");
        let (spans, _) = scan("Héllo héllo", "HÉLLO", false);
        assert_eq!(spans, vec![(0, 6), (7, 13)], "é is 2 bytes");
        // Composed vs decomposed accents stay distinct (no normalization).
        let (spans, _) = scan("e\u{301}l", "é", false);
        assert!(spans.is_empty());
    }

    #[test]
    fn scan_reports_truncation_honestly_at_the_cap() {
        let text = "x".repeat(MATCH_CAP + 1);
        let (spans, truncated) = scan(&text, "x", true);
        assert_eq!(spans.len(), MATCH_CAP);
        assert!(truncated);
        let text = "x".repeat(MATCH_CAP);
        let (spans, truncated) = scan(&text, "x", true);
        assert_eq!(spans.len(), MATCH_CAP);
        assert!(!truncated, "exactly the cap is not a truncation");
    }

    #[test]
    fn scan_empty_query_is_idle() {
        let (spans, truncated) = scan("anything", "", true);
        assert!(spans.is_empty());
        assert!(!truncated);
    }
}
