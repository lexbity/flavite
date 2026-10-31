//! Host-side semantic link insertion (RFC-0003 §9, W05).
//!
//! One gesture at a time: the host captures the acknowledged writing target
//! plus a core-issued client token and monotonic sequence, prepares a plan,
//! and commits it (immediately by default, or after explicit confirmation).
//! A gesture is invalidated — never committed — when its source caret,
//! revision, or view identity stops matching. No fallback caret is ever
//! manufactured: unmet preconditions surface as honest, actionable states.

use flavite_api::ExecutionResult;
use flavite_workspace::TabId;

/// The acknowledged source caret a gesture is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GestureSource {
    /// The tab owning the writing target.
    pub tab: TabId,
    /// View incarnation at gesture time.
    pub incarnation: u64,
    /// Source buffer.
    pub buffer_id: u64,
    /// Acknowledged revision.
    pub revision: u64,
    /// Acknowledged collapsed caret.
    pub caret: usize,
}

/// Observable gesture state (presentation keeps it honest at every step).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GestureState {
    /// A prepare request is in flight.
    Preparing {
        /// Target display title.
        target: String,
    },
    /// A plan is prepared and awaiting commit (confirmation mode only).
    Ready {
        /// Opaque plan capability (string form of the u128).
        plan_id: String,
        /// The serialized `[[...]]` text.
        serialized_link: String,
        /// Target display title.
        target: String,
    },
    /// The link was inserted with one guarded edit.
    Inserted {
        /// The serialized text now in the document.
        serialized_link: String,
        /// Target display title.
        target: String,
    },
    /// The gesture failed without mutating anything.
    Failed {
        /// Stable machine code from the core.
        code: String,
        /// The repair action.
        repair: String,
    },
    /// No gesture exists.
    Idle,
}

#[derive(Debug)]
struct PendingGesture {
    request_id: u64,
    sequence: u64,
    target_path: String,
    source: GestureSource,
    /// A prepared plan held for explicit confirmation.
    ready: Option<ReadyPlan>,
}

#[derive(Debug)]
struct ReadyPlan {
    plan_id: String,
}

/// Host-side gesture registry owned by the App.
impl Default for LinkIntents {
    fn default() -> Self {
        Self {
            client: None,
            next_sequence: 0,
            pending: None,
            state: GestureState::Idle,
        }
    }
}

pub struct LinkIntents {
    pub(crate) client: Option<u128>,
    next_sequence: u64,
    pending: Option<PendingGesture>,
    state: GestureState,
}

impl std::fmt::Debug for LinkIntents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkIntents")
            .field("next_sequence", &self.next_sequence)
            .field(
                "pending",
                &self.pending.as_ref().map(|pending| pending.request_id),
            )
            .field("state", &self.state)
            .finish()
    }
}

impl LinkIntents {
    /// Whether a completion with this request id belongs to a gesture.
    #[must_use]
    pub fn handles(&self, request_id: u64) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id)
    }

    /// Observable state.
    #[must_use]
    pub const fn state(&self) -> &GestureState {
        &self.state
    }

    /// Reset presentation to idle (no pending gesture).
    pub fn clear_state(&mut self) {
        if !matches!(self.state, GestureState::Idle) {
            self.state = GestureState::Idle;
        }
    }

    /// Begin one insertion gesture from a picker/context action. Returns the
    /// request id of the in-flight prepare, or an honest refusal reason.
    pub fn begin(
        &mut self,
        source: GestureSource,
        target_path: String,
        target_display: String,
    ) -> Result<u64, String> {
        if self.pending.is_some() {
            return Err("another link insertion is already in flight".into());
        }
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);
        self.state = GestureState::Preparing {
            target: target_display,
        };
        let sequence = self.next_sequence;
        self.pending = Some(PendingGesture {
            // The prepare request id is stamped by the caller once the core
            // returns it; a placeholder keeps the struct total until then.
            request_id: 0,
            sequence,
            target_path,
            source,
            ready: None,
        });
        Ok(sequence)
    }

    /// Stamp the pending gesture's prepare request id.
    pub fn stamp_request(&mut self, request_id: u64) {
        if let Some(pending) = &mut self.pending
            && pending.request_id == 0
        {
            pending.request_id = request_id;
        }
    }

    /// Observe a served prepare completion. Returns the plan to commit when
    /// no confirmation is required.
    #[must_use]
    pub fn observe_prepared(
        &mut self,
        request_id: u64,
        plan_id: String,
        serialized_link: String,
        target_display: String,
        requires_confirmation: bool,
    ) -> Option<(u64, String)> {
        let Some(pending) = &mut self.pending else {
            return None;
        };
        if pending.request_id != request_id {
            return None;
        }
        if requires_confirmation {
            pending.ready = Some(ReadyPlan {
                plan_id: plan_id.clone(),
            });
            self.state = GestureState::Ready {
                plan_id: plan_id.clone(),
                serialized_link: serialized_link.clone(),
                target: target_display,
            };
            return None;
        }
        let sequence = pending.sequence;
        self.state = GestureState::Idle;
        Some((sequence, plan_id))
    }

    /// The sequence and plan ID of a gesture held for confirmation.
    #[must_use]
    pub fn confirmed(&self) -> Option<(u64, String)> {
        let pending = self.pending.as_ref()?;
        let ready = pending.ready.as_ref()?;
        Some((pending.sequence, ready.plan_id.clone()))
    }

    /// Record the terminal outcome and retire the gesture.
    pub fn finish(&mut self, state: GestureState) {
        self.pending = None;
        self.state = state;
    }

    /// Cancel a held/pending gesture without consuming the sequence
    /// (§9: no auto commit after cancellation).
    pub fn cancel(&mut self) {
        if self.pending.is_some() {
            self.pending = None;
            self.state = GestureState::Idle;
        }
    }

    /// Invalidate the gesture when its source binding no longer matches.
    /// `source` is the currently acknowledged writing target identity.
    pub fn invalidate_if_source_changed(&mut self, source: Option<(TabId, u64, u64, u64, usize)>) {
        let Some(pending) = &self.pending else {
            return;
        };
        let matches = source.is_some_and(|(tab, incarnation, buffer_id, revision, caret)| {
            pending.source.tab == tab
                && pending.source.incarnation == incarnation
                && pending.source.buffer_id == buffer_id
                && pending.source.revision == revision
                && pending.source.caret == caret
        });
        if !matches {
            self.cancel();
        }
    }

    /// The gesture sequence of the pending prepare, for token stamping.
    #[must_use]
    pub fn pending_sequence(&self) -> Option<u64> {
        self.pending.as_ref().map(|pending| pending.sequence)
    }

    /// The pending gesture's target path.
    #[must_use]
    pub fn pending_target_path(&self) -> Option<String> {
        self.pending
            .as_ref()
            .map(|pending| pending.target_path.clone())
    }

    /// The pending gesture's source.
    #[must_use]
    pub fn pending_source(&self) -> Option<GestureSource> {
        self.pending.as_ref().map(|pending| pending.source.clone())
    }
}

