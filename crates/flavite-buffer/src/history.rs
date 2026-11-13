//! Revision-bound editing history. Saved note snapshots are a separate API.

use crate::selection::MAX_SELECTIONS;
use serde::{Deserialize, Serialize};

pub const PAGE_LIMIT: usize = 100;
pub const TRAIL_LIMIT: usize = 12;
/// Recorded editor metadata can carry a full canonical selection set
/// (FEUX-0002 E-4/E-12); the single-selection slice is retired.
pub const RECORDED_SELECTION_LIMIT: usize = MAX_SELECTIONS;

/// Buffer-history incarnation. It never repeats when a new process happens
/// to allocate the same numeric buffer/node IDs as an earlier process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryEpoch(u128);

impl HistoryEpoch {
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().as_u128())
    }
}

impl Default for HistoryEpoch {
    fn default() -> Self {
        Self::new()
    }
}
impl std::fmt::Display for HistoryEpoch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}
impl Serialize for HistoryEpoch {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for HistoryEpoch {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.len() != 32
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(serde::de::Error::custom(
                "history epoch must be 32 lowercase hex characters",
            ));
        }
        u128::from_str_radix(&value, 16)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// Source-free operation labels; callers cannot put note content in summaries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditKind {
    #[default]
    Edit,
    Typing,
    Delete,
    Backspace,
    Cut,
    Paste,
    Change,
    OpenAbove,
    OpenBelow,
    Format,
    Replace,
    Snapshot,
}

impl EditKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Edit => "Source edit",
            Self::Typing => "Typing",
            Self::Delete => "Delete",
            Self::Backspace => "Backspace",
            Self::Cut => "Cut",
            Self::Paste => "Paste",
            Self::Change => "Change",
            Self::OpenAbove => "Open line above",
            Self::OpenBelow => "Open line below",
            Self::Format => "Source format",
            Self::Replace => "Replace",
            Self::Snapshot => "Saved snapshot",
        }
    }
}

/// The current editor's ordered selections. This is a recorded value, not
/// ownership of live selections in another view. Members are `[id, anchor,
/// head]` triples in canonical order (FEUX-0002 §5.1); `primary` names the
/// primary member by its index in `after` — canonical order and "primary
/// first" cannot both be implicit, so primary identity is explicit
/// (FEUX-0002 E-4 / FR-4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorEdit {
    pub origin: String,
    pub kind: EditKind,
    pub before: Vec<[usize; 3]>,
    pub after: Vec<[usize; 3]>,
    /// Index into `after` of the primary member (default: the first member).
    #[serde(default)]
    pub primary: usize,
}

impl EditorEdit {
    pub fn validate(&self, before: &str, after: &str) -> Result<(), HistoryError> {
        if self.origin.len() != 32
            || !self
                .origin
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(HistoryError::InvalidSelections);
        }
        if self.primary >= self.after.len() {
            return Err(HistoryError::InvalidSelections);
        }
        validate_selections(before, &self.before)?;
        validate_selections(after, &self.after)
    }
}

pub fn validate_selections(text: &str, selections: &[[usize; 3]]) -> Result<(), HistoryError> {
    if selections.is_empty() || selections.len() > RECORDED_SELECTION_LIMIT {
        return Err(HistoryError::InvalidSelections);
    }
    let mut endpoints = selections
        .iter()
        .flat_map(|triple| &triple[1..])
        .copied()
        .collect::<Vec<_>>();
    endpoints.sort_unstable();
    endpoints.dedup();
    use unicode_segmentation::UnicodeSegmentation;
    let boundaries = text
        .grapheme_indices(true)
        .map(|(at, _)| at)
        .chain(std::iter::once(text.len()));
    let mut boundaries = boundaries.peekable();
    for at in endpoints {
        while boundaries.peek().is_some_and(|boundary| *boundary < at) {
            boundaries.next();
        }
        if boundaries.peek() != Some(&at) {
            return Err(HistoryError::InvalidSelections);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryTarget {
    pub epoch: HistoryEpoch,
    pub node: u64,
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Wall-clock display metadata only; ordering and expiry use IDs/version.
    pub changed_at_unix_ms: u64,
    pub selection_count: Option<usize>,
    pub on_path: bool,
    pub target: HistoryTarget,
    pub parent: Option<u64>,
    pub kind: EditKind,
    pub current: bool,
    pub open: bool,
    pub restorable: bool,
    pub initial: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryCursor {
    pub epoch: HistoryEpoch,
    pub generation: u64,
    pub before: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPage {
    pub epoch: HistoryEpoch,
    pub generation: u64,
    pub current: u64,
    pub entries: Vec<HistoryEntry>,
    pub trail: Vec<HistoryEntry>,
    pub redo: Option<HistoryEntry>,
    pub next: Option<HistoryCursor>,
    /// True when the queried range crosses an eviction: some states that
    /// were created earlier than `retained_watermark` are no longer
    /// retained (FEUX-0002 X01 / E-12).
    pub history_truncated: bool,
    /// Smallest surviving node id; every smaller id is evicted.
    pub retained_watermark: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryError {
    Expired,
    StalePage,
    JournalBoundary,
    InvalidSelections,
    /// A cursor was invalidated by an eviction (state it pointed at was
    /// removed to stay within the history budget). Distinct from
    /// `StalePage` so UIs can offer a budget-aware refresh.
    HistoryChanged,
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Expired => "The editing-history state expired; refresh history",
            Self::StalePage => "Editing history changed; refresh the page",
            Self::JournalBoundary => "Use Undo/Redo to cross the rename transaction",
            Self::InvalidSelections => "Recorded editor selections are invalid",
            Self::HistoryChanged => {
                "Editing history changed; earlier states were removed to stay within the memory budget"
            }
        })
    }
}

impl std::error::Error for HistoryError {}

/// One movement span: a replaced range in the coordinates of the text before
/// the transition, expressed as the replacement length it contributes
/// (FEUX-0002 E-13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovementSpan {
    pub start: usize,
    pub end: usize,
    pub replacement_len: usize,
}

/// Maximum number of spans a movement delta may carry before passive views
/// must clamp instead of guessing (E-13).
pub const MAX_MOVEMENT_SPANS: usize = 256;

/// The exact source-free transform of one transition for passive views of
/// the same buffer: an ordered list of non-overlapping deltas in pre-change
/// coordinates. `selections` is the requesting view's exact recorded
/// after-set (never approximated); `primary` is the index into
/// `selections` of that set's primary member, so primary identity survives
/// every receipt round trip (FR-4/FR-7). Passive views map through `spans`,
/// or clamp with a visible notice when `truncated` (the transition exceeded
/// [`MAX_MOVEMENT_SPANS`] or its edit chain is not fully derivable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMovement {
    pub selections: Option<Vec<[usize; 3]>>,
    pub spans: Vec<MovementSpan>,
    pub truncated: bool,
    /// Index into `selections` of the primary member (absent when unknown).
    #[serde(default)]
    pub primary: Option<usize>,
}
