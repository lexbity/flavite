//! Text-window abstraction for structure consumers (§ D-10).
//!
//! flavite-syntax MUST NOT depend on flavite-buffer (arch-check). The
//! segment cache (RFC-0011 S5) needs a byte-slice + line-window view of a
//! document without knowing whether the backing store is a `String`, a
//! `DocumentText` chunk-tree view, or (later) a file-backed window. This
//! trait is that seam: flavite-buffer implements it for `DocumentText`
//! (crates/flavite-buffer/src/chunks.rs); the syntax crate consumes only
//! this trait.
//!
//! All offsets are byte offsets into the full document text (the same
//! coordinate space as `flavite_syntax::text::TextByte`).

use std::ops::Range;

/// A single-edit changed-range hint, in pre-edit byte coordinates plus the
/// new end (the structure-sync hint; originally RFC-0008 D1).
///
/// `start..old_end` is the replaced range in the pre-edit text; `new_end`
/// is the end of the replacement (`start + replacement.len()`) in the
/// post-edit text. `None` means "no trustworthy hint" (external reload,
/// merge, journal patch, first open, or a multi-op batch).
///
/// The type lives here (the lowest shared structure-consumer crate) because
/// it is *produced* by `flavite-vault`/`flavite-api` and *consumed* by the
/// segment cache's `sync`; the editor surface re-exports it for the
/// editor-facing name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditRange {
    /// Byte offset where the edit started (pre-edit coordinates).
    pub start: usize,
    /// Byte offset just past the replaced range (pre-edit coordinates).
    pub old_end: usize,
    /// Byte offset just past the replacement (post-edit coordinates).
    pub new_end: usize,
}

/// A read-only text view with byte slices and line windows.
///
/// Implementations must satisfy:
/// - `len()` is the byte length of the document.
/// - `slice(start..end)` returns exactly the bytes `[start, end)` (UTF-8
///   safe; the caller passes char-aligned ranges).
/// - `line_start(line)` returns the byte offset of the start of line
///   `line` (line 0 at byte 0; a `\n` terminator ends a line, so line
///   `k+1` starts one byte after the k-th `\n`).
/// - `line_count()` is one plus the number of `\n` bytes in the document.
pub trait StructureText {
    /// The byte length of the document.
    fn len(&self) -> usize;

    /// The number of lines in the document (LF-counted).
    fn line_count(&self) -> usize;

    /// The byte offset of the start of `line`.
    fn line_start(&self, line: usize) -> usize;

    /// The bytes `[start, end)`, freshly allocated.
    fn slice(&self, range: Range<usize>) -> String;

    /// Whether the document contains no bytes.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The byte offset just past the end of `line` (exclusive).
    fn line_end(&self, line: usize) -> usize {
        if line + 1 < self.line_count() {
            self.line_start(line + 1)
        } else {
            self.len()
        }
    }

    /// The line containing byte offset `pos` (clamped to the last line).
    fn line_index_of(&self, pos: usize) -> usize {
        let pos = pos.min(self.len());
        let mut lo = 0usize;
        let mut hi = self.line_count().saturating_sub(1);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if self.line_start(mid) <= pos {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    }
}

impl StructureText for str {
    fn len(&self) -> usize {
        str::len(self)
    }

    fn line_count(&self) -> usize {
        1 + self.bytes().filter(|&b| b == b'\n').count()
    }

    fn line_start(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        let mut current = 0usize;
        for (i, b) in self.bytes().enumerate() {
            if b == b'\n' {
                current += 1;
                if current == line {
                    return i + 1;
                }
            }
        }
        self.len()
    }

    fn slice(&self, range: Range<usize>) -> String {
        self[range].to_owned()
    }
}

impl StructureText for String {
    fn len(&self) -> usize {
        String::len(self)
    }

    fn line_count(&self) -> usize {
        str::line_count(self)
    }

    fn line_start(&self, line: usize) -> usize {
        str::line_start(self, line)
    }

    fn slice(&self, range: Range<usize>) -> String {
        self[range].to_owned()
    }
}

impl<T: StructureText + ?Sized> StructureText for &T {
    fn len(&self) -> usize {
        (**self).len()
    }

    fn line_count(&self) -> usize {
        (**self).line_count()
    }

    fn line_start(&self, line: usize) -> usize {
        (**self).line_start(line)
    }

    fn slice(&self, range: Range<usize>) -> String {
        (**self).slice(range)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn str_impl_line_geometry() {
        let text = "ab\ncd\nef";
        assert_eq!(text.line_count(), 3);
        assert_eq!(text.line_start(0), 0);
        assert_eq!(text.line_start(1), 3);
        assert_eq!(text.line_start(2), 6);
        assert_eq!(text.line_end(0), 3);
        assert_eq!(text.line_end(1), 6);
        assert_eq!(text.line_end(2), 8);
        assert_eq!(text.line_index_of(0), 0);
        assert_eq!(text.line_index_of(3), 1);
        assert_eq!(text.line_index_of(7), 2);
        assert_eq!(text.slice(0..2), "ab");
        assert_eq!(text.slice(3..5), "cd");
    }

    #[test]
    fn empty_and_trailing_newline() {
        assert_eq!("".line_count(), 1);
        assert_eq!("a\n".line_count(), 2);
        assert_eq!("a\n".line_start(1), 2);
        assert_eq!("a\n".line_end(0), 2);
        assert_eq!("a\n".line_end(1), 2);
        let empty = "";
        let one = "a";
        assert!(empty.is_empty());
        assert!(!one.is_empty());
    }
}
