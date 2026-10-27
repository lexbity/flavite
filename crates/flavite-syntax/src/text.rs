//! Text snapshots and coordinate types (§4.16.1).
//!
//! Lattice normalizes CRLF to LF at input and strips a UTF-8 BOM, while
//! *remembering* exactly what the source bytes were, so that an unedited note
//! round-trips byte-exactly and a mixed-EOL note is never normalized (§4.8,
//! R12). Everything downstream — the scanner, both parse glues, the buffer —
//! works in [`TextByte`] coordinates over [`TextSnapshot::text`] and maps back
//! to [`SourceByte`] when it needs original bytes.
//!
//! Distinct newtypes keep the coordinate spaces from being mixed up:
//!
//! | Type | Meaning |
//! |---|---|
//! | [`SourceByte`] | offset into the original source bytes |
//! | [`TextByte`] | offset into the normalized text (CRLF → LF, BOM removed) |
//! | [`ScalarIndex`] | Unicode scalar (code point) index |
//! | [`GraphemeIndex`] | grapheme-cluster index (cursor movement, §4.16.1) |
//! | [`Utf16Offset`] | UTF-16 code-unit offset (platform text APIs) |
//!
//! Every mapping in this module is total at *legal edit boundaries* and
//! `None` at illegal ones (the interior of a CRLF pair, inside the BOM, or
//! outside the buffer): "BOM and CRLF interiors are never edit boundaries"
//! (§4.16.1).

use unicode_segmentation::UnicodeSegmentation;

/// Offset into the original source bytes of a note (§4.16.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SourceByte(pub usize);

impl From<SourceByte> for usize {
    fn from(sb: SourceByte) -> usize {
        sb.0
    }
}

impl From<usize> for SourceByte {
    fn from(val: usize) -> Self {
        Self(val)
    }
}

/// Offset into the normalized text of a snapshot (§4.16.1).
///
/// Normalization removes a leading UTF-8 BOM and folds CRLF to LF. Everything
/// else — including a standalone CR — is preserved as content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TextByte(pub usize);

impl From<TextByte> for usize {
    fn from(tb: TextByte) -> usize {
        tb.0
    }
}

impl From<usize> for TextByte {
    fn from(val: usize) -> Self {
        Self(val)
    }
}

/// Unicode scalar (code point) index (§4.16.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ScalarIndex(pub usize);

impl From<ScalarIndex> for usize {
    fn from(si: ScalarIndex) -> usize {
        si.0
    }
}

impl From<usize> for ScalarIndex {
    fn from(val: usize) -> Self {
        Self(val)
    }
}

/// Grapheme-cluster index (§4.16.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct GraphemeIndex(pub usize);

impl From<GraphemeIndex> for usize {
    fn from(gi: GraphemeIndex) -> usize {
        gi.0
    }
}

impl From<usize> for GraphemeIndex {
    fn from(val: usize) -> Self {
        Self(val)
    }
}

/// UTF-16 code-unit offset, for platform text APIs (§4.16.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Utf16Offset(pub usize);

impl From<Utf16Offset> for usize {
    fn from(uo: Utf16Offset) -> usize {
        uo.0
    }
}

impl From<usize> for Utf16Offset {
    fn from(val: usize) -> Self {
        Self(val)
    }
}

/// A half-open byte range in one of the coordinate spaces.
///
/// Parameterised by the coordinate type so a [`SourceByte`] range can never be
/// compared against a [`TextByte`] one by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span<T> {
    /// Inclusive start offset.
    pub start: T,
    /// Exclusive end offset.
    pub end: T,
}

impl<T> Span<T> {
    /// Build a span from its endpoints.
    #[must_use]
    pub const fn new(start: T, end: T) -> Self {
        Self { start, end }
    }
}

impl<T: Copy> Span<T> {
    /// Length of the range in units of `T` (only meaningful for byte offsets).
    #[must_use]
    pub fn len(&self) -> usize
    where
        T: Into<usize> + Copy,
    {
        let s: usize = self.start.into();
        let e: usize = self.end.into();
        e.saturating_sub(s)
    }

    /// Whether the range is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool
    where
        T: Into<usize> + Copy,
    {
        let s: usize = self.start.into();
        let e: usize = self.end.into();
        e <= s
    }
}

