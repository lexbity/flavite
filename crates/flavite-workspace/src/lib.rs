//! IO-free workspace model for the Flavite shell (RFC-0003, D-02).
//!
//! This crate owns layout truth: the binary split tree, tab groups, per-view
//! logical state, and the typed transactions that mutate them. It has no
//! toolkit, filesystem, database, or API-facade dependency; the UI adapter
//! projects authoritative state onto widgets and never mutates it from
//! widget callbacks.
//!
//! Every mutation goes through [`reduce`]: a transaction is rejected unless
//! its expected revision matches, applied to a copy, normalized, and
//! validated before it commits. Rejected transactions leave state untouched.

pub mod geometry;
pub mod ids;
pub mod layout;
pub mod reducer;
pub mod validate;
pub mod view;

pub use geometry::{
    Rect, SPLITTER_WIDTH, Size, fits, rects, required_minimum, solve, view_minimum,
};
pub use ids::{GroupId, Ids, SplitId, TabId, WorkspaceId};
pub use layout::{
    Arrangement, Edge, LayoutNode, MAX_GROUPS, MAX_RATIO_PPM, MAX_TABS, MIN_RATIO_PPM, Side,
    TabDropTarget, TabGroup,
};
pub use reducer::{Commit, Effect, LayoutAction, Outcome, WorkspaceError, WorkspaceState, reduce};
pub use validate::validate;
pub use view::{NoteLoad, NoteLocator, ViewKind, ViewSession};
