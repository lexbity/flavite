//! Editor-side structure engine (RFC-0008 D7-B/D8, S7–S8).
//!
//! The editor widget consumes structure spans through this type — never
//! through tree-sitter directly, keeping the `flavite-editor` → syntax
//! dependency graph unchanged (§10.2 layering; `arch-check` gates it).
//!
//! # Stage B (S7) — full parse behind the flag
//!
//! [`StructureEngine::sync`] performs a full [`crate::ts_ranges::parse_note_ts`]
//! per call. The `hint` parameter ([`EditRange`], computed by the vault
//! session from the applied transition) is accepted but not yet consumed;
//! stage C (S8) will use it to drive `tree.edit` + incremental reparses.
//!
//! # Why the hint type lives here
//!
//! RFC-0008 §5.2 originally placed [`EditRange`] in `flavite-editor`'s
//! surface, but the hint is *produced* by `flavite-vault` and `flavite-api`
//! (both below the editor) and *consumed* by this engine. The type therefore
//! lives in the lowest shared crate — flavite-syntax — and is re-exported by
//! the surface for the editor-facing name.

use crate::ts_ranges::{TsParsedNote, parse_note_ts};

/// A single-edit changed-range hint, in pre-edit byte coordinates plus the
/// new end (RFC-0008 D1).
///
/// `start..old_end` is the replaced range in the pre-edit text; `new_end`
/// is the end of the replacement (`start + replacement.len()`) in the
/// post-edit text. `None` means "no trustworthy hint" (external reload,
/// merge, journal patch, first open, or a multi-op batch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditRange {
    /// Byte offset where the edit started (pre-edit coordinates).
    pub start: usize,
    /// Byte offset just past the replaced range (pre-edit coordinates).
    pub old_end: usize,
    /// Byte offset just past the replacement (post-edit coordinates).
    pub new_end: usize,
}

/// The widget's structure source: owns parse state and produces spans.
///
/// Stage B: stateless full parse (S7). Stage C (S8) will own a
/// tree-sitter `Parser` + `Tree` for incremental reparses and will consume
/// [`EditRange`] hints.
#[derive(Debug, Default)]
pub struct StructureEngine {
    /// Sealed; stage C stores the incremental parser/tree here.
    _seal: (),
}

impl StructureEngine {
    /// Create an empty engine.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Synchronize structure with `bytes` and return parsed spans.
    ///
    /// Stage B: full parse every call (flag-on). `hint` is accepted for the
    /// S8 seam but is not yet used.
    ///
    /// # Errors
    ///
    /// Same as [`parse_note_ts`] (invalid UTF-8, embedded NUL, parse
    /// failure).
    pub fn sync(&mut self, bytes: &[u8], _hint: Option<EditRange>) -> Result<TsParsedNote, String> {
        parse_note_ts(bytes)
    }
}
