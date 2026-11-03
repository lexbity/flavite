//! The local-history view (W10): one note's content-addressed snapshots
//! with a guarded, undoable restore. Listings and restores go through the
//! API (`note.history` / `note.restore_history`); this module never reads
//! or writes note files itself and disk is never touched by a restore.

use crate::app::{App, Message};
use crate::style;
use crate::workspace::tab_strip;
use flavite_workspace::{NoteLocator, TabId};
use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Element, Length};
use std::collections::BTreeMap;

/// One listed snapshot row (typed projection of the API payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    /// Blake3 hex hash of the snapshot bytes.
    pub hash: String,
    /// Snapshot creation time, Unix seconds.
    pub timestamp_secs: u64,
    /// Why the snapshot exists (stable wire name).
    pub reason: String,
    /// Pinned/protected record count.
    pub pins: u32,
}

/// Host-owned per-tab history listings.
#[derive(Debug, Default)]
pub struct HistoryHost {
    /// Entries per history tab.
    pub tabs: BTreeMap<TabId, HistoryList>,
}

/// The listing state of one history tab.
#[derive(Debug, Default)]
pub struct HistoryList {
    /// The note this view describes.
    pub locator: Option<NoteLocator>,
    /// Newest-first snapshot rows.
    pub entries: Vec<HistoryRow>,
    /// Load failure text (retryable).
    pub error: Option<String>,
}

impl HistoryHost {
    /// Drop the state of a closed tab.
    pub fn detach(&mut self, tab: TabId) {
        self.tabs.remove(&tab);
    }

    /// Store a loaded listing.
    pub fn loaded(&mut self, tab: TabId, locator: NoteLocator, entries: Vec<HistoryRow>) {
        self.tabs.insert(
            tab,
            HistoryList {
                locator: Some(locator),
                entries,
                error: None,
            },
        );
    }

    /// Record a load failure.
    pub fn failed(&mut self, tab: TabId, message: String) {
        let list = self.tabs.entry(tab).or_default();
        list.error = Some(message);
    }
}

/// Load (or reload) one note's history through the API.
pub fn load(app: &mut App, tab: TabId, locator: &NoteLocator) {
    let Some(flavite) = &app.flavite else {
        return;
    };
    let path = locator.to_path(app.root());
    let request_id = flavite.command_registry().next_request_id();
    let invocation = flavite_api::command::Invocation::new(
        request_id,
        "note.history",
        serde_json::json!({ "path": path, "limit": 100 }),
        flavite_api::Origin::Gui,
    );
    match flavite.execute(invocation) {
        flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Success(success)) => {
            let entries = success
                .data
                .get("entries")
                .and_then(serde_json::Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter_map(|entry| {
                            Some(HistoryRow {
                                hash: entry.get("hash")?.as_str()?.to_owned(),
                                timestamp_secs: entry.get("timestamp_secs")?.as_u64()?,
                                reason: entry.get("reason")?.as_str()?.to_owned(),
                                pins: entry.get("pins")?.as_u64()? as u32,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            app.history.loaded(tab, locator.clone(), entries);
        }
        flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Error(error)) => {
            app.history.failed(tab, error.message);
        }
        _ => app
            .history
            .failed(tab, "History is unavailable right now".into()),
    }
}

/// Render the history view for one tab.
pub fn view(app: &App, tab: TabId, locator: &NoteLocator) -> Element<'static, Message> {
    let Some(list) = app.history.tabs.get(&tab) else {
        return crate::views::empty_state("History", "Loading snapshots…");
    };
    let mut rows = column![].spacing(2);
    if list.entries.is_empty() && list.error.is_none() {
        rows = rows.push(
            text("No local snapshots yet. Snapshots are captured on save, merges, renames, and conflict resolutions.")
                .style(style::dim_text),
        );
    }
    for entry in &list.entries {
        rows = rows.push(entry_row(app, tab, locator, entry));
    }
    let header = if let Some(error) = &list.error {
        column![
            text("History unavailable").size(style::HEADING),
            text(error.clone()).style(style::dim_text),
            button(text("Retry"))
                .padding(tab_strip::chip_padding())
                .style(style::action())
                .on_press(Message::HistoryRefresh { tab }),
        ]
        .spacing(style::SPACING_SM)
    } else {
        column![
            text(format!("History: {}", locator.title())).size(style::TEXT_LG),
            text(format!("{} snapshots · restore replaces the open buffer as one undoable edit; disk is written only when you save", list.entries.len()))
                .size(style::TEXT_SM)
                .style(style::dim_text),
        ]
        .spacing(style::SPACING_SM)
    };
    container(
        column![
            row![header,].spacing(style::SPACING_SM),
            button(text("Refresh"))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::HistoryRefresh { tab }),
            container(scrollable(rows).height(Length::Fill))
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(style::SPACING_SM)
                .style(style::card),
        ]
        .spacing(style::SPACING_SM),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(style::SPACING_LG)
    .style(style::content)
    .into()
}

fn entry_row(
    app: &App,
    tab: TabId,
    locator: &NoteLocator,
    entry: &HistoryRow,
) -> Element<'static, Message> {
    let age = format_age(entry.timestamp_secs);
    let restoreable = app.can_restore_history(locator);
    let mut line = row![
        text(age).width(Length::Fixed(150.0)),
        text(reason_label(&entry.reason)).width(Length::Fixed(170.0)),
    ]
    .spacing(style::SPACING_SM);
    if entry.pins > 0 {
        line = line.push(
            text(format!("pinned ×{}", entry.pins))
                .size(style::TEXT_SM)
                .style(style::dim_text),
        );
    }
    line = line.push(
        text(entry.hash.chars().take(12).collect::<String>())
            .size(style::TEXT_SM)
            .style(style::dim_text),
    );
    button(row![line, text("Restore").size(style::TEXT_SM),].spacing(style::SPACING))
        .width(Length::Fill)
        .padding(iced::Padding {
            top: 6.0,
            bottom: 6.0,
            left: 10.0,
            right: 10.0,
        })
        .style(style::list_row(false))
        .on_press_maybe(restoreable.then(|| Message::HistoryRestore {
            tab,
            hash: entry.hash.clone(),
        }))
        .into()
}

/// Why a snapshot exists, in interface wording.
fn reason_label(reason: &str) -> &'static str {
    match reason {
        "save" => "Saved version",
        "conflict_resolution" => "Conflict resolution",
        "merge" => "Merge",
        "rename" => "Rename",
        "recovery_base" => "Recovery base",
        "transaction" => "Transaction",
        _ => "Snapshot",
    }
}

/// Bounded relative age; exact times live in the tool tip space we do not
/// have, so the coarse label is honest about recency only.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn format_age(timestamp_secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(timestamp_secs);
    let delta = now.saturating_sub(timestamp_secs);
    match delta {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", delta / 60),
        3600..=86_399 => format!("{} h ago", delta / 3600),
        86_400..=2_591_999 => format!("{} d ago", delta / 86_400),
        _ => format!("{} mo ago", delta / 2_592_000),
    }
}
