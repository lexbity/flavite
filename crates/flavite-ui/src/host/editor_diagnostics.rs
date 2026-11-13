//! Content-free local diagnostics ring (FEUX-0002 E-15, X07).
//!
//! One bounded, in-memory ring records schema-whitelisted events only:
//! enums and counters, never text. The schema is enforced by construction —
//! [`DiagnosticEvent`] has no string field — so a seeded-token scan over
//! the exported text cannot find note content, clipboard values, queries,
//! paths, or produced characters. Every X03–X06 path records its outcomes
//! here as it ships; the ring is process-local and cleared on exit.
//!
//! The burst-hint banner state also lives here (host-owned UI state); the
//! pure session decides when to show it ([`Effect::ShowBurstHint`]).

use crate::app::{App, Message};
use flavite_edit_context::ContextMode;
use flavite_workspace::TabId;
use std::collections::BTreeMap;
use std::collections::VecDeque;

/// Ring capacity (NFR-8): at most this many events are retained.
pub const MAX_DIAGNOSTIC_EVENTS: usize = 1000;

/// The maximum exported byte count for the explicit export action. The
/// export is bounded by construction, never by the platform clipboard.
pub const MAX_DIAGNOSTIC_EXPORT_BYTES: usize = 64 * 1024;

/// The mode the event occurred in (process-local encoding of the statechart
/// mode; never a string).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeEnum {
    Off,
    Command,
    Typing,
}

impl From<ContextMode> for ModeEnum {
    fn from(mode: ContextMode) -> Self {
        match mode {
            ContextMode::Off => Self::Off,
            ContextMode::Command => Self::Command,
            ContextMode::Typing => Self::Typing,
        }
    }
}

/// The event family (whitelisted, finite).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A Session operation ticket was issued.
    TicketIssued,
    /// A mutating operation applied.
    OperationApplied,
    /// A mutating or selection operation was refused before mutation.
    OperationRefused,
    /// A pending operation was cancelled (host-initiated).
    OperationCancelled,
    /// Duplicate delivery of one activation returned the original outcome.
    OperationDeduplicated,
    /// A retired ticket replay was refused.
    OperationExpired,
    /// A selection-only command outcome (applied or refused).
    SelectionOp,
    /// A match collection started or was superseded.
    MatchCollect,
    /// A draft overlay became ready.
    MatchPreview,
    /// A match overlay was committed.
    MatchCommit,
    /// A match collection or apply was refused.
    MatchRefused,
    /// The regex worker was unavailable; literal paths still worked.
    WorkerUnavailable,
    /// The editing-history chooser was opened or paged.
    HistoryPage,
    /// A history restore was attempted.
    HistoryRestore,
    /// Visible history truncation was observed (eviction).
    HistoryEvict,
    /// A history operation was refused on a limit.
    HistoryRefusal,
    /// The burst hint was shown.
    BurstHint,
    /// The explicit export action ran.
    DiagnosticExport,
}

/// The outcome of the recorded event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Started,
    Ready,
    Applied,
    Refused,
    Cancelled,
    Deduplicated,
    Expired,
    Shown,
}

/// Classify a terminal api error message into its typed outcome (E-5):
/// duplicate delivery, retired replay, cancellation, and generic refusal
/// are distinct outcomes. Pure and unit-tested; the host call sites use it
/// for every ticketed error path.
#[must_use]
pub fn classify_error(message: &str) -> (EventKind, Outcome) {
    if message.contains("AlreadyApplied") {
        (EventKind::OperationDeduplicated, Outcome::Deduplicated)
    } else if message.contains("ReceiptExpired") {
        (EventKind::OperationExpired, Outcome::Expired)
    } else if message.contains("Cancelled") {
        (EventKind::OperationCancelled, Outcome::Cancelled)
    } else {
        (EventKind::OperationRefused, Outcome::Refused)
    }
}

/// One ring entry. All fields are enums or fixed-width integers; no field
/// can carry application text. Memory: ~32 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticEvent {
    pub kind: EventKind,
    pub outcome: Outcome,
    /// Counts (members, matches, spans, dropped events…); `None` when not
    /// applicable.
    pub count: Option<u32>,
    /// Duration in microseconds, when the path measured one.
    pub duration_us: Option<u32>,
    /// Process-local view sequence (stable per tab for the session).
    pub view_seq: u32,
    pub mode: ModeEnum,
    /// Monotonic ring sequence.
    pub at_seq: u64,
}

