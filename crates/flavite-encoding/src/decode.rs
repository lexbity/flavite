//! Strict decoding of encoded note sources to UTF-8 content with a checked
//! encoded↔decoded byte map (RFC-0006 §6.10).

use std::sync::Arc;

use crate::format::{
    CANCEL_CHECK_BYTES, CancelToken, CodecError, CodecLimitKind, DecodeLimits, DecodePolicy,
    FileEncoding, SourceFormat, check_limit,
};
use crate::map::{DecodeMap, scan_utf16};

/// Successfully decoded content: UTF-8 text, the resolved
/// [`SourceFormat`] and a checked [`DecodeMap`] sharing the original bytes.
///
/// Decode removes only the signature selected by the policy, preserves every
/// content scalar (including an additional U+FEFF) and performs no
/// EOL/NFC normalization — a standalone CR and content U+FEFF survive.
#[derive(Debug, Clone)]
pub struct DecodedContent {
    text: String,
    format: SourceFormat,
    map: DecodeMap,
}

impl DecodedContent {
    /// The decoded UTF-8 text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The resolved descriptor the bytes were decoded under.
    #[must_use]
    pub const fn format(&self) -> SourceFormat {
        self.format
    }

    /// The checked encoded↔decoded byte map sharing the original bytes.
    #[must_use]
    pub const fn map(&self) -> &DecodeMap {
        &self.map
    }

    /// The shared original encoded bytes (including any BOM) backing the
    /// map — used to build disk-backed snapshots without a second copy.
    #[must_use]
    pub fn source(&self) -> &std::sync::Arc<[u8]> {
        self.map.source_bytes()
    }
}

/// Decode encoded source bytes under `policy` into UTF-8 content.
///
/// The map shares one immutable copy of the source bytes; callers holding
/// borrowed slices accept that single copy at this boundary.
///
/// # Errors
///
/// Returns the typed [`CodecError`] for malformed input, unsatisfied BOM
/// policies, exceeded limits and cancellation. Malformed input is never
/// lossy-repaired and never produces U+FFFD.
pub fn decode(
    source: &[u8],
    policy: DecodePolicy,
    limits: &DecodeLimits,
    cancel: &CancelToken,
) -> Result<DecodedContent, CodecError> {
    decode_inner(Arc::from(source), policy, limits, cancel)
}

fn decode_inner(
    source: Arc<[u8]>,
    policy: DecodePolicy,
    limits: &DecodeLimits,
    cancel: &CancelToken,
) -> Result<DecodedContent, CodecError> {
    if cancel.is_cancelled() {
        return Err(CodecError::Cancelled);
    }
    check_limit(source.len(), limits.max_raw_bytes, CodecLimitKind::RawBytes)?;

    let (format, sig_len) = resolve_format(&source, policy)?;
    let body = &source[sig_len..];
    let (text, map) = match format.encoding() {
        FileEncoding::Ascii => {
            let text = decode_ascii(body, sig_len, limits.max_decoded_utf8_bytes, cancel)?;
            (text, DecodeMap::identity(source, format))
        }
        FileEncoding::Utf8 => {
            let text = decode_utf8(body, sig_len, limits.max_decoded_utf8_bytes, cancel)?;
            (text, DecodeMap::identity(source, format))
        }
        FileEncoding::Utf16Le | FileEncoding::Utf16Be => {
            let mut scan = scan_utf16(
                body,
                format.encoding(),
                sig_len,
                limits.max_decoded_utf8_bytes,
                limits.max_map_bytes,
                cancel,
            )?;
            let text = std::mem::take(&mut scan.text);
            (text, DecodeMap::from_utf16_scan(source, format, scan))
        }
    };

    Ok(DecodedContent { text, format, map })
}