/// Extract the prepared plan fields from a served completion payload.
#[must_use]
pub fn prepared_from_data(data: &serde_json::Value) -> Option<PreparedData> {
    Some(PreparedData {
        plan_id: data.get("plan_id")?.as_str()?.to_owned(),
        serialized_link: data.get("serialized_link")?.as_str()?.to_owned(),
        target_display: data
            .get("target_display")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        expires_in_ms: data
            .get("expires_in_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        requires_confirmation: data
            .get("requires_confirmation")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

/// The prepared fields the host needs from a completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedData {
    /// Opaque plan capability.
    pub plan_id: String,
    /// Serialized `[[...]]` text.
    pub serialized_link: String,
    /// Target display title.
    pub target_display: String,
    /// TTL in milliseconds.
    pub expires_in_ms: u64,
    /// Whether explicit confirmation is required.
    pub requires_confirmation: bool,
}

/// Extract the inserted revision from a successful commit payload.
#[must_use]
pub fn inserted_revision(result: &ExecutionResult) -> Option<u64> {
    match result {
        ExecutionResult::Local(flavite_api::LocalResult::Success(success)) => success
            .data
            .get("revision")
            .and_then(serde_json::Value::as_u64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> GestureSource {
        GestureSource {
            tab: TabId::from_raw(3),
            incarnation: 1,
            buffer_id: 12,
            revision: 5,
            caret: 9,
        }
    }

    #[test]
    fn gesture_lifecycle_stamps_and_finishes() {
        let mut intents = LinkIntents::default();
        intents
            .begin(source(), "beta.md".into(), "beta".into())
            .expect("begun");
        intents.stamp_request(101);
        assert!(intents.handles(101));
        assert!(!intents.handles(102));
        // Immediate mode: the plan is returned for commit.
        let commit =
            intents.observe_prepared(101, "9".into(), "[[beta]]".into(), "beta".into(), false);
        assert_eq!(commit, Some((1, "9".into())));
        intents.finish(GestureState::Inserted {
            serialized_link: "[[beta]]".into(),
            target: "beta".into(),
        });
        assert!(matches!(
            intents.state(),
            GestureState::Inserted { serialized_link, .. } if serialized_link == "[[beta]]"
        ));
    }

    #[test]
    fn confirmation_holds_until_explicit_action() {
        let mut intents = LinkIntents::default();
        intents
            .begin(source(), "beta.md".into(), "beta".into())
            .expect("begun");
        intents.stamp_request(5);
        assert_eq!(
            intents.observe_prepared(5, "9".into(), "[[beta]]".into(), "beta".into(), true),
            None,
            "confirmation mode holds the plan"
        );
        assert_eq!(intents.confirmed(), Some((1, "9".into())));
        intents.cancel();
        assert!(intents.confirmed().is_none(), "cancellation never commits");
    }

    #[test]
    fn source_changes_cancel_without_commit() {
        let mut intents = LinkIntents::default();
        intents
            .begin(source(), "beta.md".into(), "beta".into())
            .expect("begun");
        intents.stamp_request(5);
        intents.invalidate_if_source_changed(Some((
            source().tab,
            source().incarnation,
            source().buffer_id,
            source().revision + 1,
            source().caret,
        )));
        assert!(
            intents.pending_target_path().is_none(),
            "stale gesture cancelled"
        );
        intents
            .begin(source(), "beta.md".into(), "beta".into())
            .expect("begun");
        intents.invalidate_if_source_changed(None);
        assert!(intents.pending_target_path().is_none());
        intents
            .begin(source(), "beta.md".into(), "beta".into())
            .expect("begun");
        intents.invalidate_if_source_changed(Some((
            source().tab,
            source().incarnation,
            source().buffer_id,
            source().revision,
            source().caret,
        )));
        assert!(
            intents.pending_target_path().is_some(),
            "matching source keeps the gesture"
        );
    }
}
