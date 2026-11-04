//! Close plans (RFC-0003 W12, D-10): explicit decisions, per-buffer final
//! lease accounting, and no view removal before a safety outcome.
//!
//! The core owns the close policy (`note.close` refuses a pending save or
//! an unsaved buffer unless a durable recovery snapshot provably covers
//! it); this module sequences the user's decisions through that policy and
//! never removes a tab before the outcome succeeds.

use crate::app::{App, Message};
use crate::host::dialogs::Dialog;
use flavite_api::BufferStatus as BufferState;
use flavite_api::ExecutionResult;
use flavite_workspace::{LayoutAction, TabId};
use iced::Task;

/// One captured close decision.
#[derive(Debug, Clone)]
pub enum CloseDecision {
    /// Save the document, then close when the save terminal succeeds.
    SaveClose,
    /// Close while durably retaining the unsaved edits as recovery
    /// (one core transaction: flush, verify, drop the buffer).
    RetainRecovery,
    /// Explicit destructive discard of the unsaved edits, bound to the
    /// revision the confirmation was armed against.
    Discard { expected_revision: u64 },
}

/// Where a close plan came from; affects status wording only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseOrigin {
    /// One tab.
    Single,
    /// A whole group.
    Group,
    /// Application quit.
    Quit,
}

/// An in-flight close decision for one tab. Keyed by tab id, so a decision
/// survives tab movement (FR-15); the captured revision invalidates the
/// plan when the document changes underneath it.
#[derive(Debug, Clone)]
pub struct ClosePlan {
    /// The tab being closed.
    pub tab: TabId,
    /// Display path of the document.
    pub path: String,
    /// Why this close started.
    pub origin: CloseOrigin,
    /// The buffer revision captured when the plan was created.
    pub captured_revision: u64,
    /// The destructive-discards confirmation, armed with a revision.
    pub discard_armed: Option<u64>,
    /// The in-flight core request driving the decision, if any.
    pub request: Option<u64>,
}

impl ClosePlan {
    /// Capture a plan for a tab's current state.
    #[must_use]
    pub fn capture(app: &App, tab: TabId, origin: CloseOrigin) -> Option<Self> {
        let state = app.notes.state(&tab)?;
        let view = app.note_editor_view(tab)?;
        Some(Self {
            tab,
            path: state.locator.to_path(app.root()).display().to_string(),
            origin,
            captured_revision: view.revision,
            discard_armed: None,
            request: None,
        })
    }
}

impl App {
    /// Whether the tab's document is at its final view with unsaved state
    /// (dirty, conflicted, or orphaned): the case that requires a decision.
    #[must_use]
    pub fn close_needs_decision(&self, tab: TabId) -> bool {
        if self.notes.other_leases(tab) > 0 {
            return false;
        }
        matches!(
            self.note_buffer_state(tab),
            Some(BufferState::Dirty) | Some(BufferState::Conflicted) | Some(BufferState::Orphaned)
        )
    }