/// Resolve the effective descriptor and the signature length to strip.
fn resolve_format(
    source: &[u8],
    policy: DecodePolicy,
) -> Result<(SourceFormat, usize), CodecError> {
    match policy {
        DecodePolicy::Auto => auto_signature(source),
        DecodePolicy::Explicit(format) => {
            let sig = format.encoding().signature();
            if format.bom() {
                if source.starts_with(sig) {
                    Ok((format, sig.len()))
                } else {
                    Err(CodecError::BomConflict)
                }
            } else {
                // Explicit BOM-less does not strip signatures: a leading
                // U+FEFF-shaped byte sequence is content (§6.10).
                Ok((format, 0))
            }
        }
    }
}

/// Longer unsupported signatures, checked before the UTF-16 prefixes so the
/// UTF-32 signatures are diagnosed as ambiguous rather than decoded as
/// UTF-16LE/BE (§6.10).
pub(crate) const LONGER_UNSUPPORTED_SIGNATURES: [&[u8]; 6] = [
    &[0xFF, 0xFE, 0x00, 0x00], // UTF-32LE
    &[0x00, 0x00, 0xFE, 0xFF], // UTF-32BE
    &[0xF7, 0x64, 0x4C],       // UTF-1
    &[0xDD, 0x76, 0x67],       // UTF-EBCDIC
    &[0x0E, 0xFE, 0xFF],       // SCSU
    &[0x2B, 0x2F, 0x76],       // UTF-7
];

/// Auto signature recognition. BOM-less input is UTF-8, including all-ASCII
/// bytes — ASCII is never auto-selected and UTF-16 is never guessed.
fn auto_signature(source: &[u8]) -> Result<(SourceFormat, usize), CodecError> {
    for sig in &LONGER_UNSUPPORTED_SIGNATURES {
        if source.starts_with(sig) {
            return Err(CodecError::UnsupportedOrAmbiguousBom);
        }
    }
    if source.starts_with(FileEncoding::Utf8.signature()) {
        return Ok((SourceFormat::new_unchecked(FileEncoding::Utf8, true), 3));
    }
    if source.starts_with(FileEncoding::Utf16Le.signature()) {
        return Ok((SourceFormat::new_unchecked(FileEncoding::Utf16Le, true), 2));
    }
    if source.starts_with(FileEncoding::Utf16Be.signature()) {
        return Ok((SourceFormat::new_unchecked(FileEncoding::Utf16Be, true), 2));
    }
    Ok((SourceFormat::new_unchecked(FileEncoding::Utf8, false), 0))
}

/// Strict UTF-8 decode in [`CANCEL_CHECK_BYTES`] chunks: cancellation is
/// checked at least every 64 KiB of raw progress (NFR-19) while each chunk
/// still validates through the standard library's `from_utf8`.
fn decode_utf8(
    body: &[u8],
    sig_len: usize,
    max_decoded: u64,
    cancel: &CancelToken,
) -> Result<String, CodecError> {
    check_limit(body.len(), max_decoded, CodecLimitKind::DecodedUtf8Bytes)?;
    let mut out = String::with_capacity(body.len());
    let mut start = 0usize;
    while start < body.len() {
        let end = body.len().min(start + CANCEL_CHECK_BYTES);
        if cancel.is_cancelled() {
            return Err(CodecError::Cancelled);
        }
        match std::str::from_utf8(&body[start..end]) {
            Ok(text) => {
                check_limit(
                    out.len() + text.len(),
                    max_decoded,
                    CodecLimitKind::DecodedUtf8Bytes,
                )?;
                out.push_str(text);
                start = end;
            }
            Err(e) if e.error_len().is_none() && end < body.len() => {
                // The chunk boundary split a multibyte sequence: the valid
                // prefix is real and the (≤3 byte) incomplete tail leads the
                // next chunk. A non-final chunk is CANCEL_CHECK_BYTES long,
                // so the tail can never swallow the whole chunk here.
                let valid = e.valid_up_to();
                check_limit(
                    out.len() + valid,
                    max_decoded,
                    CodecLimitKind::DecodedUtf8Bytes,
                )?;
                let prefix = std::str::from_utf8(&body[start..start + valid])
                    .expect("valid_up_to prefix is valid UTF-8");
                out.push_str(prefix);
                start += valid;
            }
            Err(e) => {
                return Err(CodecError::InvalidUtf8 {
                    source_byte: start + e.valid_up_to() + sig_len,
                });
            }
        }
    }
    Ok(out)
}

