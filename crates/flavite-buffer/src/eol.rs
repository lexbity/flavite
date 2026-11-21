//! Per-line EOL side table and serialization (§4.8, R12, §4.16.1).
//!
//! Flavite MUST preserve byte-exact round-trips for unedited files and maintain
//! mixed-EOL files without normalization. This is achieved via:
//!
//! 1. A table recording LF/CRLF/None per line (the BOM lives in the
//!    `SourceFormat` descriptor, never here)
//! 2. An explicit trailing-newline flag
//! 3. A serialization pass that reconstructs the original bytes
//!
//! Newly inserted lines inherit the dominant EOL of the file; edits and their
//! undo carry EOL metadata so mixed-EOL state is preserved.

use flavite_syntax::text::{Eol as SyntaxEol, TextByte, TextSnapshot};
use serde::{Deserialize, Serialize};

/// Per-line EOL information for the buffer's serialization.
///
/// Mirrors [`SyntaxEol`] but is stored separately for the buffer's mutable
/// state, since the TextSnapshot is immutable after construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum LineEnding {
    /// Line feed (`\n`).
    Lf = 0,
    /// Carriage return + line feed (`\r\n`).
    Crlf = 1,
    /// No terminator (last line of a file without a trailing newline).
    None = 2,
}

impl From<SyntaxEol> for LineEnding {
    fn from(eol: SyntaxEol) -> Self {
        match eol {
            SyntaxEol::Lf => Self::Lf,
            SyntaxEol::Crlf => Self::Crlf,
            SyntaxEol::None => Self::None,
        }
    }
}

impl From<LineEnding> for SyntaxEol {
    fn from(eol: LineEnding) -> Self {
        match eol {
            LineEnding::Lf => Self::Lf,
            LineEnding::Crlf => Self::Crlf,
            LineEnding::None => Self::None,
        }
    }
}

/// The EOL side table for a buffer.
///
/// Tracks the line ending style for each line and whether the file has a
/// trailing newline. BOM ownership lives in the buffer's
/// [`SourceFormat`](flavite_encoding::SourceFormat)
/// descriptor (RFC-0006 §6.10), not here: this table is the EOL map of the
/// decoded-UTF-8 layer and never carries a format flag.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EolTable {
    /// EOL for each line. The length matches the number of lines in the buffer.
    pub line_endings: Vec<LineEnding>,
    /// Whether the buffer's source ends with a trailing newline.
    pub has_trailing_newline: bool,
}

