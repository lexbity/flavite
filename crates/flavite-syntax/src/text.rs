//! Text snapshots and coordinate types (§4.16.1, RFC-0006 §6.10).
//!
//! Flavite decodes strict ASCII/UTF-8/UTF-16 note bytes at the
//! [`flavite_encoding`] boundary, normalizes CRLF to LF, removes only the BOM
//! signature selected by the decode policy, and *remembers* exactly what the
//! source bytes and descriptor were, so that an unedited note round-trips
//! byte-exactly and a mixed-EOL note is never silently normalized (§4.8,
//! R12, FR-22/23). Everything downstream — the scanner, both parse glues,
//! the buffer — works in [`TextByte`] coordinates over [`TextSnapshot::text`]
//! and maps back to [`SourceByte`] when it needs original encoded bytes.
//!
//! The `TextByte ↔ SourceByte` mapping composes two checked layers:
//!
//! ```text
//! SourceByte: original encoded file bytes, including BOM and physical EOLs
//!        ↕ codec DecodeMap: endian/scalar boundaries, no newline folding
//! decoded UTF-8 bytes: BOM removed, original CR/LF scalars preserved
//!        ↕ syntax EOL map (the line table's fold prefixes): CRLF → LF
//! TextByte: normalized UTF-8 used by editor, scanner, search and buffer edits
//! ```
//!
//! Distinct newtypes keep the coordinate spaces from being mixed up:
//!
//! | Type | Meaning |
//! |---|---|
//! | [`SourceByte`] | offset into the original encoded source bytes |
//! | [`TextByte`] | offset into the normalized text (CRLF → LF, BOM removed) |
//! | [`ScalarIndex`] | Unicode scalar (code point) index |
//! | [`GraphemeIndex`] | grapheme-cluster index (cursor movement, §4.16.1) |
//! | [`Utf16Offset`] | UTF-16 code-unit offset for platform text APIs — NOT
//!   a file offset; it never names bytes of a UTF-16 encoded note (§6.10) |
//!
//! Every mapping in this module is total at *legal edit boundaries* and
//! `None` at illegal ones (the interior of an encoded scalar, a BOM, a
//! surrogate pair, or a CRLF pair; an odd UTF-16 byte position; or outside
//! the buffer): "BOM and CRLF interiors are never edit boundaries"
//! (§4.16.1, §6.10, AC-24). EOF is always a valid paired boundary.

use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

use flavite_encoding::{
    DecodeLimits, DecodeMap, DecodePolicy, EncodeLimits, SourceFormat, decode as codec_decode,
    encode as codec_encode,
};

/// Return the longest prefix of `text` whose UTF-8 byte length is at most
/// `max_bytes`.
///
/// This is intended for byte-capped previews. The returned slice always ends
/// at a character boundary and never allocates.
#[must_use]
pub fn safe_prefix(text: &str, max_bytes: usize) -> &str {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

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
///
/// Every decode failure is the underlying typed codec error: malformed bytes
/// are reported with their source position and never replaced by a lossy or
/// empty fallback (FR-22, §6.10).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    /// Strict decoding failed under the applied policy.
    #[error(transparent)]
    Codec(#[from] flavite_encoding::CodecError),
}

/// A parsed, normalized view of a note's bytes (§4.16.1, RFC-0006 §6.10).
///
/// The snapshot is immutable and owns:
/// * `text` — the normalized text both parse glues and the scanner scan;
/// * a per-line table mapping `TextByte` ↔ `SourceByte` through the composed
///   codec + EOL map;
/// * the [`SourceFormat`] descriptor (encoding + BOM) for disk-backed
///   snapshots, so [`TextSnapshot::to_source_bytes`] can reproduce the
///   original file byte-for-byte when it has not been edited.
///
/// Snapshots built by [`TextSnapshot::from_normalized`] are a *synthetic*
/// UTF-8 view: they carry no disk-decoding claim and MUST NOT determine a
/// live buffer's save format (§6.10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSnapshot {
    text: Arc<str>,
    source: Arc<[u8]>,
    lines: Arc<[LineInfo]>,
    /// Per line, the count of CRLF pairs folded before that line's content
    /// starts. The line's decoded-region byte offset is
    /// `text_start + fold_prefix[line]`. Always zeros for synthetic views.
    fold_prefix: Arc<[u32]>,
    /// Codec boundary map for disk-backed snapshots; `None` for synthetic
    /// normalized views (whose source is the normalized UTF-8 itself).
    disk: Option<DecodeMap>,
    /// Save-format intent on synthetic views (see
    /// [`TextSnapshot::from_normalized_with_descriptor`]); ignored when a
    /// disk-backed `map` is present.
    descriptor_override: Option<SourceFormat>,
}

