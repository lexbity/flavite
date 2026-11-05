//! Checked encoded↔decoded byte mapping (RFC-0006 §6.10).
//!
//! The map shares the immutable original bytes; it never retains an
//! independent mutable document and never copies the file per lookup. For
//! UTF-16 sources it keeps sparse scalar-boundary checkpoints at most
//! [`CHECKPOINT_SCALARS`] apart and binary-searches them, decoding at most
//! 255 scalars locally per query (NFR-18). Mapping is total and invertible
//! on the legal boundary domain and rejects the interiors of BOMs, encoded
//! scalars/surrogate pairs, CRLF pairs and UTF-8 continuations.

use std::sync::Arc;

use crate::decode::decode_scalar_at;
use crate::format::{CodecError, CodecLimitKind, FileEncoding, SourceFormat, check_limit};

/// Maximum scalar distance between adjacent checkpoints (§6.10, NFR-18).
pub const CHECKPOINT_SCALARS: usize = 255;

/// One sparse scalar-boundary checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Checkpoint {
    source_byte: usize,
    decoded_byte: usize,
}

/// Maps byte positions of the original encoded source to byte positions of
/// the decoded UTF-8 content and back.
///
/// Valid source boundaries are scalar starts (before/after an LF
/// terminator's scalars, the beginning/end of CRLF, and the position after a
/// BOM); EOF is always a valid paired boundary on both sides. No constant
/// multiplication by two is ever used for UTF-16.
#[derive(Debug, Clone)]
pub struct DecodeMap {
    source: Arc<[u8]>,
    format: SourceFormat,
    bom_len: usize,
    decoded_len: usize,
    /// Sparse checkpoints for UTF-16 only; UTF-8/ASCII decode is a pure
    /// offset translation.
    checkpoints: Arc<[Checkpoint]>,
}

impl DecodeMap {
    pub(crate) fn build(
        source: Arc<[u8]>,
        format: SourceFormat,
        max_map_bytes: u64,
    ) -> Result<Self, CodecError> {
        let bom_len = if format.has_bom() {
            format.encoding().signature().len()
        } else {
            0
        };
        match format.encoding() {
            FileEncoding::Ascii | FileEncoding::Utf8 => {
                let decoded_len = source.len() - bom_len;
                Ok(Self {
                    source,
                    format,
                    bom_len,
                    decoded_len,
                    checkpoints: Arc::from([]),
                })
            }
            FileEncoding::Utf16Le | FileEncoding::Utf16Be => {
                let checkpoints = build_utf16_checkpoints(&source, format.encoding(), bom_len)?;
                check_limit(
                    checkpoints.len() * std::mem::size_of::<Checkpoint>(),
                    max_map_bytes,
                    CodecLimitKind::MapBytes,
                )?;
                let decoded_len = utf16_decoded_len(&source, format.encoding(), bom_len)?;
                Ok(Self {
                    source,
                    format,
                    bom_len,
                    decoded_len,
                    checkpoints: Arc::from(checkpoints),
                })
            }
        }
    }

    /// The descriptor the map was built for.
    #[must_use]
    pub const fn format(&self) -> SourceFormat {
        self.format
    }

    /// BOM length in the original source (explicit EOF/BOM metadata).
    #[must_use]
    pub const fn bom_len(&self) -> usize {
        self.bom_len
    }

    /// Length of the original encoded source bytes.
    #[must_use]
    pub fn source_len(&self) -> usize {
        self.source.len()
    }

    /// Length of the decoded UTF-8 content bytes.
    #[must_use]
    pub const fn decoded_len(&self) -> usize {
        self.decoded_len
    }

    /// Map an original source byte offset to the decoded UTF-8 byte offset.
    ///
    /// Returns `None` out of range, inside the BOM, at an odd UTF-16 byte
    /// position, inside a surrogate pair, or inside a UTF-8 continuation
    /// sequence. EOF maps to EOF.
    #[must_use]
    pub fn source_to_decoded(&self, source_byte: usize) -> Option<usize> {
        let total = self.source.len();
        if source_byte > total {
            return None;
        }
        if source_byte == total {
            return Some(self.decoded_len);
        }
        if source_byte < self.bom_len {
            return None; // BOM interior
        }
        match self.format.encoding() {
            FileEncoding::Ascii | FileEncoding::Utf8 => Some(source_byte - self.bom_len),
            FileEncoding::Utf16Le | FileEncoding::Utf16Be => {
                self.utf16_source_to_decoded(source_byte)
            }
        }
    }

