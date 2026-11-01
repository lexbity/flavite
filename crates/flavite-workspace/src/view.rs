//! Per-view logical state (RFC-0003 D-03, §7.2).
//!
//! A `ViewSession` owns the logical identity of one tab. Widget resources,
//! buffer bindings, and transient input state stay outside the workspace
//! model; only honest lifecycle state is modeled here. W02 introduces the
//! real kinds `Empty`, `Note`, and `Notes`; serialized unknown kinds become
//! inert preserved records when the codec lands (W06).

use crate::context::ContextBinding;
use crate::ids::TabId;
use crate::input::EditorInteraction;
use std::path::{Path, PathBuf};

/// A validated, vault-relative note locator (§7.1). Display strings are not
/// identity; the components are kept exactly as constructed. Ordering is by
/// path components, so registries can key documents by locator.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
            if component.contains(['/', '\\', ':']) {
                return Err(
                    "note locator component contains a path separator or drive prefix".into(),
                );
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
    /// A knowledge-network graph view of the saved index (W07).
    Graph(GraphView),
    /// The vault note list.
    Notes,
    /// Saved source-text preview of the bound context source (W04).
    Preview(ContextBinding),
    /// Inbound links of the bound context source (W04).
    Backlinks(ContextBinding),
    /// Current revisioned heading/block structure.
    Outline(ContextBinding),
    /// Current frontmatter with guarded scalar edits.
    Properties(ContextBinding),
}

impl ViewKind {
    /// The kind a duplicate view should carry; `None` for Empty.
    #[must_use]
    pub fn duplicate(&self) -> Option<ViewKind> {
        match self {
            ViewKind::Empty => None,
            ViewKind::Note(note) => Some(ViewKind::Note(
                NoteView::new(note.locator().clone(), NoteLoad::Reserved).with_mode(note.mode()),
            )),
            ViewKind::Graph(graph) => Some(ViewKind::Graph(graph.clone())),
            ViewKind::Notes => Some(ViewKind::Notes),
            // A duplicated context tool is an independent view with its own
            // copy of the binding; the binding target itself is unchanged.
            ViewKind::Preview(binding) => Some(ViewKind::Preview(binding.clone())),
            ViewKind::Backlinks(binding) => Some(ViewKind::Backlinks(binding.clone())),
            ViewKind::Outline(binding) => Some(ViewKind::Outline(binding.clone())),
            ViewKind::Properties(binding) => Some(ViewKind::Properties(binding.clone())),
        }
    }
}

/// Source editing and rendered reading are modes of the same note identity.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum NoteMode {
    #[default]
    Edit,
    Read,
}

/// Logical state of one note view.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NoteView {
    locator: NoteLocator,
    load: NoteLoad,
    mode: NoteMode,
}

impl NoteView {
    #[must_use]
    pub const fn new(locator: NoteLocator, load: NoteLoad) -> Self {
        Self {
            locator,
            load,
            mode: NoteMode::Edit,
        }
    }

    #[must_use]
    pub const fn locator(&self) -> &NoteLocator {
        &self.locator
    }

    #[must_use]
    pub const fn load(&self) -> NoteLoad {
        self.load
    }

    #[must_use]
    pub const fn mode(&self) -> NoteMode {
        self.mode
    }
    #[must_use]
    pub fn with_mode(mut self, mode: NoteMode) -> Self {
        self.mode = mode;
        self
    }
    pub(crate) fn set_mode(&mut self, mode: NoteMode) {
        self.mode = mode;
    }

    pub(crate) fn set_load(&mut self, load: NoteLoad) {
        self.load = load;
    }
}

/// Rejection of a graph camera value outside the supported range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphCameraError {
    /// A component is NaN or infinite.
    NonFinite,
    /// A world coordinate exceeds ±1e9 (§10.3).
    OutOfWorldRange,
    /// The zoom is outside the interaction range 0.05–64 (§10.3).
    ZoomOutOfRange,
}

impl std::fmt::Display for GraphCameraError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonFinite => write!(f, "graph camera values must be finite"),
            Self::OutOfWorldRange => write!(f, "graph camera coordinates exceed ±1e9 world units"),
            Self::ZoomOutOfRange => write!(f, "graph camera zoom is outside 0.05–64"),
        }
    }
}

impl std::error::Error for GraphCameraError {}

/// Persisted pan/zoom state of one graph view (§10.3, §11.1).
///
/// Validated at construction: every component finite, coordinates within
/// ±1e9 world units, and zoom inside the interaction range 0.05–64.
/// Imported or restored cameras outside the range are *rejected*, not
/// clamped (§10.3: "rejected outside that range during import/restore
/// validation").
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphCamera {
    /// World point shown at the viewport centre.
    pub center: [f32; 2],
    /// World units → logical pixels.
    pub zoom: f32,
}

/// Interaction and persistence bounds (§10.3).
impl GraphCamera {
    /// Lowest admitted zoom.
    pub const MIN_ZOOM: f32 = 0.05;
    /// Highest admitted zoom.
    pub const MAX_ZOOM: f32 = 64.0;
    /// Largest admitted absolute world coordinate.
    pub const MAX_COORDINATE: f32 = 1.0e9;