impl TextSnapshot {
    /// Build from an existing Session/TextByte string without normalizing it
    /// again. A preserved standalone CR before an LF is content here, and a
    /// leading BOM character may also be real edited content.
    ///
    /// The result is a synthetic UTF-8 view: `format()` is `None` and it
    /// carries no disk-decoding authority (§6.10).
    #[must_use]
    pub fn from_normalized(text: &str) -> Self {
        let source = text.as_bytes().to_vec();
        let lines = build_lines_identity(text, &source, 0);
        let fold_prefix = vec![0u32; lines.len()];
        Self {
            text: text.into(),
            source: source.into(),
            lines: lines.into(),
            fold_prefix: fold_prefix.into(),
            disk: None,
            descriptor_override: None,
        }
    }

    /// Build a snapshot from raw file bytes with the `Auto` decode policy:
    /// a recognized BOM signature is accepted and preserved; BOM-less bytes
    /// must be valid UTF-8. No endianness is ever guessed (§6.10).
    ///
    /// # Errors
    ///
    /// Returns the typed [`TextError`] of a strict codec failure. Callers MUST
    /// NOT fall back to a lossy decode for anything that can be written back
    /// (§4.16.1, FR-22).
    pub fn new(bytes: &[u8]) -> Result<Self, TextError> {
        Self::with_policy(bytes, DecodePolicy::Auto)
    }

    /// Build a snapshot from raw file bytes under an explicit policy
    /// (user-confirmed encoding, or a captured descriptor on reload).
    ///
    /// # Errors
    ///
    /// Typed strict-decode failure; see [`TextSnapshot::new`].
    pub fn with_policy(bytes: &[u8], policy: DecodePolicy) -> Result<Self, TextError> {
        Self::decoding(bytes, policy, &DecodeLimits::default_envelope(), &mut || false)
    }

    /// [`TextSnapshot::with_policy`] with caller-supplied byte limits and a
    /// cancellation probe (checked at 64 KiB progress granularity, NFR-19).
    ///
    /// # Errors
    ///
    /// Typed strict-decode failure or [`flavite_encoding::CodecError::Cancelled`].
    pub fn decoding(
        bytes: &[u8],
        policy: DecodePolicy,
        limits: &DecodeLimits,
        cancel: &mut dyn FnMut() -> bool,
    ) -> Result<Self, TextError> {
        let decoded = codec_decode(bytes, policy, limits, cancel)?;
        Ok(Self::from_decoded(decoded))
    }

    /// Wrap an already validated decode. This is the only sanctioned way to
    /// attach a disk-backed snapshot: the raw bytes are never re-decoded by a
    /// second normalization pass (§4.1).
    #[must_use]
    pub fn from_decoded(decoded: flavite_encoding::DecodedContent) -> Self {
        let map = decoded.map().clone();
        let decoded_text = decoded.text();
        let (lines, fold_prefix) = build_lines_encoded(decoded_text, &map);
        Self {
            text: Arc::from(fold_crlf(decoded_text)),
            source: Arc::clone(decoded.source()),
            lines: lines.into(),
            fold_prefix: fold_prefix.into(),
            disk: Some(map),
            descriptor_override: None,
        }
    }