    /// Map a decoded UTF-8 byte offset back to the original source byte
    /// offset. UTF-8 continuation positions are rejected; EOF maps to EOF.
    #[must_use]
    pub fn decoded_to_source(&self, decoded_byte: usize) -> Option<usize> {
        if decoded_byte > self.decoded_len {
            return None;
        }
        if decoded_byte == self.decoded_len {
            return Some(self.source.len());
        }
        match self.format.encoding() {
            FileEncoding::Ascii | FileEncoding::Utf8 => Some(decoded_byte + self.bom_len),
            FileEncoding::Utf16Le | FileEncoding::Utf16Be => {
                self.utf16_decoded_to_source(decoded_byte)
            }
        }
    }

    fn utf16_source_to_decoded(&self, source_byte: usize) -> Option<usize> {
        if !(source_byte - self.bom_len).is_multiple_of(2) {
            return None; // odd code-unit position
        }
        let idx = self
            .checkpoints
            .partition_point(|cp| cp.source_byte <= source_byte);
        let cp = *self.checkpoints.get(idx.checked_sub(1)?)?;
        if cp.source_byte == source_byte {
            return Some(cp.decoded_byte);
        }
        // Walk forward at most CHECKPOINT_SCALARS scalars from the
        // checkpoint.
        let mut cur_src = cp.source_byte;
        let mut cur_dec = cp.decoded_byte;
        let mut steps = 0;
        while cur_src < source_byte {
            if steps > CHECKPOINT_SCALARS {
                return None; // defensive bound; never reached for valid maps
            }
            let (scalar, pair_len) = decode_scalar_at(
                &self.source[self.bom_len..],
                self.format.encoding(),
                (cur_src - self.bom_len) / 2,
                self.bom_len,
            )
            .ok()?;
            let next_src = cur_src + pair_len;
            if source_byte < next_src {
                return None; // scalar/surrogate-pair interior
            }
            cur_src = next_src;
            cur_dec += scalar.len_utf8();
            if cur_src == source_byte {
                return Some(cur_dec);
            }
            steps += 1;
        }
        None
    }

    fn utf16_decoded_to_source(&self, decoded_byte: usize) -> Option<usize> {
        let idx = self
            .checkpoints
            .partition_point(|cp| cp.decoded_byte <= decoded_byte);
        let cp = *self.checkpoints.get(idx.checked_sub(1)?)?;
        if cp.decoded_byte == decoded_byte {
            return Some(cp.source_byte);
        }
        let mut cur_dec = cp.decoded_byte;
        let mut cur_src = cp.source_byte;
        let mut steps = 0;
        while cur_dec < decoded_byte {
            if steps > CHECKPOINT_SCALARS {
                return None;
            }
            let (scalar, pair_len) = decode_scalar_at(
                &self.source[self.bom_len..],
                self.format.encoding(),
                (cur_src - self.bom_len) / 2,
                self.bom_len,
            )
            .ok()?;
            let next_dec = cur_dec + scalar.len_utf8();
            if decoded_byte < next_dec {
                return None; // UTF-8 continuation interior
            }
            cur_dec = next_dec;
            cur_src += pair_len;
            if cur_dec == decoded_byte {
                return Some(cur_src);
            }
            steps += 1;
        }
        None
    }
}

fn build_utf16_checkpoints(
    source: &[u8],
    encoding: FileEncoding,
    bom_len: usize,
) -> Result<Vec<Checkpoint>, CodecError> {
    let body = &source[bom_len..];
    let unit_count = body.len() / 2;
    let mut checkpoints = Vec::new();
    let mut src = bom_len;
    let mut dec = 0usize;
    let mut since_last = 0usize;
    checkpoints.push(Checkpoint {
        source_byte: src,
        decoded_byte: dec,
    });
    let mut unit_index = 0;
    while unit_index < unit_count {
        let (scalar, pair_len) = decode_scalar_at(body, encoding, unit_index, bom_len)?;
        src += pair_len;
        dec += scalar.len_utf8();
        unit_index += pair_len / 2;
        since_last += 1;
        if since_last == CHECKPOINT_SCALARS {
            checkpoints.push(Checkpoint {
                source_byte: src,
                decoded_byte: dec,
            });
            since_last = 0;
        }
    }
    Ok(checkpoints)
}