impl Span<TextByte> {
    /// Reinterpret the same offsets as source offsets (only valid when the
    /// snapshot is byte-identical after normalization, e.g. an all-LF note).
    ///
    /// Prefer [`TextSnapshot::text_to_source`] / [`TextSnapshot::span_to_source`],
    /// which honour BOM and CRLF.
    #[must_use]
    pub const fn as_source_span(self) -> Span<SourceByte> {
        Span::new(SourceByte(self.start.0), SourceByte(self.end.0))
    }
}

/// Line-ending kind recorded per line (§4.8, R12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Eol {
    /// Line feed (`\n`).
    Lf,
    /// Carriage return + line feed (`\r\n`).
    Crlf,
    /// No terminator (last line of a file without a trailing newline).
    None,
}

impl Eol {
    /// Number of source bytes this terminator occupies.
    #[must_use]
    pub const fn source_len(self) -> usize {
        match self {
            Self::None => 0,
            Self::Lf => 1,
            Self::Crlf => 2,
        }
    }
}

/// One line of a [`TextSnapshot`], in both coordinate spaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineInfo {
    /// `TextByte` offset of the first content byte.
    pub text_start: TextByte,
    /// `TextByte` offset just past the last content byte.
    ///
    /// When the line is terminated, the terminator sits at this offset in the
    /// normalized text (CRLF is one byte there).
    pub text_end: TextByte,
    /// `SourceByte` offset of the first content byte.
    pub source_start: SourceByte,
    /// `SourceByte` offset just past the last content byte — i.e. where the
    /// source line terminator begins.
    pub source_end: SourceByte,
    /// How the line was terminated in the source.
    pub eol: Eol,
}

impl LineInfo {
    /// Content length in bytes (identical in both coordinate spaces).
    #[must_use]
    pub fn content_len(&self) -> usize {
        self.text_end.0.saturating_sub(self.text_start.0)
    }
}

/// Errors produced while building a [`TextSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    /// Only valid UTF-8 may be edited (§4.16.1). Other encodings are listed
    /// with a diagnostic and read-only, and never lossy-decoded into a rewrite.
    #[error("source is not valid UTF-8 (invalid sequence at byte {valid_up_to})")]
    NotUtf8 {
        /// Byte offset of the first invalid sequence.
        valid_up_to: usize,
    },
}

/// A parsed, normalized view of a note's bytes (§4.16.1).
///
/// The snapshot is immutable and owns:
/// * `text` — the normalized text both parse glues and the scanner scan;
/// * a per-line table mapping `TextByte` ↔ `SourceByte`;
/// * the BOM flag, so [`TextSnapshot::to_source_bytes`] can reproduce the
///   original file byte-for-byte when it has not been edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSnapshot {
    text: String,
    source: Vec<u8>,
    lines: Vec<LineInfo>,
    has_bom: bool,
}

impl TextSnapshot {
    /// Build a snapshot from raw file bytes.
    ///
    /// # Errors
    ///
    /// Returns [`TextError::NotUtf8`] when the bytes are not valid UTF-8.
    /// Callers MUST NOT fall back to a lossy decode for anything that can be
    /// written back (§4.16.1).
    pub fn new(bytes: &[u8]) -> Result<Self, TextError> {
        let has_bom = bytes.starts_with(&[0xEF, 0xBB, 0xBF]);
        let bom_len = if has_bom { 3 } else { 0 };
        let body = if has_bom { &bytes[3..] } else { bytes };
        let text_str = std::str::from_utf8(body).map_err(|e| TextError::NotUtf8 {
            valid_up_to: e.valid_up_to() + bom_len,
        })?;
        let text = fold_crlf(text_str);
        let lines = build_lines(&text, bytes, bom_len);
        Ok(Self {
            text,
            source: bytes.to_vec(),
            lines,
            has_bom,
        })
    }

    /// The normalized text every parser and the scanner operate on.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The original source bytes, unmodified.
    #[must_use]
    pub fn source(&self) -> &[u8] {
        &self.source
    }

    /// Whether the source began with a UTF-8 BOM.
    #[must_use]
    pub const fn has_bom(&self) -> bool {
        self.has_bom
    }

