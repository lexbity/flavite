//! Whole-buffer strict encoding under an exact descriptor.
//!
//! Output representability and the exact output byte count are validated
//! **before** allocation or any filesystem effect: strict ASCII never
//! substitutes `?` or escapes, and an output limit violation arrives with
//! zero bytes written (RFC-0006 D-15, §6.10, FR-22, NFR-19).

use crate::format::{CodecError, CodecLimitKind, EncodeLimits, FileEncoding, SourceFormat};
use crate::stream::{CHECK_INTERVAL, Encoder};

/// Byte offset of the first character that strict ASCII cannot represent, if
/// any. Whole-note representability checks use this before a save stage is
/// created.
#[must_use]
pub fn ascii_representable(text: &str) -> Option<u64> {
    text.char_indices()
        .find(|(_, c)| !c.is_ascii())
        .map(|(idx, _)| idx as u64)
}

/// Exact output size (BOM included) for `text` under `format`, with checked
/// arithmetic. Strict ASCII returns [`CodecError::Unencodable`] before any
/// output is produced.
///
/// # Errors
///
/// [`CodecError::Unencodable`] for ASCII policy with non-ASCII content.
pub fn encoded_len(text: &str, format: &SourceFormat) -> Result<u64, CodecError> {
    let bom = format.bom_len() as u64;
    match format.encoding() {
        FileEncoding::Ascii => match ascii_representable(text) {
            Some(byte) => Err(CodecError::Unencodable { text_byte: byte }),
            None => Ok(text.len() as u64),
        },
        FileEncoding::Utf8 => Ok(bom.checked_add(text.len() as u64).ok_or(CodecError::limit(
            CodecLimitKind::EncodeOutput,
            u64::MAX,
            format.bom_len() as u64 + text.len() as u64,
        ))?),
        FileEncoding::Utf16Le | FileEncoding::Utf16Be => {
            let mut units: u64 = 0;
            for c in text.chars() {
                units =
                    units
                        .checked_add(u64::from(c.len_utf16() as u16))
                        .ok_or(CodecError::limit(
                            CodecLimitKind::EncodeOutput,
                            u64::MAX,
                            bom + units.saturating_mul(2),
                        ))?;
            }
            let body = units.checked_mul(2).ok_or(CodecError::limit(
                CodecLimitKind::EncodeOutput,
                u64::MAX,
                bom + units.saturating_mul(2),
            ))?;
            body.checked_add(bom).ok_or(CodecError::limit(
                CodecLimitKind::EncodeOutput,
                u64::MAX,
                body,
            ))
        }
    }
}

/// Encode decoded UTF-8 (with its original physical EOL bytes) into the
/// exact serialized form of `format`.
///
/// The complete representability and output bound are validated **before**
/// any byte is produced; on error the caller has written nothing and can
/// present an actionable failure (`Unencodable` / `LimitExceeded`).
///
/// # Errors
///
/// [`CodecError::Unencodable`], [`CodecError::LimitExceeded`] or
/// [`CodecError::Cancelled`] — all before any partial output is returned.
pub fn encode(
    text: &str,
    format: &SourceFormat,
    limits: &EncodeLimits,
    cancel: &mut dyn FnMut() -> bool,
) -> Result<Vec<u8>, CodecError> {
    if text.len() as u64 > limits.max_input_utf8_bytes {
        return Err(CodecError::limit(
            CodecLimitKind::EncodeInput,
            text.len() as u64,
            limits.max_input_utf8_bytes,
        ));
    }
    // Preflight: exact length (also the ASCII representability gate) before
    // allocating anything.
    let total = encoded_len(text, format)?;
    if total > limits.max_output_bytes {
        return Err(CodecError::limit(
            CodecLimitKind::EncodeOutput,
            total,
            limits.max_output_bytes,
        ));
    }
    if cancel() {
        return Err(CodecError::Cancelled);
    }
    let mut out = Vec::<u8>::new();
    let mut encoder = Encoder::new(*format, *limits);
    let header = encoder.header();
    out.reserve(header.len() + text.len());
    out.extend_from_slice(header);
    // Feed in bounded pieces so cancellation is honored at progress points.
    let bytes = text.as_bytes();
    let mut offset = 0usize;
    while offset < bytes.len() {
        if offset > 0 && offset.is_multiple_of(CHECK_INTERVAL as usize) && cancel() {
            return Err(CodecError::Cancelled);
        }
        let mut end = (offset + CHECK_INTERVAL as usize).min(bytes.len());
        // Never split a UTF-8 sequence.
        while end < bytes.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        encoder.encode(&text[offset..end], &mut out)?;
        offset = end;
    }
    debug_assert_eq!(total, encoder.finish() + format.bom_len() as u64);
    Ok(out)
}
