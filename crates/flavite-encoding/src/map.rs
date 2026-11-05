//! Boundary maps between encoded source bytes and decoded UTF-8 bytes.
//!
//! `SourceByte` positions count the original encoded file bytes (including a
//! BOM and physical EOL bytes); decoded positions count UTF-8 bytes after the
//! BOM was removed, with original CR/LF scalars preserved. Folding CRLF to LF
//! belongs to `flavite-syntax`'s EOL map, which composes on top of this map —
//! this crate performs no newline normalization.
//!
//! The map never copies the whole decoded file: ASCII/UTF-8 mapping is a BOM
//! offset with direct byte-boundary inspection, and UTF-16 uses sparse
//! scalar-boundary checkpoints at most [`MAX_CHECKPOINT_SCALAR_GAP`] scalars
//! apart. A query binary-searches the checkpoint table and then decodes at
//! most 255 scalars locally (NFR-18).

use std::sync::Arc;

use crate::format::{FileEncoding, SourceFormat};

/// Maximum scalar distance between two checkpoints. A local remap therefore
/// decodes at most this many scalars after the binary search.
pub const MAX_CHECKPOINT_SCALAR_GAP: usize = 255;

/// One recorded scalar boundary: absolute decoded-UTF-8 and source byte
/// positions. Offsets fit `u32` because decode limits are bounded well below
/// 4 GiB (NFR-19) and enforced at construction with checked arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    /// Decoded UTF-8 byte offset at a scalar boundary.
    pub decoded_utf8: u32,
    /// Encoded source byte offset of the same boundary.
    pub source: u32,
}

/// Checked `SourceByte ↔ decoded UTF-8 byte` mapping for one immutable
/// encoded source. Cheap to clone: the original bytes are shared, not copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeMap {
    inner: Arc<Inner>,
}

#[derive(Debug, PartialEq, Eq)]
struct Inner {
    encoding: FileEncoding,
    /// The full validated descriptor this map was built under.
    format: SourceFormat,
    /// Byte length of the removed BOM signature (0, 2 or 3).
    bom_len: u32,
    /// Total encoded source byte length (including BOM).
    source_len: u64,
    /// Decoded UTF-8 byte length.
    decoded_len: u64,
    /// The original encoded bytes, shared with the snapshot owner.
    source: Arc<[u8]>,
    /// Sparse scalar-boundary table; empty for ASCII/UTF-8 (identity).
    checkpoints: Vec<Checkpoint>,
}

/// One scalar's size on each side of the map.
#[derive(Debug, Clone, Copy)]
struct Scalar {
    decoded_bytes: u32,
    source_bytes: u32,
}

/// Public scalar-size view returned by [`DecodeMap::scalar_size_at_source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScalarSize {
    /// Bytes this scalar occupies in the decoded UTF-8 stream.
    pub decoded_bytes: u32,
    /// Bytes this scalar occupies in the encoded source.
    pub source_bytes: u32,
}

impl DecodeMap {
    /// Build an ASCII/UTF-8 (identity-offset) map.
    #[allow(clippy::missing_panics_doc)] // constructor never panics; debug asserts are preconditions
    pub(crate) fn identity(
        encoding: FileEncoding,
        format: SourceFormat,
        source: Arc<[u8]>,
        bom_len: u32,
        decoded_len: u64,
    ) -> Self {
        debug_assert!(!encoding.is_utf16());
        debug_assert_eq!(encoding, format.encoding());
        Self {
            inner: Arc::new(Inner {
                encoding,
                format,
                bom_len,
                source_len: source.len() as u64,
                decoded_len,
                source,
                checkpoints: Vec::new(),
            }),
        }
    }

    /// Build a UTF-16 map from a recorded checkpoint table. `checkpoints` must
    /// start at `(0, bom_len)` and be ascending on both axes with gaps of at
    /// most [`MAX_CHECKPOINT_SCALAR_GAP`] scalars.
    pub(crate) fn utf16(
        encoding: FileEncoding,
        format: SourceFormat,
        source: Arc<[u8]>,
        bom_len: u32,
        decoded_len: u64,
        checkpoints: Vec<Checkpoint>,
    ) -> Self {
        debug_assert!(encoding.is_utf16());
        debug_assert_eq!(
            checkpoints.first(),
            Some(&Checkpoint {
                decoded_utf8: 0,
                source: bom_len
            })
        );
        Self {
            inner: Arc::new(Inner {
                encoding,
                format,
                bom_len,
                source_len: source.len() as u64,
                decoded_len,
                source,
                checkpoints,
            }),
        }
    }