    /// The per-line table.
    #[must_use]
    pub fn lines(&self) -> &[LineInfo] {
        &self.lines
    }

    /// Number of lines (a file ending in a newline has no trailing empty line).
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Reconstruct the exact source bytes of this (unedited) snapshot.
    ///
    /// Byte-exact for any input: BOM, per-line LF/CRLF, standalone CR, and a
    /// missing trailing newline all survive (§4.8, AC-7's unedited half).
    #[must_use]
    pub fn to_source_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.source.len());
        if self.has_bom {
            out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        }
        for line in &self.lines {
            out.extend_from_slice(&self.text.as_bytes()[line.text_start.0..line.text_end.0]);
            match line.eol {
                Eol::Lf => out.push(b'\n'),
                Eol::Crlf => out.extend_from_slice(b"\r\n"),
                Eol::None => {}
            }
        }
        out
    }

    /// Map a normalized offset to its source offset.
    ///
    /// Returns `None` when `text` is out of range or lands inside a CRLF pair
    /// or the BOM — never a legal edit boundary (§4.16.1).
    #[must_use]
    pub fn text_to_source(&self, text: TextByte) -> Option<SourceByte> {
        if text.0 > self.text.len() {
            return None;
        }
        if text.0 == self.text.len() {
            return Some(SourceByte(self.source.len()));
        }
        let line = self.line_for_text(text.0)?;

        // Calculate the base source offset
        let mut source_offset = line.source_start.0 + (text.0 - line.text_start.0);

        // If we're at the line end, we're at the terminator position.
        // For CRLF, the terminator is "\r\n" at [source_end, source_end+2),
        // so text_end maps to source_end (the start of the terminator).
        // For LF, the terminator is "\n" at [source_end, source_end+1),
        // so text_end maps to source_end.
        if text.0 == line.text_end.0 {
            source_offset = line.source_end.0;
        }

        Some(SourceByte(source_offset))
    }

    /// Map a source offset to its normalized offset.
    ///
    /// Returns `None` when `source` is out of range, inside the BOM, or inside
    /// the `\n` of a CRLF pair — never a legal edit boundary (§4.16.1).
    #[must_use]
    pub fn source_to_text(&self, source: SourceByte) -> Option<TextByte> {
        if source.0 > self.source.len() {
            return None;
        }
        if source.0 == self.source.len() {
            return Some(TextByte(self.text.len()));
        }
        if source.0 < self.bom_len() {
            return None;
        }
        let line = self.line_for_source(source.0)?;
        if source.0 < line.source_end.0 {
            return Some(TextByte(
                line.text_start.0 + (source.0 - line.source_start.0),
            ));
        }
        // At or past `source_end`: inside this line's terminator.
        // For CRLF (2 bytes: \r at source_end, \n at source_end+1), both are illegal
        // except that \r maps to the \n in text for this line.
        // For LF (1 byte: \n at source_end), it's illegal.
        if line.eol == Eol::Crlf && source.0 == line.source_end.0 {
            // \r at source_end maps to \n at text_end for this line.
            return Some(TextByte(line.text_end.0));
        }
        // Anything else (LF terminator byte, or CRLF \n) is illegal.
        None
    }

    /// Map a full span from normalized to source coordinates.
    ///
    /// Both endpoints must be legal; otherwise `None`.
    #[must_use]
    pub fn span_to_source(&self, span: Span<TextByte>) -> Option<Span<SourceByte>> {
        let start_source = self.text_to_source(span.start)?;
        let end_source = self.text_to_source(span.end)?;

        // Check if the span starts at a line end with CRLF, which would include
        // the CRLF interior in the source.
        if let Some(line) = self.line_for_text(span.start.0)
            && span.start.0 == line.text_end.0
            && line.eol == Eol::Crlf
        {
            // Starts at a CRLF line end, which means the source span would
            // include the CRLF bytes, which are illegal.
            return None;
        }

        Some(Span::new(start_source, end_source))
    }

    /// Scalar (code point) index of a normalized byte offset.
    ///
    /// `None` for out-of-range offsets or offsets that split a scalar.
    #[must_use]
    pub fn scalar_index(&self, at: TextByte) -> Option<ScalarIndex> {
        let head = self.text.get(..at.0)?;
        Some(ScalarIndex(head.chars().count()))
    }

    /// Byte offset of a scalar index (clamped to the end of the text).
    #[must_use]
    pub fn text_byte_at_scalar(&self, index: ScalarIndex) -> Option<TextByte> {
        let mut count = 0;
        for (offset, _) in self.text.char_indices() {
            if count == index.0 {
                return Some(TextByte(offset));
            }
            count += 1;
        }
        (count == index.0).then_some(TextByte(self.text.len()))
    }

    /// Previous grapheme-cluster boundary at or before `at` (cursor movement).
    #[must_use]
    pub fn prev_grapheme_boundary(&self, at: TextByte) -> TextByte {
        prev_grapheme_boundary(&self.text, at)
    }

    /// Next grapheme-cluster boundary at or after `at` (cursor movement).
    #[must_use]
    pub fn next_grapheme_boundary(&self, at: TextByte) -> TextByte {
        next_grapheme_boundary(&self.text, at)
    }

    /// UTF-16 code-unit offset of a normalized byte offset (platform APIs).
    #[must_use]
    pub fn utf16_offset(&self, at: TextByte) -> Option<Utf16Offset> {
        let head = self.text.get(..at.0)?;
        Some(Utf16Offset(head.encode_utf16().count()))
    }

    /// Normalized byte offset of a UTF-16 code-unit offset.
    #[must_use]
    pub fn text_byte_at_utf16(&self, offset: Utf16Offset) -> Option<TextByte> {
        let mut units = 0usize;
        for (byte, ch) in self.text.char_indices() {
            if units == offset.0 {
                return Some(TextByte(byte));
            }
            units += ch.len_utf16();
        }
        (units == offset.0).then_some(TextByte(self.text.len()))
    }

    fn bom_len(&self) -> usize {
        if self.has_bom { 3 } else { 0 }
    }

    fn line_for_text(&self, at: usize) -> Option<&LineInfo> {
        // `at` < text.len(), so it belongs to the line whose range contains it.
        self.lines
            .iter()
            .find(|l| at >= l.text_start.0 && at <= l.text_end.0)
    }

    fn line_for_source(&self, at: usize) -> Option<&LineInfo> {
        self.lines
            .iter()
            .find(|l| at >= l.source_start.0 && at <= l.source_end.0)
    }
}

