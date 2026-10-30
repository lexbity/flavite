//! Per-view logical state (RFC-0003 D-03, §7.2).
//!
//! A `ViewSession` owns the logical identity of one tab. Widget resources,
//! buffer bindings, and transient input state stay outside the workspace
//! model; only honest lifecycle state is modeled here. W02 introduces the
//! real kinds `Empty`, `Note`, and `Notes`; serialized unknown kinds become
//! inert preserved records when the codec lands (W06).

use crate::ids::TabId;
use crate::input::EditorInteraction;
use std::path::{Path, PathBuf};

/// A validated, vault-relative note locator (§7.1). Display strings are not
/// identity; the components are kept exactly as constructed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NoteLocator {
    components: Vec<String>,
}

impl NoteLocator {
    /// Build a locator from vault-relative path components. Rejects empty
    /// component lists, empty components, `.`/`..`, and NUL bytes.
    ///
    /// # Errors
    /// Returns a static description of the first invalid component.
    pub fn from_components<I, S>(components: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let components: Vec<String> = components.into_iter().map(Into::into).collect();
        if components.is_empty() {
            return Err("note locator needs at least one path component".into());
        }
        for component in &components {
            if component.is_empty() {
                return Err("note locator has an empty path component".into());
            }
            if component == "." || component == ".." {
                return Err(format!(
                    "note locator rejects `{component}` path components"
                ));
            }
            if component.contains('\0') {
                return Err("note locator rejects NUL bytes".into());
            }
        }
        Ok(Self { components })
    }

    /// Build a locator from a path relative to the vault root.
    ///
    /// # Errors
    /// Same rules as [`Self::from_components`].
    pub fn from_relative(path: &Path) -> Result<Self, String> {
        let components = path
            .components()
            .map(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "note locator components must be valid UTF-8".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_components(components)
    }

    /// Path components, in order.
    #[must_use]
    pub fn components(&self) -> &[String] {
        &self.components
    }

    /// The file name component without its extension, for tab titles.
    ///
    /// # Panics
    /// Never: construction guarantees at least one component.
    #[must_use]
    pub fn title(&self) -> &str {
        let name = self.components.last().expect("non-empty by construction");
        name.strip_suffix(".md").unwrap_or(name)
    }

    /// Join with a vault root for the absolute native path.
    #[must_use]
    pub fn to_path(&self, root: &Path) -> PathBuf {
        let mut path = root.to_path_buf();
        for component in &self.components {
            path.push(component);
        }
        path
    }
}

/// Honest load state of a source-reading note tab (§8.3: Reserved ->
/// Opening -> Ready/Failed; W02 models the observable states).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoteLoad {
    /// The view reserved a slot; the host is opening the note.
    Reserved,
    /// The buffer is open; the tab shows current source text.
    Ready,
    /// The open failed or the note is missing; the tab offers retry/close.
    Failed,
}

/// The kind of one workspace view. Only real, implemented kinds exist;
/// the RFC forbids enabled placeholders for unimplemented capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ViewKind {
    /// A blank tab created with the group or after closing everything.
    Empty,
    /// A source-reading view of one note (editing integrates in W03).
    Note(NoteView),
    /// The vault note list.
    Notes,
}

impl ViewKind {
    /// The kind a duplicate view should carry; `None` for Empty.
    #[must_use]
    pub fn duplicate(&self) -> Option<ViewKind> {
        match self {
            ViewKind::Empty => None,
            ViewKind::Note(note) => Some(ViewKind::Note(NoteView::new(
                note.locator().clone(),
                NoteLoad::Reserved,
            ))),
            ViewKind::Notes => Some(ViewKind::Notes),
        }
    }
}

/// Logical state of one note view.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NoteView {
    locator: NoteLocator,
    load: NoteLoad,
}

impl NoteView {
    #[must_use]
    pub const fn new(locator: NoteLocator, load: NoteLoad) -> Self {
        Self { locator, load }
    }

    #[must_use]
    pub const fn locator(&self) -> &NoteLocator {
        &self.locator
    }

    #[must_use]
    pub const fn load(&self) -> NoteLoad {
        self.load
    }

    pub(crate) fn set_load(&mut self, load: NoteLoad) {
        self.load = load;
    }
}

/// Logical state owned by one tab, independent of widget position (D-03).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ViewSession {
    tab: TabId,
    kind: ViewKind,
    interaction: EditorInteraction,
}

impl ViewSession {
    #[must_use]
    pub fn new(tab: TabId, kind: ViewKind) -> Self {
        Self {
            tab,
            kind,
            interaction: EditorInteraction::new(tab, 1),
        }
    }

    #[must_use]
    pub const fn tab(&self) -> TabId {
        self.tab
    }

    #[must_use]
    pub const fn kind(&self) -> &ViewKind {
        &self.kind
    }

    /// Acknowledged per-view input state, independent of widget-tree position.
    #[must_use]
    pub const fn interaction(&self) -> &EditorInteraction {
        &self.interaction
    }

    pub(crate) fn set_interaction(&mut self, interaction: EditorInteraction) {
        self.interaction = interaction;
    }

    pub(crate) fn kind_mut(&mut self) -> &mut ViewKind {
        &mut self.kind
    }

    /// The tab strip title (§13.1: concise names; details live in the view).
    #[must_use]
    pub fn title(&self) -> String {
        match &self.kind {
            ViewKind::Empty => "Empty".into(),
            ViewKind::Notes => "Notes".into(),
            ViewKind::Note(note) => note.locator().title().to_owned(),
        }
    }

    /// Whether a duplicate view of this kind is meaningful. Duplicating an
    /// Empty tab has no effect, so the reducer rejects it.
    #[must_use]
    pub fn duplicate(&self) -> Option<ViewKind> {
        match &self.kind {
            ViewKind::Empty => None,
            ViewKind::Note(note) => Some(ViewKind::Note(NoteView::new(
                note.locator().clone(),
                NoteLoad::Reserved,
            ))),
            ViewKind::Notes => Some(ViewKind::Notes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locators_reject_traversal_and_empty_components() {
        assert!(NoteLocator::from_components(["..", "x"]).is_err());
        assert!(NoteLocator::from_components(["."]).is_err());
        assert!(NoteLocator::from_components(Vec::<String>::new()).is_err());
        assert!(NoteLocator::from_components(["a", ""]).is_err());
        assert!(NoteLocator::from_components(["a\0"]).is_err());
        let locator = NoteLocator::from_components(["Systems", "Feedback loops.md"]).unwrap();
        assert_eq!(locator.components(), ["Systems", "Feedback loops.md"]);
        assert_eq!(locator.title(), "Feedback loops");
    }

    #[test]
    fn locator_titles_strip_only_markdown_extension() {
        let locator = NoteLocator::from_components(["a.b.md"]).unwrap();
        assert_eq!(locator.title(), "a.b");
        let locator = NoteLocator::from_components(["plain"]).unwrap();
        assert_eq!(locator.title(), "plain");
    }

    #[test]
    fn only_real_views_duplicate() {
        let empty = ViewSession::new(crate::ids::TabId::from_raw(1), ViewKind::Empty);
        assert!(empty.duplicate().is_none());
        let notes = ViewSession::new(empty.tab(), ViewKind::Notes);
        assert_eq!(notes.duplicate(), Some(ViewKind::Notes));
        assert_eq!(empty.title(), "Empty");
    }
}