    /// The validated descriptor the source was decoded under.
    #[must_use]
    pub fn format(&self) -> SourceFormat {
        self.inner.format
    }

    #[must_use]
    pub fn encoding(&self) -> FileEncoding {
        self.inner.encoding
    }

    /// Byte length of the removed BOM signature.
    #[must_use]
    pub fn bom_len(&self) -> u64 {
        u64::from(self.inner.bom_len)
    }

    /// Total encoded source length, including the BOM.
    #[must_use]
    pub fn source_len(&self) -> u64 {
        self.inner.source_len
    }

    /// Decoded UTF-8 length (BOM removed, physical CR/LF preserved).
    #[must_use]
    pub fn decoded_len(&self) -> u64 {
        self.inner.decoded_len
    }

    /// The shared original bytes.
    #[must_use]
    pub fn source_bytes(&self) -> &Arc<[u8]> {
        &self.inner.source
    }

    /// Recorded checkpoint table (for tests/inspection).
    #[must_use]
    pub fn checkpoints(&self) -> &[Checkpoint] {
        &self.inner.checkpoints
    }

    /// Approximate heap overhead of the map itself (NFR-18 accounting).
    #[must_use]
    pub fn map_bytes(&self) -> usize {
        self.inner.checkpoints.len() * std::mem::size_of::<Checkpoint>()
    }

    /// Map an encoded source offset to its decoded UTF-8 offset.
    ///
    /// Returns `None` for positions inside the BOM, inside an encoded scalar
    /// or surrogate pair, odd UTF-16 byte positions, and non-scalar UTF-8
    /// continuation positions. EOF is always a valid paired boundary.
    #[must_use]
    pub fn source_to_decoded(&self, source: u64) -> Option<u64> {
        let inner = &self.inner;
        if source > inner.source_len {
            return None;
        }
        if source == inner.source_len {
            // EOF is always the valid pair of decoded EOF.
            return Some(inner.decoded_len);
        }
        if source < u64::from(inner.bom_len) {
            // Before or inside the BOM signature: no decoded meaning.
            return None;
        }
        if !inner.encoding.is_utf16() {
            let d = source - u64::from(inner.bom_len);
            if d == inner.decoded_len {
                return Some(d);
            }
            if is_utf8_continuation(inner.source[d as usize + inner.bom_len as usize]) {
                return None;
            }
            return Some(d);
        }
        self.walk_source_to_decoded(source)
    }

    /// Map a decoded UTF-8 offset to its encoded source offset.
    ///
    /// Returns `None` for out-of-range positions and for offsets inside an
    /// encoded scalar (UTF-8 continuation positions never occur because the
    /// decoded domain is produced by strict decoding; the check exists for
    /// callers composing with their own folded offsets).
    #[must_use]
    pub fn decoded_utf8_to_source(&self, decoded: u64) -> Option<u64> {
        let inner = &self.inner;
        if decoded > inner.decoded_len {
            return None;
        }
        if decoded == inner.decoded_len {
            return Some(inner.source_len);
        }
        if !inner.encoding.is_utf16() {
            let s = decoded + u64::from(inner.bom_len);
            if is_utf8_continuation(inner.source[s as usize]) {
                return None;
            }
            return Some(s);
        }
        self.walk_decoded_to_source(decoded)
    }

    /// Whether a source offset is a legal boundary (BOM end, scalar start,
    /// terminator start, or EOF).
    #[must_use]
    pub fn is_valid_source_boundary(&self, source: u64) -> bool {
        self.source_to_decoded(source).is_some()
    }

    /// Whether a decoded offset is a legal scalar boundary.
    #[must_use]
    pub fn is_valid_decoded_boundary(&self, decoded: u64) -> bool {
        self.decoded_utf8_to_source(decoded).is_some()
    }