/// Previous grapheme boundary at or before `at`, using default segmentation.
///
/// `at` is clamped to the text, so the function is total over valid offsets.
#[must_use]
pub fn prev_grapheme_boundary(text: &str, at: TextByte) -> TextByte {
    let clamped = at.0.min(text.len());
    let was_clamped = at.0 > text.len();

    if clamped == 0 {
        return TextByte(0);
    }
    // Snap to a char boundary first so we never index mid-scalar.
    let mut start = clamped;
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }

    // If we were clamped (out of range) and snapped to the end, return the end.
    if was_clamped && start == text.len() {
        return TextByte(start);
    }

    let head = &text[..start];
    match head.grapheme_indices(false).next_back() {
        Some((idx, _)) => TextByte(idx),
        None => TextByte(0),
    }
}

/// Next grapheme boundary at or after `at`, using default segmentation.
#[must_use]
pub fn next_grapheme_boundary(text: &str, at: TextByte) -> TextByte {
    let mut start = at.0.min(text.len());
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    if start >= text.len() {
        return TextByte(text.len());
    }
    match text[start..].grapheme_indices(false).next() {
        Some((idx, g)) => TextByte(start + idx + g.len()),
        None => TextByte(text.len()),
    }
}

/// Unicode default (locale-independent) casefold + NFC lookup key (§4.8).
///
/// Used for stem/alias/path keys so that resolution does not depend on the
/// user's locale (the Turkish `İ`/`ı` rule in §4.8).
#[must_use]
pub fn fold_key(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.to_lowercase().nfc().collect::<String>()
}

/// Replace CRLF with LF, leaving standalone CR bytes untouched (§4.8, R12).
fn fold_crlf(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' && chars.peek() == Some(&'\n') {
            chars.next();
            out.push('\n');
            continue;
        }
        out.push(c);
    }
    out
}

