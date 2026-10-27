//! S1 IME event-delivery probe — spike S1, slice A5 (§9 Phase 0; D18; ADR-0001; AC-24).
//!
//! A minimal iced application hosting one custom widget
//! ([`xtask::s1_probe::ImeProbe`]) that records every runtime event it
//! receives — with special recording for IME events (preedit text, commit
//! text). Run it manually on each target OS with a real IME and follow
//! `devdocs/checklists/ime-acceptance.md`; the per-OS event matrix and the
//! verdicts live in `devdocs/spikes/S1-ime-delivery.md`.
//!
//! Run with: `cargo run -p xtask --bin s1_ime_probe`

use iced::Element;
use xtask::s1_probe::ImeProbe;

/// The probe application: one custom widget, nothing else.
#[derive(Debug, Default)]
struct ProbeApp;

fn main() -> iced::Result {
    eprintln!("S1 IME event-delivery probe (spike S1, PLAN-A slice A5)");
    eprintln!("Follow devdocs/checklists/ime-acceptance.md on this OS.");
    eprintln!("Click into the input area, then type with an IME (JA / ZH / KO).");
    eprintln!("Every event the custom widget receives is logged to stdout with a timestamp.");
    iced::application(ProbeApp::default, ProbeApp::update, ProbeApp::view)
        .title("flavite S1 — IME event-delivery probe")
        .window_size(iced::Size::new(720.0, 520.0))
        .centered()
        .run()
}

impl ProbeApp {
    /// No messages — the widget records everything into its own state.
    fn update(&mut self, _message: iced::Never) {}

    fn view(&self) -> Element<'_, iced::Never> {
        Element::new(ImeProbe::<iced::Renderer>::new())
    }
}