impl EolTable {
    /// Derive the EOL table by scanning `text`'s literal bytes: each line
    /// split on `\n` records its physical ending (`\r\n` → Crlf, `\n` →
    /// Lf, unterminated → None). Used at save/recovery/snapshot-legacy
    /// sites and for building tables from scratch (D-04).
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        if text.is_empty() {
            return Self {
                line_endings: vec![LineEnding::None],
                has_trailing_newline: false,
            };
        }
        let mut line_endings = Vec::new();
        let mut has_trailing_newline = false;
        for line in text.split_inclusive('\n') {
            let ending = if line.ends_with("\r\n") {
                LineEnding::Crlf
            } else if line.ends_with('\n') {
                LineEnding::Lf
            } else {
                LineEnding::None
            };
            has_trailing_newline = ending != LineEnding::None;
            line_endings.push(ending);
        }
        Self {
            line_endings,
            has_trailing_newline,
        }
    }

    /// Create a new EOL table from a TextSnapshot.
    #[must_use]
    pub fn from_snapshot(snapshot: &TextSnapshot) -> Self {
        let line_count = snapshot.line_count();
        let mut line_endings = Vec::with_capacity(line_count);

        for line in snapshot.lines() {
            line_endings.push(LineEnding::from(line.eol));
        }

        let has_trailing_newline = if line_count > 0 {
            // A file has a trailing newline if its last line has an EOL.
            snapshot.lines()[line_count - 1].eol != SyntaxEol::None
        } else {
            false
        };

        Self {
            line_endings,
            has_trailing_newline,
        }
    }

    /// Get the EOL for a specific line.
    ///
    /// Returns `None` if the line index is out of bounds.
    #[must_use]
    pub fn get(&self, line_idx: usize) -> Option<LineEnding> {
        self.line_endings.get(line_idx).copied()
    }

    /// Set the EOL for a specific line.
    ///
    /// # Panics
    /// Panics if the line index is out of bounds.
    pub fn set(&mut self, line_idx: usize, eol: LineEnding) {
        self.line_endings[line_idx] = eol;
        // Update trailing newline flag if this is the last line
        if line_idx == self.line_endings.len() - 1 {
            self.has_trailing_newline = eol != LineEnding::None;
        }
    }

    /// Get the number of lines.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.line_endings.len()
    }

    /// Whether the buffer has a trailing newline.
    #[must_use]
    pub const fn has_trailing_newline(&self) -> bool {
        self.has_trailing_newline
    }

    /// Get the dominant EOL style in the buffer.
    ///
    /// Returns the most common line ending, with CRLF preferred over LF for
    /// mixed files (Windows convention).
    #[must_use]
    pub fn dominant_eol(&self) -> LineEnding {
        let mut lf_count = 0usize;
        let mut crlf_count = 0usize;
        let mut _none_count = 0usize;

        for &eol in &self.line_endings {
            match eol {
                LineEnding::Lf => lf_count += 1,
                LineEnding::Crlf => crlf_count += 1,
                LineEnding::None => _none_count += 1,
            }
        }

        // Prefer CRLF if present, otherwise LF, otherwise None
        if crlf_count > 0 {
            LineEnding::Crlf
        } else if lf_count > 0 {
            LineEnding::Lf
        } else {
            LineEnding::None
        }
    }

    /// Push a new line with the given EOL.
    pub fn push(&mut self, eol: LineEnding) {
        self.line_endings.push(eol);
        // If this is the first line, we're the new last line
        if self.line_endings.len() == 1 {
            self.has_trailing_newline = eol != LineEnding::None;
        }
    }

    /// Pop the last line.
    ///
    /// Returns the EOL of the removed line, or `None` if empty.
    #[must_use]
    pub fn pop(&mut self) -> Option<LineEnding> {
        let result = self.line_endings.pop();
        if let Some(_eol) = result {
            // Update trailing newline if we removed the last line
            if self.line_endings.is_empty() {
                self.has_trailing_newline = false;
            } else {
                let new_last = self.line_endings.last().copied();
                self.has_trailing_newline = new_last != Some(LineEnding::None);
            }
        }
        result
    }

    /// Insert a line at a specific position with the given EOL.
    ///
    /// # Panics
    /// Panics if the index is out of bounds.
    pub fn insert(&mut self, line_idx: usize, eol: LineEnding) {
        self.line_endings.insert(line_idx, eol);
    }

    /// Remove a line at a specific position.
    ///
    /// # Panics
    /// Panics if the index is out of bounds.
    pub fn remove(&mut self, line_idx: usize) -> LineEnding {
        let result = self.line_endings.remove(line_idx);
        // If we removed the last line, update trailing newline
        if line_idx == self.line_endings.len() {
            self.has_trailing_newline = self
                .line_endings
                .last()
                .copied()
                .is_some_and(|e| e != LineEnding::None);
        }
        result
    }

    /// Clear all line endings.
    pub fn clear(&mut self) {
        self.line_endings.clear();
        self.has_trailing_newline = false;
    }

    /// Create a byte-exact serialization of the current DECODED UTF-8 layer
    /// with physical EOLs applied (CRLF restored where recorded).
    ///
    /// This reconstructs the decoded source bytes for unedited content, or
    /// applies the EOL rules to edited content. It does NOT prepend a BOM and
    /// does NOT perform codec serialization: the buffer's [`SourceFormat`]
    /// descriptor owns both (RFC-0006 §6.10).
    ///
    /// [`SourceFormat`]: flavite_encoding::SourceFormat
    ///
    /// # Arguments
    /// * `text` - The normalized text (CRLF folded to LF, BOM removed)
    /// * `line_breaks` - The TextByte offsets where lines end in the normalized text
    #[must_use]
    pub fn serialize(&self, text: &str, line_breaks: &[TextByte]) -> Vec<u8> {
        let mut out = Vec::with_capacity(text.len() + self.line_endings.len() * 2);

        let mut start = 0usize;
        for (i, &end_byte) in line_breaks.iter().enumerate() {
            let end = end_byte.0;
            // Push the line content
            out.extend_from_slice(&text.as_bytes()[start..end]);

            // Push the line ending based on our table
            if let Some(eol) = self.line_endings.get(i) {
                match eol {
                    LineEnding::Lf => out.push(b'\n'),
                    LineEnding::Crlf => out.extend_from_slice(b"\r\n"),
                    LineEnding::None => {}
                }
            }

            // Skip the newline in normalized text if this line has an EOL
            if let Some(eol) = self.line_endings.get(i) {
                if *eol != LineEnding::None {
                    // Skip the LF in normalized text
                    start = end + 1;
                } else {
                    start = end;
                }
            } else {
                start = end;
            }
        }
        if start < text.len() {
            out.extend_from_slice(&text.as_bytes()[start..]);
        }

        out
    }
}