/// Strict ASCII decode in [`CANCEL_CHECK_BYTES`] chunks (NFR-19).
fn decode_ascii(
    body: &[u8],
    sig_len: usize,
    max_decoded: u64,
    cancel: &CancelToken,
) -> Result<String, CodecError> {
    check_limit(body.len(), max_decoded, CodecLimitKind::DecodedUtf8Bytes)?;
    let mut out = String::with_capacity(body.len());
    let mut start = 0usize;
    while start < body.len() {
        let end = body.len().min(start + CANCEL_CHECK_BYTES);
        if cancel.is_cancelled() {
            return Err(CodecError::Cancelled);
        }
        if let Some(idx) = body[start..end].iter().position(|b| *b >= 0x80) {
            return Err(CodecError::InvalidUtf8 {
                source_byte: start + idx + sig_len,
            });
        }
        check_limit(
            out.len() + (end - start),
            max_decoded,
            CodecLimitKind::DecodedUtf8Bytes,
        )?;
        // All bytes are ASCII, therefore valid UTF-8.
        out.push_str(std::str::from_utf8(&body[start..end]).unwrap_or_default());
        start = end;
    }
    Ok(out)
}

/// Decode the scalar whose (possibly paired) code units start at
/// `unit_index`, returning the scalar and its source length in bytes.
/// `sig_len` is the BOM length already stripped before `body`, used for
/// error positions in original source coordinates.
pub(crate) fn decode_scalar_at(
    body: &[u8],
    encoding: FileEncoding,
    unit_index: usize,
    sig_len: usize,
) -> Result<(char, usize), CodecError> {
    let read = |i: usize| -> u16 {
        let bytes = [body[2 * i], body[2 * i + 1]];
        match encoding {
            FileEncoding::Utf16Le => u16::from_le_bytes(bytes),
            _ => u16::from_be_bytes(bytes),
        }
    };
    let unit = read(unit_index);
    let err_at = |source_byte: usize| CodecError::InvalidUtf16 {
        source_byte: sig_len + source_byte,
    };
    if (0xD800..=0xDBFF).contains(&unit) {
        // High surrogate: only a following low surrogate completes the pair.
        let next_index = unit_index + 1;
        if next_index >= body.len() / 2 {
            return Err(err_at(2 * unit_index));
        }
        let next = read(next_index);
        if !((0xDC00..=0xDFFF).contains(&next)) {
            return Err(err_at(2 * unit_index));
        }
        let combined = 0x1_0000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(next) - 0xDC00);
        match char::from_u32(combined) {
            Some(scalar) => Ok((scalar, 4)),
            None => Err(err_at(2 * unit_index)),
        }
    } else if (0xDC00..=0xDFFF).contains(&unit) {
        // Lone low surrogate.
        Err(err_at(2 * unit_index))
    } else {
        match char::from_u32(u32::from(unit)) {
            Some(scalar) => Ok((scalar, 2)),
            None => Err(err_at(2 * unit_index)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_auto(source: &[u8]) -> Result<DecodedContent, CodecError> {
        decode(
            source,
            DecodePolicy::Auto,
            &DecodeLimits::default(),
            &CancelToken::new(),
        )
    }

    #[test]
    fn auto_recognizes_supported_signatures_only() {
        let cases: [(&[u8], FileEncoding, bool); 4] = [
            (&[0xEF, 0xBB, 0xBF, b'a'], FileEncoding::Utf8, true),
            (&[0xFF, 0xFE, 0x41, 0x00], FileEncoding::Utf16Le, true),
            (&[0xFE, 0xFF, 0x00, 0x41], FileEncoding::Utf16Be, true),
            (b"plain", FileEncoding::Utf8, false),
        ];
        for (source, encoding, bom) in cases {
            let decoded = decode_auto(source).expect("decodes");
            assert_eq!(decoded.format(), SourceFormat::new(encoding, bom).unwrap());
        }
    }

    #[test]
    fn auto_diagnoses_longer_signatures_before_utf16_prefixes() {
        for sig in &LONGER_UNSUPPORTED_SIGNATURES {
            let mut source = sig.to_vec();
            source.extend_from_slice(b"more");
            assert_eq!(
                decode_auto(&source).expect_err("diagnosed"),
                CodecError::UnsupportedOrAmbiguousBom,
                "signature {sig:?}"
            );
        }
        // BOM-less bytes that look like UTF-16 are refused as invalid UTF-8,
        // never guessed: 0xD8 is an invalid UTF-8 lead byte.
        let err = decode_auto(&[0xD8, 0x00]).expect_err("not guessed as UTF-16");
        assert!(matches!(err, CodecError::InvalidUtf8 { source_byte: 0 }));
    }

    #[test]
    fn utf16_error_positions_include_the_signature() {
        // BOM (2) + lone high surrogate D800 followed by 'A'.
        let source = [0xFF, 0xFE, 0x00, 0xD8, 0x41, 0x00];
        let err = decode_auto(&source).expect_err("unpaired surrogate");
        assert_eq!(err, CodecError::InvalidUtf16 { source_byte: 2 });
        // Odd trailing byte after the signature.
        let source = [0xFF, 0xFE, 0x41, 0x00, 0x42];
        assert_eq!(
            decode_auto(&source).expect_err("odd byte count"),
            CodecError::OddUtf16ByteCount
        );
    }

    #[test]
    fn explicit_bom_policies() {
        let explicit_be = SourceFormat::new(FileEncoding::Utf16Be, true).unwrap();
        let err = decode(
            &[0xFF, 0xFE, 0x41, 0x00],
            DecodePolicy::Explicit(explicit_be),
            &DecodeLimits::default(),
            &CancelToken::new(),
        )
        .expect_err("wrong signature");
        assert_eq!(err, CodecError::BomConflict);

        // Explicit BOM-less treats a leading signature-shaped sequence as
        // content: FF FE decodes to U+FEFF, then U+0000.
        let le_format = SourceFormat::new(FileEncoding::Utf16Le, false).unwrap();
        let decoded = decode(
            &[0xFF, 0xFE, 0x00, 0x00],
            DecodePolicy::Explicit(le_format),
            &DecodeLimits::default(),
            &CancelToken::new(),
        )
        .expect("decodes as content");
        assert_eq!(decoded.text(), "\u{FEFF}\u{0}");
        assert_eq!(decoded.format(), le_format);
    }

    #[test]
    fn strict_ascii_decode_rejects_high_bytes() {
        let ascii = SourceFormat::new(FileEncoding::Ascii, false).unwrap();
        let err = decode(
            b"ok\xffbad",
            DecodePolicy::Explicit(ascii),
            &DecodeLimits::default(),
            &CancelToken::new(),
        )
        .expect_err("not ASCII");
        assert_eq!(err, CodecError::InvalidUtf8 { source_byte: 2 });
    }

    #[test]
    fn cancellation_and_limits_fire_before_effects() {
        let token = CancelToken::new();
        token.cancel();
        assert_eq!(
            decode(b"x", DecodePolicy::Auto, &DecodeLimits::default(), &token)
                .expect_err("cancelled"),
            CodecError::Cancelled
        );

        let limits = DecodeLimits {
            max_raw_bytes: 2,
            ..DecodeLimits::default()
        };
        let err = decode(
            b"longer than two",
            DecodePolicy::Auto,
            &limits,
            &CancelToken::new(),
        )
        .expect_err("raw limit");
        assert_eq!(
            err,
            CodecError::LimitExceeded {
                kind: CodecLimitKind::RawBytes,
                observed: 15,
                limit: 2,
            }
        );
    }

    #[test]
    fn preserves_content_scalars_without_normalization() {
        // Standalone CR and a second U+FEFF survive; no EOL folding.
        let source = "a\rb\n\u{FEFF}c".as_bytes();
        let decoded = decode_auto(source).expect("decodes");
        assert_eq!(decoded.text(), "a\rb\n\u{FEFF}c");
    }
}