fn utf16_decoded_len(
    source: &[u8],
    encoding: FileEncoding,
    bom_len: usize,
) -> Result<usize, CodecError> {
    let body = &source[bom_len..];
    let unit_count = body.len() / 2;
    let mut dec = 0usize;
    let mut unit_index = 0;
    while unit_index < unit_count {
        let (scalar, pair_len) = decode_scalar_at(body, encoding, unit_index, bom_len)?;
        dec += scalar.len_utf8();
        unit_index += pair_len / 2;
    }
    Ok(dec)
}

#[cfg(test)]
mod tests {

    use crate::format::{CancelToken, DecodeLimits, DecodePolicy};

    fn decode_auto(source: &[u8]) -> crate::decode::DecodedContent {
        crate::decode::decode(
            source,
            DecodePolicy::Auto,
            &DecodeLimits::default(),
            &CancelToken::new(),
        )
        .expect("decodes")
    }

    #[test]
    fn utf8_map_is_identity_past_the_bom() {
        let source = [0xEF, 0xBB, 0xBF, b'a', b'\r', b'\n', b'b'];
        let decoded = decode_auto(&source);
        let map = decoded.map();
        assert_eq!(map.bom_len(), 3);
        assert_eq!(map.source_len(), 7);
        assert_eq!(map.decoded_len(), 4);
        // BOM interior is illegal; position after the BOM is legal.
        assert_eq!(map.source_to_decoded(0), None);
        assert_eq!(map.source_to_decoded(2), None);
        assert_eq!(map.source_to_decoded(3), Some(0));
        // The CR of a CRLF pair is a legal scalar boundary.
        assert_eq!(map.source_to_decoded(4), Some(1));
        assert_eq!(map.decoded_to_source(0), Some(3));
        assert_eq!(map.decoded_to_source(1), Some(4));
        // EOF pairs on both sides.
        assert_eq!(map.source_to_decoded(7), Some(4));
        assert_eq!(map.decoded_to_source(4), Some(7));
    }

    #[test]
    fn utf16_map_rejects_odd_and_surrogate_interiors() {
        // BOM + "a" + U+1F600 (surrogate pair) + "b"
        let text = "a\u{1F600}b";
        let mut source = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            source.extend_from_slice(&unit.to_le_bytes());
        }
        let decoded = decode_auto(&source);
        let map = decoded.map();
        // source: BOM(0..2) + a(2..4) + pair(4..8) + b(8..10)
        assert_eq!(map.source_to_decoded(1), None, "odd position");
        assert_eq!(map.source_to_decoded(2), Some(0), "start of 'a'");
        assert_eq!(map.source_to_decoded(3), None, "odd position");
        assert_eq!(map.source_to_decoded(4), Some(1), "start of pair");
        assert_eq!(map.source_to_decoded(6), None, "surrogate interior");
        assert_eq!(map.source_to_decoded(8), Some(5), "start of 'b'");
        // decoded: a(0..1) + U+1F600(1..5) + b(5..6)
        assert_eq!(map.decoded_to_source(2), None, "utf-8 continuation");
        assert_eq!(map.decoded_to_source(1), Some(4));
        assert_eq!(map.decoded_to_source(5), Some(8));
        assert_eq!(map.decoded_to_source(6), Some(10), "EOF");
        assert_eq!(map.source_to_decoded(10), Some(6));
    }

    #[test]
    fn checkpoint_spacing_stays_within_255_scalars() {
        let text = "x".repeat(600);
        let mut source = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            source.extend_from_slice(&unit.to_le_bytes());
        }
        let decoded = decode_auto(&source);
        let map = decoded.map();
        // Every 100th scalar must invert exactly through the sparse table.
        for scalar in 0..600 {
            let src = 2 + 2 * scalar;
            let dec = map.source_to_decoded(src).expect("legal boundary");
            assert_eq!(map.decoded_to_source(dec), Some(src));
        }
    }
}
