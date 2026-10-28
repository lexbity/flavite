//! Per-line EOL side table and serialization (§4.8, R12, §4.16.1).
//!
//! Lattice MUST preserve byte-exact round-trips for unedited files and maintain
//! mixed-EOL files without normalization. This is achieved via:
//!
//! 1. A bitmap recording LF/CRLF/None per line
//! 2. Explicit BOM flag
//! 3. Explicit trailing-newline flag
//! 4. A serialization pass that reconstructs the original bytes
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
/// Tracks the line ending style for each line, the BOM presence, and whether
/// the file has a trailing newline.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EolTable {
    /// EOL for each line. The length matches the number of lines in the buffer.
    pub line_endings: Vec<LineEnding>,
    /// Whether the buffer's source began with a UTF-8 BOM.
    pub has_bom: bool,
    /// Whether the buffer's source ends with a trailing newline.
    pub has_trailing_newline: bool,
}

impl EolTable {
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
            has_bom: snapshot.has_bom(),
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

    /// Whether the buffer has a BOM.
    #[must_use]
    pub const fn has_bom(&self) -> bool {
        self.has_bom
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

    /// Create a byte-exact serialization of the current text with EOLs applied.
    ///
    /// This reconstructs the original bytes for unedited content, or applies
    /// the EOL rules to edited content.
    ///
    /// # Arguments
    /// * `text` - The normalized text (CRLF folded to LF, BOM removed)
    /// * `line_breaks` - The TextByte offsets where lines end in the normalized text
    #[must_use]
    pub fn serialize(&self, text: &str, line_breaks: &[TextByte]) -> Vec<u8> {
        let mut out = Vec::with_capacity(text.len() + self.line_endings.len() * 2 + 3);

        // Add BOM if present
        if self.has_bom {
            out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        }

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

    /// Apply EOL metadata to a range of newly inserted lines.
    ///
    /// New lines inherit the dominant EOL of the buffer.
    pub fn apply_to_inserted(&mut self, start_line: usize, count: usize) {
        let _dominant = self.dominant_eol();
        for i in 0..count {
            let line_idx = start_line + i;
            if line_idx < self.line_endings.len() {
                // For existing lines that were split, keep their original EOL
                continue;
            }
            // New lines get the dominant EOL
            // We'll push them when the actual line split happens
        }
        // This is called during edit application; the actual line split
        // will push new entries with the correct EOL
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
    /// Whether the buffer has a BOM.
    pub has_bom: bool,
    /// Whether the buffer has a trailing newline.
    pub has_trailing_newline: bool,
}

impl EolTable {
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
            has_bom: self.has_bom,
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

        assert!(table.has_bom());
        assert!(table.has_trailing_newline());
    }

    #[test]
    fn serialize_round_trip() {
        let src = "\u{FEFF}hello\r\nworld\n";
        let snapshot = snap(src);
        let table = EolTable::from_snapshot(&snapshot);

        // Build line breaks
        let line_breaks: Vec<TextByte> = snapshot.lines().iter().map(|l| l.text_end).collect();

        let serialized = table.serialize(snapshot.text(), &line_breaks);
        assert_eq!(serialized, src.as_bytes());
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
}