    /// Synthetic normalized view that additionally carries the *save-format
    /// intent* (descriptor) for the buffer it belongs to.
    ///
    /// Used only for undo/redo lineage of drafts that are not currently
    /// encodable under their descriptor (strict-ASCII policy with non-ASCII
    /// content): `source()` is the decoded UTF-8 — NOT the serialized form of
    /// the format — and MUST NOT be treated as evidence of a saved file.
    /// Saving such a buffer fails with `Unencodable` before any file effect
    /// (§6.10, FR-22).
    #[must_use]
    pub fn from_normalized_with_descriptor(text: &str, format: SourceFormat) -> Self {
        let mut snapshot = Self::from_normalized(text);
        snapshot.descriptor_override = Some(format);
        snapshot
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

    /// The authoritative disk descriptor, or `None` for a synthetic
    /// normalized view (which must not determine a save format, §6.10).
    /// A pending-encoding draft view reports its save-format intent here.
    #[must_use]
    pub fn format(&self) -> Option<SourceFormat> {
        self.disk
            .as_ref()
            .map(DecodeMap::format)
            .or(self.descriptor_override)
    }

    /// The composed codec boundary map for disk-backed snapshots.
    #[must_use]
    pub fn map(&self) -> Option<&DecodeMap> {
        self.disk.as_ref()
    }

    /// Whether the source began with a BOM signature (removed at decode).
    #[must_use]
    pub fn has_bom(&self) -> bool {
        self.format().is_some_and(|f| f.bom())
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
    /// Byte-exact for any input: descriptor BOM, per-line LF/CRLF, standalone
    /// CR, and a missing trailing newline all survive (§4.8, §6.10, AC-23).
    /// An unedited disk-backed snapshot's content is, by construction,
    /// representable under its own descriptor.
    ///
    /// # Panics
    ///
    /// Only on an internal impossibility: an unedited snapshot failing to
    /// re-encode under the descriptor it was decoded from.
    #[must_use]
    pub fn to_source_bytes(&self) -> Vec<u8> {
        match &self.disk {
            None => self.reconstruct_identity(&[]),
            Some(map) => {
                let format = map.format();
                if format.encoding().is_utf16() {
                    let decoded = self.reconstruct_decoded();
                    codec_encode(
                        &decoded,
                        &format,
                        &EncodeLimits {
                            max_input_utf8_bytes: u64::MAX,
                            max_output_bytes: u64::MAX,
                        },
                        &mut || false,
                    )
                    .expect("an unedited snapshot re-encodes under its own descriptor")
                } else {
                    self.reconstruct_identity(format.bom_bytes())
                }
            }
        }
    }

    /// Identity-family (ASCII/UTF-8) reconstruction: content slices plus the
    /// physical terminator bytes recorded per line.
    fn reconstruct_identity(&self, bom: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bom.len() + self.source.len());
        out.extend_from_slice(bom);
        for line in self.lines.iter() {
            out.extend_from_slice(&self.text.as_bytes()[line.text_start.0..line.text_end.0]);
            match line.eol {
                Eol::Lf => out.push(b'\n'),
                Eol::Crlf => out.extend_from_slice(b"\r\n"),
                Eol::None => {}
            }
        }
        out
    }

    /// Reassemble the decoded UTF-8 region (BOM absent, physical CR/LF
    /// scalars restored) from normalized text + per-line EOL kinds.
    fn reconstruct_decoded(&self) -> String {
        let mut out = String::with_capacity(self.text.len() + self.fold_prefix.len());
        for line in self.lines.iter() {
            out.push_str(&self.text[line.text_start.0..line.text_end.0]);
            match line.eol {
                Eol::Lf => out.push('\n'),
                Eol::Crlf => out.push_str("\r\n"),
                Eol::None => {}
            }
        }
        out
    }

    /// Map a normalized offset to its encoded source offset through the
    /// composed codec + EOL map.
    ///
    /// Returns `None` when `text` is out of range or lands inside an encoded
    /// scalar, the CRLF pair, or the BOM — never a legal edit boundary
    /// (§4.16.1, §6.10).
    #[must_use]
    pub fn text_to_source(&self, text: TextByte) -> Option<SourceByte> {
        if text.0 > self.text.len() {
            return None;
        }
        if text.0 == self.text.len() {
            return Some(SourceByte(self.source.len()));
        }
        let idx = self.line_index_for_text(text.0)?;
        let line = &self.lines[idx];
        // At the line's end we are at the terminator's start position: for
        // CRLF that is the `\r`; for LF the `\n` itself (the inverse mapping
        // refuses the LF byte again, as before).
        if text.0 == line.text_end.0 {
            return Some(line.source_end);
        }
        // Inside line content: fold the text offset to the decoded region,
        // then map through the codec boundary table.
        let fold = usize::try_from(self.fold_prefix[idx]).unwrap_or(usize::MAX);
        let decoded = line.text_start.0.checked_add(fold)?.checked_add(text.0 - line.text_start.0)?;
        self.decoded_to_source(decoded).map(SourceByte)
    }

    /// Map an encoded source offset to its normalized offset through the
    /// composed codec + EOL map.
    ///
    /// Returns `None` when `source` is out of range, inside the BOM, inside an
    /// encoded scalar or surrogate pair, at an odd UTF-16 byte, or inside the
    /// `\n` of a CRLF pair — never a legal edit boundary (§4.16.1, §6.10).
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
        let idx = self.line_index_for_source(source.0)?;
        let line = &self.lines[idx];
        if source.0 == line.source_end.0 {
            // Terminator start: the `\r` of a CRLF maps to the folded LF
            // position in text; the LF byte/unit itself is not addressable.
            return if line.eol == Eol::Crlf {
                Some(line.text_end)
            } else {
                None
            };
        }
        if source.0 > line.source_end.0 {
            // Inside the terminator (CRLF's LF side) — illegal.
            return None;
        }
        // Line content region.
        let decoded = self.source_to_decoded(source.0)?;
        let fold = usize::try_from(self.fold_prefix[idx]).ok()?;
        let text_pos = decoded.checked_sub(fold)?;
        // Defensive: the found line must contain the mapped decoded position.
        if text_pos < line.text_start.0 || text_pos >= line.text_end.0 {
            return None;
        }
        Some(TextByte(text_pos))
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
        if let Some(idx) = self.line_index_for_text(span.start.0) {
            let line = &self.lines[idx];
            if span.start.0 == line.text_end.0 && line.eol == Eol::Crlf {
                // Starts at a CRLF line end, which means the source span would
                // include the CRLF bytes, which are illegal.
                return None;
            }
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
        self.disk
            .as_ref()
            .map_or(0, |map| usize::try_from(map.bom_len()).unwrap_or(0))
    }

    /// `SourceByte ↔ decoded-UTF-8 byte` through the codec map; synthetic
    /// views are identity on char boundaries and refuse scalar interiors.
    fn decoded_to_source(&self, decoded: usize) -> Option<usize> {
        match &self.disk {
            Some(map) => usize::try_from(map.decoded_utf8_to_source(decoded as u64)?).ok(),
            None => self.text.is_char_boundary(decoded).then_some(decoded),
        }
    }

    fn source_to_decoded(&self, source: usize) -> Option<usize> {
        match &self.disk {
            Some(map) => usize::try_from(map.source_to_decoded(source as u64)?).ok(),
            None => (self.text.is_char_boundary(source) || source == self.text.len())
                .then_some(source),
        }
    }

    /// Index of the line whose text range `[text_start, text_end]` contains
    /// `at`. Binary: the line table is ascending in both coordinate spaces.
    fn line_index_for_text(&self, at: usize) -> Option<usize> {
        let lines = &self.lines;
        debug_assert!(!lines.is_empty());
        let idx = lines.partition_point(|l| l.text_start.0 <= at).checked_sub(1)?;
        (at <= lines[idx].text_end.0).then_some(idx)
    }

    /// Index of the line whose source range `[source_start, source_end]`
    /// contains `at` (last start ≤ `at`).
    fn line_index_for_source(&self, at: usize) -> Option<usize> {
        let lines = &self.lines;
        debug_assert!(!lines.is_empty());
        let idx = lines
            .partition_point(|l| l.source_start.0 <= at)
            .checked_sub(1)?;
        (at <= lines[idx].source_end.0).then_some(idx)
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

/// Build the per-line table for a synthetic normalized view: identity
/// `text ↔ source` arithmetic with the BOM removed (§4.16.1).
fn build_lines_identity(text: &str, source: &[u8], bom_len: usize) -> Vec<LineInfo> {
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

/// Build the per-line table for a disk-backed snapshot in ONE pass over the
/// decoded region, advancing the encoded cursor by real scalar widths —
/// never by affine byte arithmetic within a line (§6.10; the retired
/// `source_start + (text - text_start)` shortcut is correct only for UTF-8
/// and is exactly what the composed map replaces).
///
/// Returns the line table plus the per-line count of CRLF pairs folded before
/// that line's content (the `text → decoded` offset correction).
fn build_lines_encoded(decoded: &str, map: &DecodeMap) -> (Vec<LineInfo>, Vec<u32>) {
    let bytes = decoded.as_bytes();
    let utf16 = map.encoding().is_utf16();
    let mut lines = Vec::new();
    let mut fold_prefix = Vec::new();
    let mut dec_i = 0usize;
    let mut src_i = usize::try_from(map.bom_len()).unwrap_or(0);
    let mut folded: u32 = 0;
    while let Some(nl_rel) = bytes[dec_i..].iter().position(|b| *b == b'\n') {
        let nl = dec_i + nl_rel;
        let crlf = nl > 0 && bytes[nl - 1] == b'\r';
        let content_end = if crlf { nl - 1 } else { nl };
        let src_start = src_i;
        advance_source(&mut src_i, &decoded[dec_i..content_end], utf16);
        let src_end = src_i;
        fold_prefix.push(folded);
        lines.push(LineInfo {
            // Content start in text space = decoded start minus folds before
            // this line (`folded` is the pre-increment count).
            text_start: TextByte(dec_i - folded as usize),
            text_end: TextByte(content_end - folded as usize),
            source_start: SourceByte(src_start),
            source_end: SourceByte(src_end),
            eol: if crlf { Eol::Crlf } else { Eol::Lf },
        });
        folded += u32::from(crlf);
        // Step over the physical terminator bytes.
        src_i += terminator_source_bytes(crlf, utf16);
        dec_i = nl + 1;
    }
    // Tail: unterminated content — or an empty file, which gets one
    // `Eol::None` line (matching the identity builder).
    if dec_i < bytes.len() || lines.is_empty() {
        let src_start = src_i;
        advance_source(&mut src_i, &decoded[dec_i..], utf16);
        fold_prefix.push(folded);
        lines.push(LineInfo {
            text_start: TextByte(dec_i - folded as usize),
            text_end: TextByte(bytes.len() - folded as usize),
            source_start: SourceByte(src_start),
            source_end: SourceByte(src_i),
            eol: Eol::None,
        });
    }
    (lines, fold_prefix)
}

/// Physical width of a line terminator in the encoded space. CR/LF are
/// always BMP scalars: 1 byte each for the identity families, 2 per code
/// unit for UTF-16.
const fn terminator_source_bytes(crlf: bool, utf16: bool) -> usize {
    match (crlf, utf16) {
        (true, true) => 4,
        (true, false) => 2,
        (false, true) => 2,
        (false, false) => 1,
    }
}

/// Advance an encoded cursor across a decoded region by real scalar widths.
fn advance_source(src: &mut usize, region: &str, utf16: bool) {
    if utf16 {
        *src += region.chars().map(|c| c.len_utf16()).sum::<usize>() * 2;
    } else {
        *src += region.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloning_a_snapshot_shares_immutable_text_source_and_coordinates() {
        let snapshot = TextSnapshot::new("\u{feff}é\r\nsecond\n".as_bytes()).unwrap();
        let cloned = snapshot.clone();
        assert!(Arc::ptr_eq(&snapshot.text, &cloned.text));
        assert!(Arc::ptr_eq(&snapshot.source, &cloned.source));
        assert!(Arc::ptr_eq(&snapshot.lines, &cloned.lines));
        assert_eq!(cloned.to_source_bytes(), snapshot.source());
    }

    fn snap(src: &str) -> TextSnapshot {
        TextSnapshot::new(src.as_bytes()).expect("utf-8")
    }

    #[test]
    fn rejects_non_utf8() {
        use flavite_encoding::CodecError;
        // `FF FE` alone is a valid UTF-16LE BOM signature with empty content;
        // strict rejection applies to malformed UTF-8 without a recognized BOM.
        let snapshot = TextSnapshot::new(&[0xFF, 0xFE]).expect("UTF-16LE BOM is accepted");
        assert_eq!(snapshot.text(), "");
        let err = TextSnapshot::new(&[0x41, 0xFF, 0x42]).expect_err("must reject");
        assert_eq!(err, TextError::Codec(CodecError::InvalidUtf8 { source_byte: 1 }));
        // BOM-prefixed invalid data reports the offset in the original bytes.
        let err = TextSnapshot::new(&[0xEF, 0xBB, 0xBF, 0xFF]).expect_err("must reject");
        assert_eq!(err, TextError::Codec(CodecError::InvalidUtf8 { source_byte: 3 }));
        assert!(err.to_string().contains("invalid UTF-8"));
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

    #[test]
    fn safe_prefix_respects_char_boundaries() {
        for (text, limit, expected) in [
            ("abc", 0, ""),
            ("abc", 2, "ab"),
            ("abc", 9, "abc"),
            ("a界b", 2, "a"),
            ("a界b", 4, "a界"),
            ("😀x", 3, ""),
            ("😀x", 4, "😀"),
        ] {
            let prefix = safe_prefix(text, limit);
            assert_eq!(prefix, expected);
            assert!(prefix.is_char_boundary(prefix.len()));
            assert!(prefix.len() <= limit);
        }

        for limit in [1023, 1024, 1025] {
            let text = format!("{}tail", "界".repeat(400));
            let prefix = safe_prefix(&text, limit);
            assert!(prefix.is_char_boundary(prefix.len()));
            assert!(prefix.len() <= limit);
        }
    }
}