    /// The default centered camera at 1:1.
    #[must_use]
    pub const fn centered() -> Self {
        Self {
            center: [0.0, 0.0],
            zoom: 1.0,
        }
    }

    /// Build a validated camera.
    ///
    /// # Errors
    /// Returns the first violated bound ([`GraphCameraError`]).
    pub fn new(center: [f32; 2], zoom: f32) -> Result<Self, GraphCameraError> {
        if !center[0].is_finite() || !center[1].is_finite() || !zoom.is_finite() {
            return Err(GraphCameraError::NonFinite);
        }
        if center[0].abs() > Self::MAX_COORDINATE || center[1].abs() > Self::MAX_COORDINATE {
            return Err(GraphCameraError::OutOfWorldRange);
        }
        if !(Self::MIN_ZOOM..=Self::MAX_ZOOM).contains(&zoom) {
            return Err(GraphCameraError::ZoomOutOfRange);
        }
        Ok(Self { center, zoom })
    }

    /// Apply a pan and clamp the result to the world bounds (interaction,
    /// not import: out-of-range drags stop at the edge instead of refusing).
    #[must_use]
    pub fn panned(self, dx: f32, dy: f32) -> Self {
        let clamp = |value: f32| {
            if value.is_finite() {
                value.clamp(-Self::MAX_COORDINATE, Self::MAX_COORDINATE)
            } else {
                0.0
            }
        };
        Self {
            center: [clamp(self.center[0] + dx), clamp(self.center[1] + dy)],
            zoom: self.zoom,
        }
    }

    /// Multiply the zoom, clamped to the interaction range.
    #[must_use]
    pub fn zoomed(self, factor: f32) -> Self {
        let zoom = if factor.is_finite() && factor > 0.0 {
            (self.zoom * factor).clamp(Self::MIN_ZOOM, Self::MAX_ZOOM)
        } else {
            self.zoom
        };
        Self {
            center: self.center,
            zoom,
        }
    }
}

impl Default for GraphCamera {
    fn default() -> Self {
        Self::centered()
    }
}

// A validated camera is always finite, so identity is exact-bit equality;
// hashing the raw bits keeps `Eq`/`Hash` consistent without an `f32`
// structural derive.
impl Eq for GraphCamera {}

impl std::hash::Hash for GraphCamera {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (if self.center[0] == 0.0 {
            0
        } else {
            self.center[0].to_bits()
        })
        .hash(state);
        (if self.center[1] == 0.0 {
            0
        } else {
            self.center[1].to_bits()
        })
        .hash(state);
        self.zoom.to_bits().hash(state);
    }
}

/// Logical state of one graph view: the durable per-view camera.
///
/// Node selection, hit candidates, and in-flight drag previews are
/// host-transient (never part of the authoritative model); the camera is
/// durable because it survives tab moves and layout restarts (§11.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GraphView {
    camera: GraphCamera,
    scope: crate::GraphScope,
}

impl GraphView {
    #[must_use]
    pub fn new(camera: GraphCamera) -> Self {
        Self {
            camera,
            scope: crate::GraphScope::default(),
        }
    }

    #[must_use]
    pub const fn camera(&self) -> GraphCamera {
        self.camera
    }

    #[must_use]
    pub const fn scope(&self) -> &crate::GraphScope {
        &self.scope
    }

    /// Validate before committing a scope; rejected updates leave it intact.
    ///
    /// # Errors
    /// Returns the first exceeded clause, depth, or value bound.
    pub fn set_scope(&mut self, scope: crate::GraphScope) -> Result<(), &'static str> {
        scope.validate()?;
        self.scope = scope;
        Ok(())
    }

    /// Replace the camera after the host validates a released gesture.
    ///
    /// # Errors
    /// Returns the first violated bound; the view keeps its old camera.
    pub fn set_camera(&mut self, camera: GraphCamera) -> Result<(), GraphCameraError> {
        self.camera = GraphCamera::new(camera.center, camera.zoom)?;
        Ok(())
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
            ViewKind::Graph(_) => "Graph".into(),
            ViewKind::Preview(_) => "Preview".into(),
            ViewKind::Backlinks(_) => "Backlinks".into(),
            ViewKind::Outline(_) => "Outline".into(),
            ViewKind::Properties(_) => "Properties".into(),
        }
    }

    /// Whether a duplicate view of this kind is meaningful. Duplicating an
    /// Empty tab has no effect, so the reducer rejects it.
    #[must_use]
    pub fn duplicate(&self) -> Option<ViewKind> {
        match &self.kind {
            ViewKind::Empty => None,
            ViewKind::Note(note) => Some(ViewKind::Note(
                NoteView::new(note.locator().clone(), NoteLoad::Reserved).with_mode(note.mode()),
            )),
            ViewKind::Graph(graph) => Some(ViewKind::Graph(graph.clone())),
            ViewKind::Notes => Some(ViewKind::Notes),
            ViewKind::Preview(binding) => Some(ViewKind::Preview(binding.clone())),
            ViewKind::Backlinks(binding) => Some(ViewKind::Backlinks(binding.clone())),
            ViewKind::Outline(binding) => Some(ViewKind::Outline(binding.clone())),
            ViewKind::Properties(binding) => Some(ViewKind::Properties(binding.clone())),
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