/// The recorded ending of one split-inclusive line slice.
#[must_use]
pub(crate) fn line_ending_of(line: &str) -> LineEnding {
    if line.ends_with("\r\n") {
        LineEnding::Crlf
    } else if line.ends_with('\n') {
        LineEnding::Lf
    } else {
        LineEnding::None
    }
}

/// Split text into split-inclusive lines (an empty text yields one phantom
/// line — matching the snapshot serializer's line model).
#[must_use]
pub(crate) fn logical_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        vec![""]
    } else {
        text.split_inclusive('\n').collect()
    }
}

/// Statistics about line endings for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EolStats {
    /// Number of lines with LF ending.
    pub lf_count: usize,
    /// Number of lines with CRLF ending.
    pub crlf_count: usize,
    /// Number of lines with no ending (should be 0 or 1 at the end).
    pub none_count: usize,
    /// Whether the buffer has a trailing newline.
    pub has_trailing_newline: bool,
}

impl EolTable {
    /// Splice this table across one edited window (RFC-0011 D-04): the
    /// windowed reference splice RFC-0008 D6 windowed reference splice.
    ///
    /// `window_first` is the table index of the window's first line;
    /// `window_old` is the pre-edit bytes of `[window_start, tail_end)`;
    /// `window_new` is the post-edit bytes of that window; `window_reaches_end`
    /// says whether the window runs to the end of the document (trailing
    /// newline state is then derived from the new window, else preserved).
    ///
    /// Unchanged prefix/suffix entries keep their recorded endings; changed
    /// lines inherit the old dominant ending; a new final unterminated line
    /// stays unterminated.
    #[must_use]
    pub fn splice_window(
        &self,
        window_first: usize,
        window_old: &str,
        window_new: &str,
        window_reaches_end: bool,
    ) -> EolTable {
        let old_count = self.line_count();
        if window_new.is_empty() {
            // The whole run was deleted. The surviving document is the
            // unchanged, line-aligned head and tail: the run contributes no
            // lines (a wholly deleted run is not a phantom unterminated line).
            if window_first == 0 && window_reaches_end {
                // The document is now empty: one unterminated line (§6.10).
                return EolTable {
                    line_endings: vec![LineEnding::None],
                    has_trailing_newline: false,
                };
            }
            let removed = old_count
                .saturating_sub(window_first)
                .min(window_old.split_inclusive('\n').count());
            let mut table = self.clone();
            table
                .line_endings
                .splice(window_first..window_first + removed, []);
            // The surviving document tail is unchanged: the old final line
            // when the tail survives, otherwise the (terminated) last head
            // line.
            table.has_trailing_newline = if !window_reaches_end {
                self.has_trailing_newline
            } else {
                window_first > 0
            };
            return table;
        }
        let dominant = match self.dominant_eol() {
            LineEnding::None => LineEnding::Lf,
            ending => ending,
        };
        // `logical_lines` maps an empty run to its one phantom line, matching
        // the whole-document reference.
        let old_lines = logical_lines(window_old);
        let new_lines = logical_lines(window_new);
        let removed = old_count.saturating_sub(window_first).min(old_lines.len());
        let new_count = old_count - removed + new_lines.len();

        let mut head = 0;
        while head < old_lines.len() && head < new_lines.len() && old_lines[head] == new_lines[head]
        {
            head += 1;
        }
        let prefix = window_first + head;
        let mut suffix = 0;
        while suffix < old_lines.len().saturating_sub(head)
            && suffix < new_lines.len().saturating_sub(head)
            && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
        {
            suffix += 1;
        }
        // The unchanged lines beyond the window are equal on both sides and
        // always extend the whole-document suffix (the reference compares
        // them line by line); the window tail matches stack on top of them.
        let beyond_window = old_count
            .saturating_sub(window_first)
            .saturating_sub(removed);
        let suffix = (suffix + beyond_window)
            .min(old_count.saturating_sub(prefix))
            .min(new_count.saturating_sub(prefix));
        let old_middle = old_count.saturating_sub(prefix + suffix);
        let new_middle = new_count.saturating_sub(prefix + suffix);

        let mut table = EolTable::default();
        table.line_endings.reserve(new_count);
        for line in 0..new_count {
            // The scanned terminator of the new text at this line: a
            // replacement line without a newline is authoritative and never
            // inherits one.
            let scanned = if line < window_first {
                self.get(line)
            } else if line < window_first + new_lines.len() {
                Some(line_ending_of(new_lines[line - window_first]))
            } else {
                self.get(old_count - (new_count - line))
            };
            if scanned == Some(LineEnding::None) {
                table.line_endings.push(LineEnding::None);
                continue;
            }
            let old_line = if line < prefix {
                Some(line)
            } else if line >= new_count - suffix {
                Some(old_count - (new_count - line))
            } else if old_middle == new_middle {
                Some(line)
            } else {
                None
            };
            let ending = old_line
                .and_then(|old_line| self.get(old_line))
                .unwrap_or(dominant);
            table.line_endings.push(ending);
        }
        // The reference's trailing flag comes from the whole-document scan:
        // the last document line lies beyond the window (the old trailing
        // state stands) or is the window's final new line.
        table.has_trailing_newline = if !window_reaches_end {
            self.has_trailing_newline
        } else {
            new_lines
                .last()
                .is_some_and(|line| line_ending_of(line) != LineEnding::None)
        };
        table
    }

