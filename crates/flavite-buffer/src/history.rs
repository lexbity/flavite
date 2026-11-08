//! Revision-bound editing history. Saved note snapshots are a separate API.

use serde::{Deserialize, Serialize};

pub const PAGE_LIMIT: usize = 100;
pub const TRAIL_LIMIT: usize = 12;
/// The mounted editor currently renders one selection. Refuse metadata it
/// cannot faithfully restore; the canonical multi-selection slice raises this.
pub const RECORDED_SELECTION_LIMIT: usize = 1;

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

/// The current editor's ordered selections, primary first. This is a recorded
/// value, not ownership of live selections in another view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorEdit {
    pub origin: String,
    pub kind: EditKind,
    pub before: Vec<[usize; 2]>,
    pub after: Vec<[usize; 2]>,
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
        validate_selections(before, &self.before)?;
        validate_selections(after, &self.after)
    }
}

pub fn validate_selections(text: &str, selections: &[[usize; 2]]) -> Result<(), HistoryError> {
    if selections.is_empty() || selections.len() > RECORDED_SELECTION_LIMIT {
        return Err(HistoryError::InvalidSelections);
    }
    let mut endpoints = selections.iter().flatten().copied().collect::<Vec<_>>();
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryError {
    Expired,
    StalePage,
    JournalBoundary,
    InvalidSelections,
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Expired => "The editing-history state expired; refresh history",
            Self::StalePage => "Editing history changed; refresh the page",
            Self::JournalBoundary => "Use Undo/Redo to cross the rename transaction",
            Self::InvalidSelections => "Recorded editor selections are invalid",
        })
    }
}

impl std::error::Error for HistoryError {}

/// An exact source-free transform for passive views of the same buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryChange {
    pub start: usize,
    pub end: usize,
    pub inserted: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMovement {
    pub selections: Option<Vec<[usize; 2]>>,
    pub change: HistoryChange,
}