    /// The user-visible unsaved label for the close dialog.
    #[must_use]
    pub fn close_state_label(&self, tab: TabId) -> &'static str {
        match self.note_buffer_state(tab) {
            Some(BufferState::Dirty) => "Unsaved changes",
            Some(BufferState::Conflicted) => "Conflict — the file changed on disk",
            Some(BufferState::Orphaned) => "Missing on disk — unsaved edits and recovery retained",
            _ => "Saved",
        }
    }

    /// Begin a close for one tab (W12): decide or close.
    pub fn request_close_tab(&mut self, tab: TabId) -> Task<Message> {
        if self.close_needs_decision(tab) {
            match ClosePlan::capture(self, tab, CloseOrigin::Single) {
                Some(plan) => {
                    self.status = format!("Closing {} needs a decision", plan.path);
                    self.dialog = Some(Dialog::Close { tab });
                    self.closing = Some(plan);
                }
                None => self.status = "This view can no longer be inspected; close refused".into(),
            }
            return Task::none();
        }
        self.complete_safe_close(tab, CloseOrigin::Single)
    }

    /// Begin a close for a whole group: immediately closable tabs close,
    /// decision-required tabs queue; one refusal retains the undecided
    /// tabs without undoing completed closes (§7.4).
    pub fn request_close_group(&mut self, group: flavite_workspace::GroupId) -> Task<Message> {
        let Some(g) = self.workspace.group(&group) else {
            return Task::none();
        };
        let tabs: Vec<TabId> = g.tabs().to_vec();
        let mut queue = Vec::new();
        let mut closed = 0usize;
        for tab in tabs {
            if self.close_needs_decision(tab) {
                queue.push(tab);
            } else {
                let _ = self.complete_safe_close(tab, CloseOrigin::Group);
                closed += 1;
            }
        }
        if queue.is_empty() {
            self.status = format!(
                "Group closed ({closed} tab{})",
                if closed == 1 { "" } else { "s" }
            );
            return Task::none();
        }
        self.status = format!(
            "Group close: {closed} tab(s) closed, {} need decisions",
            queue.len()
        );
        self.close_queue = queue[1..].to_vec();
        let first = queue[0];
        if let Some(plan) = ClosePlan::capture(self, first, CloseOrigin::Group) {
            self.closing = Some(plan);
            self.dialog = Some(Dialog::Close { tab: first });
        }
        Task::none()
    }

    /// Run the safety outcome for a tab that needs no decision (§8.3):
    /// a final clean view closes its Session buffer through the core
    /// policy; shared views only release their lease.
    pub fn complete_safe_close(&mut self, tab: TabId, origin: CloseOrigin) -> Task<Message> {
        if self.notes.other_leases(tab) > 0 {
            self.dispatch_layout(|revision| LayoutAction::CloseTab {
                expected_revision: revision,
                tab,
            });
            return Task::none();
        }
        let Some(state) = self.notes.state(&tab) else {
            // Failed/reserved views have no document to close.
            self.dispatch_layout(|revision| LayoutAction::CloseTab {
                expected_revision: revision,
                tab,
            });
            return Task::none();
        };
        let path = state.locator.to_path(self.root());
        let unsaved = matches!(
            self.note_buffer_state(tab),
            Some(BufferState::Dirty) | Some(BufferState::Conflicted) | Some(BufferState::Orphaned)
        );
        if unsaved {
            // Races between classification and execution are decided by
            // the core policy, never by the UI.
            match ClosePlan::capture(self, tab, origin) {
                Some(plan) => {
                    self.closing = Some(plan);
                    self.dialog = Some(Dialog::Close { tab });
                }
                None => self.status = "This view can no longer be inspected; close refused".into(),
            }
            return Task::none();
        }
        let Some(flavite) = self.flavite.as_mut() else {
            self.dispatch_layout(|revision| LayoutAction::CloseTab {
                expected_revision: revision,
                tab,
            });
            return Task::none();
        };
        let request_id = flavite.command_registry().next_request_id();
        let invocation = flavite_api::Invocation::new(
            request_id,
            "note.close",
            serde_json::json!({ "path": path, "policy": "clean" }),
            flavite_api::Origin::Gui,
        );
        match flavite.execute(invocation) {
            flavite_api::ExecutionResult::Local(local) => {
                if local.is_success() {
                    // The buffer is gone; the tab follows (§8.3 order).
                    self.notes.close_document(tab);
                    self.dispatch_layout(|revision| LayoutAction::CloseTab {
                        expected_revision: revision,
                        tab,
                    });
                    self.status = "Note closed".into();
                } else {
                    // Never remove the view on a refused close: report the
                    // typed outcome honestly.
                    self.status = format!(
                        "Note stays open: {}",
                        crate::operation::describe_result(&local)
                    );
                }
            }
            other => {
                self.status = format!("Note stays open: {other:?}");
            }
        }
        Task::none()
    }

    /// Record the user's decision for the active close plan.
    pub fn apply_close_decision(&mut self, decision: CloseDecision) -> Task<Message> {
        let Some(mut plan) = self.closing.take() else {
            return Task::none();
        };
        self.dialog = None;
        let tab = plan.tab;
        // The captured revision must still be current: a document that
        // changed since the dialog opened invalidates the plan (D-07).
        let current = self.note_editor_view(tab).map(|view| view.revision);
        if current != Some(plan.captured_revision) && plan.request.is_none() {
            self.status = "The note changed since the close dialog opened; choose again".into();
            if self.workspace.tab(&tab).is_some() {
                self.dialog = Some(Dialog::Close { tab });
                self.closing = Some(plan);
            }
            return Task::none();
        }
        let path = self
            .notes
            .state(&tab)
            .map(|state| state.locator.to_path(self.root()));
        let Some(path) = path else {
            self.closing = Some(plan);
            return Task::none();
        };
        let Some(flavite) = self.flavite.as_mut() else {
            self.closing = Some(plan);
            return Task::none();
        };
        match decision {
            CloseDecision::SaveClose => {
                let request_id = flavite.command_registry().next_request_id();
                let invocation = flavite_api::Invocation::new(
                    request_id,
                    "note.save",
                    serde_json::json!(path),
                    flavite_api::Origin::Gui,
                );
                match flavite.execute(invocation) {
                    flavite_api::ExecutionResult::Pending { request_id } => {
                        self.operations
                            .accept(request_id, "close: save before close");
                        plan.request = Some(request_id);
                        self.closing = Some(plan);
                        self.status = "Saving before close…".into();
                    }
                    flavite_api::ExecutionResult::Local(local) => {
                        if local.is_success() {
                            self.finish_close_after_save(plan);
                        } else {
                            self.closing = Some(plan);
                            self.status = format!(
                                "Close cancelled: save failed — {}",
                                crate::operation::describe_result(&local)
                            );
                        }
                    }
                    other => {
                        self.closing = Some(plan);
                        self.status = format!("Close cancelled: save did not start ({other:?})");
                    }
                }
            }
            CloseDecision::RetainRecovery => {
                let request_id = flavite.command_registry().next_request_id();
                let invocation = flavite_api::Invocation::new(
                    request_id,
                    "note.close",
                    serde_json::json!({ "path": path, "policy": "retain_recovery" }),
                    flavite_api::Origin::Gui,
                );
                match flavite.execute(invocation) {
                    flavite_api::ExecutionResult::Local(local) => {
                        if local.is_success() {
                            // Durable recovery snapshot covers the edits;
                            // the buffer is gone, the tab follows.
                            self.notes.close_document(tab);
                            self.dispatch_layout(|revision| LayoutAction::CloseTab {
                                expected_revision: revision,
                                tab,
                            });
                            self.status =
                                "Closed; unsaved changes retained as recoverable copy".into();
                            self.advance_close_queue();
                        } else {
                            self.closing = Some(plan);
                            self.status = format!(
                                "Close cancelled: recovery could not be retained — {}",
                                crate::operation::describe_result(&local)
                            );
                        }
                    }
                    other => {
                        self.closing = Some(plan);
                        self.status = format!("Close cancelled: {other:?}");
                    }
                }
            }
            CloseDecision::Discard { expected_revision } => {
                // Revision-bound confirmation: the discard executes only
                // against the revision the user confirmed.
                if current != Some(expected_revision) {
                    self.dialog = Some(Dialog::Close { tab });
                    self.closing = Some(plan);
                    self.status =
                        "The note changed since the discard confirmation; choose again".into();
                    return Task::none();
                }
                let request_id = flavite.command_registry().next_request_id();
                let invocation = flavite_api::Invocation::new(
                    request_id,
                    "note.discard_edits",
                    serde_json::json!({ "path": path }),
                    flavite_api::Origin::Gui,
                );
                match flavite.execute(invocation) {
                    flavite_api::ExecutionResult::Local(local) => {
                        if local.is_success() {
                            self.status = "Unsaved edits discarded (kept in local history)".into();
                            self.finish_close_after_save(plan);
                        } else {
                            self.closing = Some(plan);
                            self.status = format!(
                                "Close cancelled: discard failed — {}",
                                crate::operation::describe_result(&local)
                            );
                        }
                    }
                    other => {
                        self.closing = Some(plan);
                        self.status = format!("Close cancelled: {other:?}");
                    }
                }
            }
        }
        Task::none()
    }

    /// After a save or discard terminal success the buffer is clean;
    /// close it through the core policy, then the tab.
    fn finish_close_after_save(&mut self, plan: ClosePlan) {
        let tab = plan.tab;
        let Some(path) = self
            .notes
            .state(&tab)
            .map(|state| state.locator.to_path(self.root()))
        else {
            return;
        };
        let Some(flavite) = self.flavite.as_mut() else {
            return;
        };
        let request_id = flavite.command_registry().next_request_id();
        let invocation = flavite_api::Invocation::new(
            request_id,
            "note.close",
            serde_json::json!({ "path": path, "policy": "clean" }),
            flavite_api::Origin::Gui,
        );
        match flavite.execute(invocation) {
            flavite_api::ExecutionResult::Local(local) => {
                if local.is_success() {
                    self.notes.close_document(tab);
                    self.dispatch_layout(|revision| LayoutAction::CloseTab {
                        expected_revision: revision,
                        tab,
                    });
                    self.advance_close_queue();
                } else {
                    // A later edit re-dirtied the buffer between save and
                    // close: the core refused; the plan is stale by design.
                    self.closing = Some(plan);
                    self.dialog = Some(Dialog::Close { tab });
                    self.status = format!(
                        "The note changed after saving; close needs a new decision — {}",
                        crate::operation::describe_result(&local)
                    );
                }
            }
            other => {
                self.closing = Some(plan);
                self.status = format!("Close cancelled: {other:?}");
            }
        }
    }

    /// The user keeps the document open: cancel the plan; in a group or
    /// quit sequence the undecided tabs are retained (§7.4).
    pub fn cancel_close(&mut self) {
        self.closing = None;
        self.dialog = None;
        let retained = self.close_queue.len();
        self.close_queue.clear();
        if retained > 0 {
            self.status = format!("Close cancelled; {retained} undecided tab(s) remain open");
        } else {
            self.status = "Close cancelled".into();
        }
    }

    /// Route a completion that belongs to the active close plan. Returns
    /// true when consumed. The save terminal drives the close; a failure
    /// never closes the view (AC-W13).
    pub fn route_close_completion(&mut self, request_id: u64, result: &ExecutionResult) -> bool {
        let Some(plan) = self.closing.as_ref() else {
            return false;
        };
        if plan.request != Some(request_id) {
            return false;
        }
        let plan = self.closing.take().expect("checked above");
        let tab = plan.tab;
        match result {
            ExecutionResult::Local(local) if local.is_success() => {
                // The saved revision is the floor; if the user kept typing,
                // the buffer is dirty again and the decision must be
                // recaptured rather than trusted.
                let state = self.note_buffer_state(tab);
                if matches!(state, Some(BufferState::Dirty)) {
                    self.dialog = Some(Dialog::Close { tab });
                    self.closing = Some(plan);
                    self.status =
                        "The note changed while saving; close needs a new decision".into();
                    return true;
                }
                self.finish_close_after_save(plan);
            }
            ExecutionResult::Rejected(rejected) => {
                self.closing = Some(plan);
                self.status = format!(
                    "Close cancelled: save rejected — {}",
                    crate::operation::describe_rejection(&rejected.reason)
                );
            }
            other => {
                self.closing = Some(plan);
                let completion = flavite_api::OperationCompletion {
                    request_id,
                    result: other.clone(),
                };
                self.status = format!(
                    "Close cancelled: save did not complete — {}",
                    crate::operation::describe_completion(&completion)
                );
            }
        }
        true
    }

    /// Continue a group/quit close sequence with the next queued tab.
    pub fn advance_close_queue(&mut self) {
        self.closing = None;
        if self.close_queue.is_empty() {
            // W12: the last decision of a quit sequence resolved — the
            // schedule continues into the operation drain.
            if self.shutdown.is_some() {
                self.begin_operation_drain();
            }
            return;
        }
        let next = self.close_queue.remove(0);
        if self.workspace.tab(&next).is_none() {
            // The tab vanished mid-sequence; skip it.
            self.advance_close_queue();
            return;
        }
        if self.close_needs_decision(next) {
            match ClosePlan::capture(self, next, CloseOrigin::Group) {
                Some(plan) => {
                    self.closing = Some(plan);
                    self.dialog = Some(Dialog::Close { tab: next });
                }
                None => {
                    self.close_queue.clear();
                    if self.shutdown.is_some() {
                        self.begin_operation_drain();
                    }
                }
            }
        } else {
            let _ = self.complete_safe_close(next, CloseOrigin::Group);
            self.advance_close_queue();
        }
    }
}