    /// Compute statistics about line endings.
    #[must_use]
    pub fn stats(&self) -> EolStats {
        let mut lf_count = 0usize;
        let mut crlf_count = 0usize;
        let mut none_count = 0usize;

        for &eol in &self.line_endings {
            match eol {
                LineEnding::Lf => lf_count += 1,
                LineEnding::Crlf => crlf_count += 1,
                LineEnding::None => none_count += 1,
            }
        }

        EolStats {
            lf_count,
            crlf_count,
            none_count,
            has_trailing_newline: self.has_trailing_newline,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_syntax::text::TextSnapshot;

    fn snap(src: &str) -> TextSnapshot {
        TextSnapshot::new(src.as_bytes()).expect("utf-8")
    }

    #[test]
    fn from_snapshot_preserves_eols() {
        let src = "line1\r\nline2\nline3";
        let snapshot = snap(src);
        let table = EolTable::from_snapshot(&snapshot);

        assert_eq!(table.line_count(), 3);
        assert_eq!(table.get(0), Some(LineEnding::Crlf));
        assert_eq!(table.get(1), Some(LineEnding::Lf));
        assert_eq!(table.get(2), Some(LineEnding::None));
        assert!(!table.has_trailing_newline());
    }

    #[test]
    fn from_snapshot_with_trailing_newline() {
        let src = "line1\r\nline2\n";
        let snapshot = snap(src);
        let table = EolTable::from_snapshot(&snapshot);

        assert_eq!(table.line_count(), 2);
        assert_eq!(table.get(0), Some(LineEnding::Crlf));
        assert_eq!(table.get(1), Some(LineEnding::Lf));
        assert!(table.has_trailing_newline());
    }

    #[test]
    fn from_snapshot_with_bom() {
        let src = "\u{FEFF}line1\n";
        let snapshot = snap(src);
        let table = EolTable::from_snapshot(&snapshot);

        // The EOL table owns only line-ending state; the BOM lives on the
        // snapshot descriptor (RFC-0006 §6.10).
        assert!(snapshot.has_bom());
        assert!(table.has_trailing_newline());
    }

    #[test]
    fn serialize_round_trip() {
        let src = "\u{FEFF}hello\r\nworld\n";
        let snapshot = snap(src);
        let table = EolTable::from_snapshot(&snapshot);

        // Build line breaks
        let line_breaks: Vec<TextByte> = snapshot.lines().iter().map(|l| l.text_end).collect();

        // `serialize` produces the DECODED layer (no BOM); the codec re-adds
        // it per the descriptor.
        let serialized = table.serialize(snapshot.text(), &line_breaks);
        assert_eq!(serialized, "hello\r\nworld\n".as_bytes());
    }

    #[test]
    fn serialize_mixed_eol() {
        let src = "one\r\ntwo\nthree\r\n";
        let snapshot = snap(src);
        let table = EolTable::from_snapshot(&snapshot);

        let line_breaks: Vec<TextByte> = snapshot.lines().iter().map(|l| l.text_end).collect();

        let serialized = table.serialize(snapshot.text(), &line_breaks);
        assert_eq!(serialized, src.as_bytes());
    }

    #[test]
    fn dominant_eol() {
        let mut table = EolTable::default();
        table.push(LineEnding::Crlf);
        table.push(LineEnding::Crlf);
        table.push(LineEnding::Lf);
        assert_eq!(table.dominant_eol(), LineEnding::Crlf);

        let mut table2 = EolTable::default();
        table2.push(LineEnding::Lf);
        table2.push(LineEnding::Lf);
        assert_eq!(table2.dominant_eol(), LineEnding::Lf);

        let mut table3 = EolTable::default();
        table3.push(LineEnding::None);
        assert_eq!(table3.dominant_eol(), LineEnding::None);
    }

    #[test]
    fn stats() {
        let mut table = EolTable::default();
        table.push(LineEnding::Crlf);
        table.push(LineEnding::Lf);
        table.push(LineEnding::Crlf);
        table.push(LineEnding::None);

        let stats = table.stats();
        assert_eq!(stats.crlf_count, 2);
        assert_eq!(stats.lf_count, 1);
        assert_eq!(stats.none_count, 1);
    }

    #[test]
    fn mutable_eol_table_operations_keep_trailing_state_consistent() {
        let mut table = EolTable::default();
        assert_eq!(table.pop(), None);
        table.push(LineEnding::Crlf);
        assert!(table.has_trailing_newline());
        table.set(0, LineEnding::None);
        assert!(!table.has_trailing_newline());
        table.insert(0, LineEnding::Lf);
        assert_eq!(table.remove(1), LineEnding::None);
        assert!(table.has_trailing_newline());
        assert_eq!(table.pop(), Some(LineEnding::Lf));
        assert!(table.line_endings.is_empty());

        table.push(LineEnding::Crlf);
        table.clear();
        assert_eq!(table.line_count(), 0);
        assert!(!table.has_trailing_newline());
    }

    #[test]
    fn syntax_eol_conversion_covers_every_variant() {
        assert_eq!(LineEnding::from(SyntaxEol::Lf), LineEnding::Lf);
        assert_eq!(LineEnding::from(SyntaxEol::Crlf), LineEnding::Crlf);
        assert_eq!(LineEnding::from(SyntaxEol::None), LineEnding::None);
        assert_eq!(SyntaxEol::from(LineEnding::Lf), SyntaxEol::Lf);
        assert_eq!(SyntaxEol::from(LineEnding::Crlf), SyntaxEol::Crlf);
        assert_eq!(SyntaxEol::from(LineEnding::None), SyntaxEol::None);
    }
}