    fn walk_source_to_decoded(&self, target: u64) -> Option<u64> {
        let inner = &self.inner;
        let start = self
            .checkpoint_for(target, |c| c.source)
            .copied()
            .unwrap_or(Checkpoint {
                decoded_utf8: 0,
                source: inner.bom_len,
            });
        if u64::from(start.source) > target {
            return None;
        }
        let mut pos = u64::from(start.source);
        let mut dec = u64::from(start.decoded_utf8);
        while pos < target {
            let scalar = self.scalar_at_source(pos)?;
            pos = pos.checked_add(u64::from(scalar.source_bytes))?;
            dec = dec.checked_add(u64::from(scalar.decoded_bytes))?;
        }
        if pos == target {
            Some(dec)
        } else {
            // Overshot: `target` is inside a scalar (or an odd byte).
            None
        }
    }

    fn walk_decoded_to_source(&self, target: u64) -> Option<u64> {
        let inner = &self.inner;
        let start = self
            .checkpoint_for(target, |c| c.decoded_utf8)
            .copied()
            .unwrap_or(Checkpoint {
                decoded_utf8: 0,
                source: inner.bom_len,
            });
        if u64::from(start.decoded_utf8) > target {
            return None;
        }
        let mut pos = u64::from(start.source);
        let mut dec = u64::from(start.decoded_utf8);
        while dec < target {
            let scalar = self.scalar_at_source(pos)?;
            pos = pos.checked_add(u64::from(scalar.source_bytes))?;
            dec = dec.checked_add(u64::from(scalar.decoded_bytes))?;
        }
        if dec == target { Some(pos) } else { None }
    }

    /// Largest checkpoint with `key(checkpoint) <= target`.
    fn checkpoint_for(&self, target: u64, key: impl Fn(&Checkpoint) -> u32) -> Option<&Checkpoint> {
        let cps = &self.inner.checkpoints;
        if cps.is_empty() {
            return None;
        }
        // partition_point: first index where key > target.
        let idx = cps.partition_point(|c| u64::from(key(c)) <= target);
        if idx == 0 { None } else { Some(&cps[idx - 1]) }
    }

    /// Size of one scalar starting at an absolute source position, for
    /// consumers (like `flavite-syntax`'s composed line walk) that advance a
    /// decoded region in single passes. `None` marks a position that is not a
    /// scalar start (or is out of range).
    #[must_use]
    pub fn scalar_size_at_source(&self, pos: u64) -> Option<ScalarSize> {
        self.scalar_at_source(pos).map(|s| ScalarSize {
            decoded_bytes: s.decoded_bytes,
            source_bytes: s.source_bytes,
        })
    }

    /// Size of one scalar starting at an absolute source position. The source
    /// was strictly decoded, so surrogate pairs are well-formed here.
    fn scalar_at_source(&self, pos: u64) -> Option<Scalar> {
        let inner = &self.inner;
        let bytes = &inner.source;
        let p = usize::try_from(pos).ok()?;
        if !inner.encoding.is_utf16() {
            // Identity family: decoded and source widths are equal.
            if p == inner.source.len() {
                return None; // EOF is not a scalar start
            }
            let lead = bytes[p];
            if is_utf8_continuation(lead) {
                return None;
            }
            let len = match lead {
                0x00..=0x7F => 1,
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                _ => 4,
            };
            if p + len > bytes.len() {
                return None;
            }
            return Some(Scalar {
                decoded_bytes: len as u32,
                source_bytes: len as u32,
            });
        }
        let unit = |at: usize| -> Option<u16> {
            let pair = bytes.get(at..at + 2)?;
            Some(if inner.encoding == FileEncoding::Utf16Le {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            })
        };
        let u = unit(p)?;
        if (0xD800..0xDC00).contains(&u) {
            // High surrogate: the pair is one scalar (4 source bytes,
            // 4 decoded UTF-8 bytes).
            let _low = unit(p + 2)?;
            return Some(Scalar {
                decoded_bytes: 4,
                source_bytes: 4,
            });
        }
        let ch = char::from_u32(u32::from(u))?;
        Some(Scalar {
            decoded_bytes: ch.len_utf8() as u32,
            source_bytes: 2,
        })
    }
}

/// UTF-8 continuation byte test (`10xx_xxxx`).
pub(crate) const fn is_utf8_continuation(byte: u8) -> bool {
    byte & 0xC0 == 0x80
}
