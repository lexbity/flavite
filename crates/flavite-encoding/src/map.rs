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
use crate::format::{
    CANCEL_CHECK_BYTES, CancelToken, CodecError, CodecLimitKind, DecodeLimits, FileEncoding,
    SourceFormat, check_limit,
};

/// Maximum scalar distance between adjacent checkpoints (§6.10, NFR-18).
pub const CHECKPOINT_SCALARS: usize = 255;

/// One sparse scalar-boundary checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Checkpoint {
    source_byte: usize,
    decoded_byte: usize,
}

/// Fused result of the single UTF-16 decode pass: the decoded text, the
/// sparse checkpoints collected along the way and the decoded byte length.
pub(crate) struct Utf16Scan {
    pub(crate) text: String,
    pub(crate) checkpoints: Vec<Checkpoint>,
    pub(crate) decoded_len: usize,
}

/// Decode UTF-16 `body` once, producing the text, the checkpoint table and
/// the decoded length in a single scalar walk. The map budget is refused
/// before any allocation (§6.10) and cancellation is checked at least every
/// [`CANCEL_CHECK_BYTES`] of raw progress (NFR-19).
pub(crate) fn scan_utf16(
    body: &[u8],
    encoding: FileEncoding,
    bom_len: usize,
    max_decoded: u64,
    max_map_bytes: u64,
    cancel: &crate::format::CancelToken,
) -> Result<Utf16Scan, CodecError> {
    // Structural errors are diagnosed before budget errors: the first
    // invalid sequence names its kind, not the map size (§6.10).
    if !body.len().is_multiple_of(2) {
        return Err(CodecError::OddUtf16ByteCount);
    }
    let unit_count = body.len() / 2;
    // Predicted checkpoint count for this body: refused before allocation.
    let predicted_checkpoints = unit_count / CHECKPOINT_SCALARS + 1;
    check_limit(
        predicted_checkpoints.saturating_mul(std::mem::size_of::<Checkpoint>()),
        max_map_bytes,
        CodecLimitKind::MapBytes,
    )?;
    let mut checkpoints = Vec::with_capacity(predicted_checkpoints);
    // Worst case is three UTF-8 bytes per two-byte BMP code unit.
    let mut out = String::with_capacity(unit_count.saturating_mul(3));
    let mut src = bom_len;
    let mut dec = 0usize;
    let mut since_last = 0usize;
    checkpoints.push(Checkpoint {
        source_byte: src,
        decoded_byte: dec,
    });
    let mut next_cancel = bom_len + CANCEL_CHECK_BYTES;
    let mut unit_index = 0usize;
    while unit_index < unit_count {
        let raw = bom_len + unit_index * 2;
        if raw >= next_cancel {
            if cancel.is_cancelled() {
                return Err(CodecError::Cancelled);
            }
            next_cancel = raw + CANCEL_CHECK_BYTES;
        }
        let (scalar, pair_len) = decode_scalar_at(body, encoding, unit_index, bom_len)?;
        check_limit(
            dec + scalar.len_utf8(),
            max_decoded,
            CodecLimitKind::DecodedUtf8Bytes,
        )?;
        out.push(scalar);
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
    Ok(Utf16Scan {
        text: out,
        checkpoints,
        decoded_len: dec,
    })
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
    /// Map for a UTF-8/ASCII source: decode is a pure offset translation
    /// past the BOM, so there is no checkpoint table.
    pub(crate) fn identity(source: Arc<[u8]>, format: SourceFormat) -> Self {
        let bom_len = format.bom_len();
        let decoded_len = source.len() - bom_len;
        Self {
            source,
            format,
            bom_len,
            decoded_len,
            checkpoints: Arc::from([]),
        }
    }

    /// Map for a UTF-16 source from the fused decode scan; the scan already
    /// refused the map budget before allocation.
    pub(crate) fn from_utf16_scan(
        source: Arc<[u8]>,
        format: SourceFormat,
        scan: Utf16Scan,
    ) -> Self {
        let bom_len = format.bom_len();
        Self {
            source,
            format,
            bom_len,
            decoded_len: scan.decoded_len,
            checkpoints: Arc::from(scan.checkpoints),
        }
    }

    /// Build the boundary map for `source` under an explicit `format`
    /// without producing text. Callers that already hold the decoded layer
    /// (for example a buffer re-encoding under an unchanged descriptor)
    /// compose snapshots from parts without a decode round trip (D-06).
    /// For UTF-16 this walks the same scalar scan as decode; for
    /// ASCII/UTF-8 the map is the pure identity translation and the bytes
    /// are not revalidated — the caller guarantees they encode its decoded
    /// text under `format`.
    ///
    /// # Errors
    ///
    /// Typed [`CodecError`] for unsatisfied BOM signatures, exceeded limits
    /// and cancellation; malformed UTF-16 structure is rejected by the scan.
    pub fn build(
        source: Arc<[u8]>,
        format: SourceFormat,
        limits: &DecodeLimits,
        cancel: &CancelToken,
    ) -> Result<Self, CodecError> {
        if cancel.is_cancelled() {
            return Err(CodecError::Cancelled);
        }
        check_limit(source.len(), limits.max_raw_bytes, CodecLimitKind::RawBytes)?;
        let sig_len = if format.bom() {
            let sig = format.encoding().signature();
            if !source.starts_with(sig) {
                return Err(CodecError::BomConflict);
            }
            sig.len()
        } else {
            0
        };
        match format.encoding() {
            FileEncoding::Ascii | FileEncoding::Utf8 => Ok(Self::identity(source, format)),
            FileEncoding::Utf16Le | FileEncoding::Utf16Be => {
                let scan = scan_utf16(
                    &source[sig_len..],
                    format.encoding(),
                    sig_len,
                    limits.max_decoded_utf8_bytes,
                    limits.max_map_bytes,
                    cancel,
                )?;
                Ok(Self::from_utf16_scan(source, format, scan))
            }
        }
    }

    /// The descriptor the map was built for.
    #[must_use]
    pub const fn format(&self) -> SourceFormat {
        self.format
    }

    /// The immutable original encoded bytes the map was built from.
    pub(crate) const fn source_bytes(&self) -> &std::sync::Arc<[u8]> {
        &self.source
    }

    /// BOM length in the original source (explicit EOF/BOM metadata).
    #[must_use]
    pub const fn bom_len(&self) -> usize {
        self.bom_len
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
            FileEncoding::Ascii | FileEncoding::Utf8 => {
                // §6.10: a UTF-8 continuation byte is never a legal
                // boundary, even though the offset arithmetic is identity.
                if self
                    .source
                    .get(source_byte)
                    .is_some_and(|byte| byte & 0xC0 == 0x80)
                {
                    None
                } else {
                    Some(source_byte - self.bom_len)
                }
            }
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
            FileEncoding::Ascii | FileEncoding::Utf8 => {
                let at = decoded_byte + self.bom_len;
                if self.source.get(at).is_some_and(|byte| byte & 0xC0 == 0x80) {
                    None
                } else {
                    Some(at)
                }
            }
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

#[cfg(test)]
mod tests {

    use crate::format::{CancelToken, DecodeLimits, DecodePolicy, SourceFormat};

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
    fn utf8_map_rejects_continuation_interiors() {
        // "é" = C3 A9, then "b".
        let decoded = decode_auto("\u{e9}b".as_bytes());
        let map = decoded.map();
        // Scalar starts are legal; continuation positions are not (§6.10).
        assert_eq!(map.source_to_decoded(0), Some(0));
        assert_eq!(map.source_to_decoded(2), Some(2));
        assert_eq!(map.source_to_decoded(1), None);
        assert_eq!(map.decoded_to_source(0), Some(0));
        assert_eq!(map.decoded_to_source(2), Some(2));
        assert_eq!(map.decoded_to_source(1), None);
        // Inversion holds on the whole legal domain.
        for legal in [0usize, 2, 3] {
            let s = map.source_to_decoded(legal).expect("legal boundary");
            assert_eq!(map.decoded_to_source(s), Some(legal));
        }
        // A 4-byte scalar rejects every interior on both sides.
        let wide = decode_auto("x\u{1F600}y".as_bytes());
        let map = wide.map();
        assert_eq!(map.source_to_decoded(1), Some(1));
        for interior in [2usize, 3, 4] {
            assert_eq!(map.source_to_decoded(interior), None);
            assert_eq!(map.decoded_to_source(interior), None);
        }
        assert_eq!(map.source_to_decoded(5), Some(5));
    }

    #[test]
    fn build_matches_decode_map_for_the_same_bytes() {
        use crate::decode::decode;
        for (format, source) in [
            (SourceFormat::utf8(), "héllo\r\nwörld\n".as_bytes().to_vec()),
            (SourceFormat::utf16le_with_bom(), {
                let mut bytes = Vec::from([0xFF, 0xFE]);
                for unit in "A😀\r\nsecon\r\n".encode_utf16() {
                    bytes.extend_from_slice(&unit.to_le_bytes());
                }
                bytes
            }),
        ] {
            let cancel = CancelToken::new();
            let built = crate::map::DecodeMap::build(
                std::sync::Arc::from(source.clone()),
                format,
                &DecodeLimits::default(),
                &cancel,
            )
            .expect("builds");
            let decoded = decode(
                &source,
                DecodePolicy::Explicit(format),
                &DecodeLimits::default(),
                &CancelToken::new(),
            )
            .expect("decodes");
            // Probe a spread of legal and illegal positions on both maps.
            let total = source.len();
            for at in [0usize, 1, 2, total / 2, total.saturating_sub(1), total] {
                assert_eq!(
                    built.source_to_decoded(at),
                    decoded.map().source_to_decoded(at),
                    "source_to_decoded({at}) under {format:?}"
                );
            }
            let decoded_len = built.source_to_decoded(total).expect("eof");
            for at in [0usize, 1, decoded_len / 2, decoded_len] {
                assert_eq!(
                    built.decoded_to_source(at),
                    decoded.map().decoded_to_source(at),
                    "decoded_to_source({at}) under {format:?}"
                );
            }
        }
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
