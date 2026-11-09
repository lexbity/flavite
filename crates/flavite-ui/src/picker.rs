//! One candidate snapshot and matcher service for note pickers (W10).
//!
//! RFC-0003 W10 replaces the temporary W02 picker duplication: the quick
//! switch overlay, the Notes view filter, the open-in-tab dialog, and any
//! future note-target flow all consume this one in-memory matcher over the
//! bounded catalog snapshot. Command-palette matching stays in
//! [`crate::palette`] (commands have a different haystack), but both share
//! the same nucleo matcher configuration through `shared_matcher`.
//!
//! §4.7: matching is in-memory nucleo over at most
//! `MAX_NOTE_CATALOG_LIMIT` titles; no query ever reaches the index.

use flavite_api::NoteCatalogEntry;
use flavite_workspace::NoteLocator;
use std::sync::{LazyLock, Mutex};

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher};

/// The shared note/command matcher. A single static keeps allocation
/// behavior identical across surfaces and avoids competing matcher pools.
pub(crate) static SHARED_MATCHER: LazyLock<Mutex<Matcher>> =
    LazyLock::new(|| Mutex::new(Matcher::new(Config::DEFAULT)));

/// Acquire the shared matcher, recovering from a poisoned lock.
pub(crate) fn shared_matcher() -> std::sync::MutexGuard<'static, Matcher> {
    SHARED_MATCHER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One selectable note candidate rendered by every picker surface.
#[derive(Debug, Clone)]
pub struct NoteCandidate {
    /// The catalog entry (title and native path).
    pub entry: NoteCatalogEntry,
    /// Pre-resolved locator; invalid paths are filtered at snapshot time.
    pub locator: NoteLocator,
    /// Precomputed haystack: title plus native path.
    haystack: String,
}

impl NoteCandidate {
    /// The display title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.entry.title
    }

    /// The vault-relative path for display.
    #[must_use]
    pub fn path(&self) -> std::path::Display<'_> {
        self.entry.path.display()
    }
}

/// A bounded, pre-resolved candidate snapshot of the current catalog.
#[derive(Debug, Clone, Default)]
pub struct NoteCandidates {
    candidates: std::sync::Arc<[NoteCandidate]>,
    entries: std::sync::Arc<[NoteCatalogEntry]>,
    cache: std::cell::RefCell<Option<MatchSnapshot>>,
}

#[derive(Debug, Clone)]
struct MatchSnapshot {
    query: String,
    candidates: std::sync::Arc<[NoteCandidate]>,
    entries: std::sync::Arc<[NoteCatalogEntry]>,
}

impl NoteCandidates {
    /// Build a snapshot from the current catalog, dropping entries whose
    /// paths cannot form a locator (they are unopenable by construction).
    #[must_use]
    pub fn from_catalog(entries: &[NoteCatalogEntry]) -> Self {
        let candidates: std::sync::Arc<[NoteCandidate]> = entries
            .iter()
            .filter_map(|entry| {
                Some(NoteCandidate {
                    locator: NoteLocator::from_relative(&entry.path).ok()?,
                    entry: entry.clone(),
                    haystack: format!("{} {}", entry.title, entry.path.display()),
                })
            })
            .collect();
        let entries = candidates
            .iter()
            .map(|candidate| candidate.entry.clone())
            .collect::<Vec<_>>()
            .into();
        Self {
            candidates,
            entries,
            cache: std::cell::RefCell::new(None),
        }
    }

    /// All candidates in canonical path order.
    #[must_use]
    pub fn all(&self) -> &[NoteCandidate] {
        &self.candidates
    }

    /// Fuzzy-match candidates against title and path (§4.7). An empty
    /// query returns every candidate in canonical order.
    #[must_use]
    pub fn matches(&self, query: &str) -> std::sync::Arc<[NoteCandidate]> {
        let needle = query.trim();
        if needle.is_empty() {
            return self.candidates.clone();
        }
        if let Some(cached) = self
            .cache
            .borrow()
            .as_ref()
            .filter(|cached| cached.query == needle)
        {
            return cached.candidates.clone();
        }
        let pattern = Pattern::parse(needle, CaseMatching::Smart, Normalization::Smart);
        let haystacks: Vec<&str> = self
            .candidates
            .iter()
            .map(|candidate| candidate.haystack.as_str())
            .collect();
        let mut matcher = shared_matcher();
        let matched_indexes: Vec<usize> = pattern
            .match_list(haystacks.iter().copied(), &mut matcher)
            .into_iter()
            .filter_map(|(matched, _)| haystacks.iter().position(|candidate| *candidate == matched))
            .collect();
        drop(matcher);
        let candidates: std::sync::Arc<[NoteCandidate]> = matched_indexes
            .into_iter()
            .map(|index| self.candidates[index].clone())
            .collect::<Vec<_>>()
            .into();
        let entries = candidates
            .iter()
            .map(|candidate| candidate.entry.clone())
            .collect::<Vec<_>>()
            .into();
        *self.cache.borrow_mut() = Some(MatchSnapshot {
            query: needle.into(),
            candidates: candidates.clone(),
            entries,
        });
        candidates
    }

    /// The same cached match order as facade rows, shared across picker frames.
    pub fn matching_entries(&self, query: &str) -> std::sync::Arc<[NoteCatalogEntry]> {
        if query.trim().is_empty() {
            return self.entries.clone();
        }
        let _ = self.matches(query);
        self.cache
            .borrow()
            .as_ref()
            .expect("match snapshot")
            .entries
            .clone()
    }
}