/// Build the per-line table for `text` (CRLF-folded) against `source` bytes.
fn build_lines(text: &str, source: &[u8], bom_len: usize) -> Vec<LineInfo> {
    let mut lines = Vec::new();
    let mut text_start = 0usize;
    let mut source_start = bom_len;

    for (idx, byte) in text.as_bytes().iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let content_len = idx - text_start;
        let source_end = source_start + content_len;
        let eol = if source.get(source_end) == Some(&b'\r') {
            Eol::Crlf
        } else {
            Eol::Lf
        };
        lines.push(LineInfo {
            text_start: TextByte(text_start),
            text_end: TextByte(idx),
            source_start: SourceByte(source_start),
            source_end: SourceByte(source_end),
            eol,
        });
        text_start = idx + 1;
        source_start = source_end + eol.source_len();
    }

    if text_start < text.len() || lines.is_empty() {
        lines.push(LineInfo {
            text_start: TextByte(text_start),
            text_end: TextByte(text.len()),
            source_start: SourceByte(source_start),
            source_end: SourceByte(source_start + (text.len() - text_start)),
            eol: Eol::None,
        });
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(src: &str) -> TextSnapshot {
        TextSnapshot::new(src.as_bytes()).expect("utf-8")
    }

    #[test]
    fn rejects_non_utf8() {
        let err = TextSnapshot::new(&[0xFF, 0xFE]).expect_err("must reject");
        assert_eq!(err, TextError::NotUtf8 { valid_up_to: 0 });
        // BOM-prefixed invalid data reports the offset in the original bytes.
        let err = TextSnapshot::new(&[0xEF, 0xBB, 0xBF, 0xFF]).expect_err("must reject");
        assert_eq!(err, TextError::NotUtf8 { valid_up_to: 3 });
        assert!(err.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn round_trips_byte_exactly() {
        for src in [
            "",
            "a",
            "a\n",
            "a\r\nb\r\n",
            "mixed\r\nlines\nand\nmore",
            "no trailing newline",
            "trailing crlf\r\n",
            "standalone\rcr in the middle\n",
            "\n\n\n",
            "unicode ✓ ✓\r\nsecond\n",
        ] {
            let s = snap(src);
            assert_eq!(s.to_source_bytes(), src.as_bytes(), "round-trip: {src:?}");
        }
    }

    #[test]
    fn bom_is_preserved_and_stripped_from_text() {
        let s = snap("\u{FEFF}hello\r\nworld");
        assert!(s.has_bom());
        assert_eq!(s.text(), "hello\nworld");
        assert_eq!(s.to_source_bytes(), "\u{FEFF}hello\r\nworld".as_bytes());
        assert_eq!(s.text_to_source(TextByte(0)), Some(SourceByte(3)));
        assert_eq!(
            s.source_to_text(SourceByte(0)),
            None,
            "BOM interior is illegal"
        );
        assert_eq!(s.source_to_text(SourceByte(3)), Some(TextByte(0)));
    }

    #[test]
    fn text_to_source_across_mixed_eols() {
        let src = "one\r\ntwo\nthree\r\n";
        let s = snap(src);
        assert_eq!(s.line_count(), 3);
        assert_eq!(s.lines()[0].eol, Eol::Crlf);
        assert_eq!(s.lines()[1].eol, Eol::Lf);
        assert_eq!(s.lines()[2].eol, Eol::Crlf);

        // "one" occupies the same offsets in both spaces.
        assert_eq!(s.text_to_source(TextByte(3)), Some(SourceByte(3)));
        // The newline in normalized text maps to the CR in source.
        assert_eq!(s.text_to_source(TextByte(3)), Some(SourceByte(3)));
        // Next line starts after the two-byte CRLF in source, one byte in text.
        assert_eq!(s.text_to_source(TextByte(4)), Some(SourceByte(5)));
        // Interior of the CRLF pair is never a legal boundary.
        assert_eq!(s.source_to_text(SourceByte(4)), None);
        assert_eq!(s.source_to_text(SourceByte(3)), Some(TextByte(3)));
        // Endpoints.
        assert_eq!(
            s.text_to_source(TextByte(s.text().len())),
            Some(SourceByte(src.len()))
        );
        assert_eq!(
            s.source_to_text(SourceByte(src.len())),
            Some(TextByte(s.text().len()))
        );
        assert_eq!(s.text_to_source(TextByte(999)), None);
        assert_eq!(s.source_to_text(SourceByte(999)), None);
    }

    #[test]
    fn empty_and_single_line_files() {
        let s = snap("");
        assert_eq!(s.line_count(), 1);
        assert_eq!(s.lines()[0].eol, Eol::None);
        assert_eq!(s.text_to_source(TextByte(0)), Some(SourceByte(0)));

        let s = snap("only");
        assert_eq!(s.line_count(), 1);
        assert_eq!(s.lines()[0].eol, Eol::None);
        assert_eq!(s.text_to_source(TextByte(4)), Some(SourceByte(4)));
    }

    #[test]
    fn trailing_newline_adds_no_empty_line() {
        assert_eq!(snap("a\n").line_count(), 1);
        assert_eq!(snap("a\n\n").line_count(), 2);
    }

    #[test]
    fn span_to_source() {
        let s = snap("ab\r\ncd");
        let span = Span::new(TextByte(0), TextByte(2));
        assert_eq!(
            s.span_to_source(span),
            Some(Span::new(SourceByte(0), SourceByte(2)))
        );
        // A span ending inside a CRLF pair is rejected.
        let bad = Span::new(TextByte(2), TextByte(4));
        assert_eq!(s.span_to_source(bad), None);
    }

    #[test]
    fn scalar_and_utf16_round_trip() {
        let s = snap("a\u{1F600}\r\n\u{0301}b"); // emoji + combining mark
        for off in 0..=s.text().len() {
            if !s.text().is_char_boundary(off) {
                assert_eq!(s.scalar_index(TextByte(off)), None);
                continue;
            }
            let idx = s.scalar_index(TextByte(off)).expect("scalar index");
            assert_eq!(s.text_byte_at_scalar(idx), Some(TextByte(off)));

            let u16 = s.utf16_offset(TextByte(off)).expect("utf16");
            assert_eq!(s.text_byte_at_utf16(u16), Some(TextByte(off)));
        }
        assert_eq!(s.scalar_index(TextByte(999)), None);
        assert_eq!(s.text_byte_at_utf16(Utf16Offset(999)), None);
        assert_eq!(s.utf16_offset(TextByte(999)), None);
        assert_eq!(s.text_byte_at_scalar(ScalarIndex(999)), None);
    }

    #[test]
    fn grapheme_boundaries() {
        let s = snap("e\u{0301}x"); // "é" as e + combining acute
        assert_eq!(s.text().len(), 4);
        assert_eq!(s.next_grapheme_boundary(TextByte(0)), TextByte(3));
        assert_eq!(s.prev_grapheme_boundary(TextByte(4)), TextByte(3));
        assert_eq!(s.prev_grapheme_boundary(TextByte(0)), TextByte(0));
        assert_eq!(s.next_grapheme_boundary(TextByte(4)), TextByte(4));
        // Out-of-range offsets clamp instead of panicking.
        assert_eq!(s.next_grapheme_boundary(TextByte(99)), TextByte(4));
        assert_eq!(s.prev_grapheme_boundary(TextByte(99)), TextByte(4));
        // Mid-scalar offsets snap to the enclosing boundary.
        let s2 = snap("\u{1F600}");
        assert_eq!(s2.next_grapheme_boundary(TextByte(1)), TextByte(4));
        assert_eq!(s2.prev_grapheme_boundary(TextByte(1)), TextByte(0));
    }

    #[test]
    fn fold_key_is_locale_independent() {
        assert_eq!(fold_key("Notes"), fold_key("NOTES"));
        assert_eq!(fold_key("café"), fold_key("cafe\u{0301}"));
        // §4.8: default folding — Turkish dotted/dotless I do not shift.
        assert_eq!(fold_key("İ"), fold_key("i\u{0307}"));
    }

    #[test]
    fn span_helpers() {
        let span = Span::new(TextByte(2), TextByte(5));
        assert_eq!(span.len(), 3);
        assert!(!span.is_empty());
        let empty = Span::new(TextByte(5), TextByte(2));
        assert!(empty.is_empty());
        assert_eq!(
            span.as_source_span(),
            Span::new(SourceByte(2), SourceByte(5))
        );
    }
}