/// The diagnostics host: bounded ring + explicit export + the burst-hint
/// banner state.
#[derive(Debug, Default)]
pub struct DiagnosticsHost {
    ring: VecDeque<DiagnosticEvent>,
    dropped: u64,
    seq: u64,
    next_view_seq: u32,
    view_seqs: BTreeMap<TabId, u32>,
    /// Whether the burst-hint banner is visible. The pure session decides
    /// when to emit [`Effect::ShowBurstHint`]; this is the rendered banner
    /// state, cleared on dismiss, mode exit, or the banner buttons.
    pub burst_hint: bool,
}

/// Messages owned by the diagnostics host.
#[derive(Debug, Clone)]
pub enum DiagnosticsMessage {
    /// The user dismissed the burst-hint banner.
    DismissBurstHint,
    /// The banner's Exit Context Mode action.
    ExitContextMode(TabId),
    /// The banner's Open history action.
    OpenHistory(TabId),
    /// Explicit export: copy the ring as text (content-free by schema).
    Export,
}

impl DiagnosticsHost {
    /// Record one event. The ring drops the oldest entry beyond
    /// [`MAX_DIAGNOSTIC_EVENTS`] and counts the drop.
    pub fn record(
        &mut self,
        tab: Option<TabId>,
        mode: ModeEnum,
        kind: EventKind,
        outcome: Outcome,
        count: Option<u32>,
        duration_us: Option<u32>,
    ) {
        let view_seq = tab.map_or(0, |tab| self.view_seq_for(tab));
        self.seq = self.seq.wrapping_add(1);
        let event = DiagnosticEvent {
            kind,
            outcome,
            count,
            duration_us,
            view_seq,
            mode,
            at_seq: self.seq,
        };
        if self.ring.len() == MAX_DIAGNOSTIC_EVENTS {
            self.ring.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.ring.push_back(event);
    }

    fn view_seq_for(&mut self, tab: TabId) -> u32 {
        if let Some(seq) = self.view_seqs.get(&tab) {
            return *seq;
        }
        let seq = self.next_view_seq;
        self.next_view_seq = self.next_view_seq.wrapping_add(1);
        self.view_seqs.insert(tab, seq);
        seq
    }

    /// The current ring length.
    #[must_use]
    pub fn event_count(&self) -> usize {
        self.ring.len()
    }

    /// Events dropped by the bound.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// The explicit export: a fixed text rendering of the ring. Content-free
    /// by schema; only the user's explicit export action produces it.
    #[must_use]
    pub fn export_text(&self) -> String {
        let mut out = String::with_capacity(1024);
        for event in &self.ring {
            out.push_str(&format!(
                "{} {:?} {:?} {:?} {:?} v{} {:?}\n",
                event.at_seq,
                event.kind,
                event.outcome,
                event.count,
                event.duration_us,
                event.view_seq,
                event.mode,
            ));
            if out.len() > MAX_DIAGNOSTIC_EXPORT_BYTES {
                return out;
            }
        }
        out
    }

    /// Show the burst-hint banner (the pure session emitted
    /// [`Effect::ShowBurstHint`]).
    pub fn show_burst_hint(&mut self) {
        self.burst_hint = true;
    }

    /// Clear the burst-hint banner.
    pub fn clear_burst_hint(&mut self) {
        self.burst_hint = false;
    }
}

/// Reduce one diagnostics-host message.
pub fn handle(app: &mut App, message: DiagnosticsMessage) -> iced::Task<Message> {
    match message {
        DiagnosticsMessage::DismissBurstHint => {
            app.diagnostics.clear_burst_hint();
            iced::Task::none()
        }
        DiagnosticsMessage::ExitContextMode(tab) => {
            app.diagnostics.clear_burst_hint();
            route_context_command(app, tab, flavite_edit_context::ContextCommand::Exit)
        }
        DiagnosticsMessage::OpenHistory(tab) => {
            app.diagnostics.clear_burst_hint();
            route_context_command(app, tab, flavite_edit_context::ContextCommand::History)
        }
        DiagnosticsMessage::Export => {
            let text = app.diagnostics.export_text();
            let lines = text.lines().count() as u32;
            app.diagnostics.record(
                None,
                ModeEnum::Off,
                EventKind::DiagnosticExport,
                Outcome::Shown,
                Some(lines),
                None,
            );
            if lines == 0 {
                app.context_mode.feedback("No diagnostics recorded yet");
                return iced::Task::none();
            }
            if text.len() > MAX_DIAGNOSTIC_EXPORT_BYTES {
                return iced::Task::none();
            }
            super::editor_clipboard::write(text)
        }
    }
}

/// Reuse the delivered toolbar/accelerator path for the banner actions so
/// admission, sealing, and focus behavior are exactly the delivered ones.
/// Executed synchronously (the same recursive-update pattern the palette
/// uses) so the action lands in the same host turn.
fn route_context_command(
    app: &mut App,
    tab: TabId,
    command: flavite_edit_context::ContextCommand,
) -> iced::Task<Message> {
    let Some(target) = super::context_mode::EditorTarget::capture(app, tab) else {
        return iced::Task::none();
    };
    app.update(Message::ContextMode(
        super::context_mode::ContextMessage::Run(target, command),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_is_small_and_fixed_size() {
        assert!(std::mem::size_of::<DiagnosticEvent>() <= 40);
    }

    #[test]
    fn ring_is_bounded_and_counts_drops() {
        let mut host = DiagnosticsHost::default();
        for _ in 0..MAX_DIAGNOSTIC_EVENTS + 5 {
            host.record(
                None,
                ModeEnum::Command,
                EventKind::OperationApplied,
                Outcome::Applied,
                None,
                None,
            );
        }
        assert_eq!(host.event_count(), MAX_DIAGNOSTIC_EVENTS);
        assert_eq!(host.dropped(), 5);
        // The oldest events were dropped; the newest survive (seq 1..5
        // dropped, 6..1005 retained).
        let exported = host.export_text();
        let first = exported.lines().next().expect("export nonempty");
        assert!(
            first.starts_with("6 "),
            "oldest dropped events are absent: {first}"
        );
    }

    #[test]
    fn view_sequences_are_stable_per_tab() {
        let mut host = DiagnosticsHost::default();
        let a = TabId::from_raw(1);
        let b = TabId::from_raw(2);
        host.record(
            Some(a),
            ModeEnum::Command,
            EventKind::OperationApplied,
            Outcome::Applied,
            None,
            None,
        );
        host.record(
            Some(b),
            ModeEnum::Command,
            EventKind::OperationApplied,
            Outcome::Applied,
            None,
            None,
        );
        host.record(
            Some(a),
            ModeEnum::Command,
            EventKind::OperationApplied,
            Outcome::Applied,
            None,
            None,
        );
        let exported = host.export_text();
        let lines: Vec<&str> = exported.lines().collect();
        assert!(lines[0].ends_with(" v0 Command"), "{}", lines[0]);
        assert!(lines[1].ends_with(" v1 Command"), "{}", lines[1]);
        assert!(lines[2].ends_with(" v0 Command"), "{}", lines[2]);
    }

    #[test]
    fn export_never_contains_application_text_by_construction() {
        let mut host = DiagnosticsHost::default();
        host.record(
            None,
            ModeEnum::Typing,
            EventKind::OperationApplied,
            Outcome::Applied,
            Some(47),
            Some(12),
        );
        let exported = host.export_text();
        assert!(exported.contains("OperationApplied"));
        assert!(exported.contains("Typing"));
        assert!(exported.contains("47"));
        assert!(exported.contains("12"));
    }

    #[test]
    fn terminal_outcomes_classify_distinctly() {
        assert_eq!(
            classify_error("AlreadyApplied"),
            (EventKind::OperationDeduplicated, Outcome::Deduplicated)
        );
        assert_eq!(
            classify_error("ReceiptExpired"),
            (EventKind::OperationExpired, Outcome::Expired)
        );
        assert_eq!(
            classify_error("Cancelled"),
            (EventKind::OperationCancelled, Outcome::Cancelled)
        );
        assert_eq!(
            classify_error("StaleRevision: expected 1, current 2"),
            (EventKind::OperationRefused, Outcome::Refused)
        );
        assert_eq!(
            classify_error("Buffer not found"),
            (EventKind::OperationRefused, Outcome::Refused)
        );
    }

    #[test]
    fn append_stays_within_the_turn_budget() {
        // NFR-8: append stays a fixed-size push. The debug-build ceiling is
        // a loose CI-safe bound; the release p95 target is measured on the
        // reference runner.
        let mut host = DiagnosticsHost::default();
        let start = std::time::Instant::now();
        for i in 0..10_000 {
            host.record(
                None,
                ModeEnum::Command,
                EventKind::OperationApplied,
                Outcome::Applied,
                Some(i),
                None,
            );
        }
        let elapsed = start.elapsed();
        assert!(elapsed.as_millis() < 1500, "10k appends took {elapsed:?}");
    }
}
