//! Strict encoding of UTF-8 text into the supported source formats
//! (RFC-0006 §6.10).

use crate::format::{
    CANCEL_CHECK_BYTES, CancelToken, CodecError, CodecLimitKind, EncodeLimits, FileEncoding,
    SourceFormat, check_limit,
};

/// Encode UTF-8 text (with whatever CR/LF/EOLs it carries — the codec never
/// normalizes) into `format`.
///
/// The complete representability and the exact output size are validated
/// **before** any buffer reservation (§6.10): strict ASCII refuses the first
/// non-ASCII character without substituting `?`; a requested signature is
/// prepended only when the descriptor carries it.
///
/// # Errors
///
/// [`CodecError::Unencodable`] for non-ASCII text under strict ASCII,
/// [`CodecError::LimitExceeded`] for budget violations, and
/// [`CodecError::Cancelled`] when the token fires.
pub fn encode(
    text: &str,
    format: SourceFormat,
    limits: &EncodeLimits,
    cancel: &CancelToken,
) -> Result<Vec<u8>, CodecError> {
    if cancel.is_cancelled() {
        return Err(CodecError::Cancelled);
    }
    check_limit(
        text.len(),
        limits.max_input_utf8_bytes,
        CodecLimitKind::InputUtf8Bytes,
    )?;

    let sig_len = if format.bom() {
        format.encoding().signature().len()
    } else {
        0
    };

    // Validate representability and compute the exact output length before
    // allocating (§6.10: output size known before any filesystem effect).
    let body_len = match format.encoding() {
        FileEncoding::Ascii => match text.as_bytes().iter().position(|b| *b >= 0x80) {
            Some(idx) => return Err(CodecError::Unencodable { text_byte: idx }),
            None => text.len(),
        },
        FileEncoding::Utf8 => text.len(),
        FileEncoding::Utf16Le | FileEncoding::Utf16Be => {
            let units = text
                .chars()
                .map(char::len_utf16)
                .try_fold(0usize, usize::checked_add);
            match units {
                Some(units) => units
                    .checked_mul(2)
                    .ok_or(exhausted(limits.max_output_bytes))?,
                None => return Err(exhausted(limits.max_output_bytes)),
            }
        }
    };
    let total = body_len
        .checked_add(sig_len)
        .ok_or(exhausted(limits.max_output_bytes))?;
    check_limit(total, limits.max_output_bytes, CodecLimitKind::OutputBytes)?;

    let mut out = Vec::with_capacity(total);
    if format.bom() {
        out.extend_from_slice(format.encoding().signature());
    }
    match format.encoding() {
        // ASCII representability was validated above; both stay verbatim.
        FileEncoding::Ascii | FileEncoding::Utf8 => out.extend_from_slice(text.as_bytes()),
        FileEncoding::Utf16Le | FileEncoding::Utf16Be => {
            let le = format.encoding() == FileEncoding::Utf16Le;
            let mut consumed = 0usize;
            for ch in text.chars() {
                if consumed >= CANCEL_CHECK_BYTES {
                    if cancel.is_cancelled() {
                        return Err(CodecError::Cancelled);
                    }
                    consumed = 0;
                }
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    let bytes = if le {
                        unit.to_le_bytes()
                    } else {
                        unit.to_be_bytes()
                    };
                    out.extend_from_slice(&bytes);
                }
                consumed += ch.len_utf8();
            }
        }
    }
    debug_assert_eq!(out.len(), total);
    Ok(out)
}

fn exhausted(limit: u64) -> CodecError {
    CodecError::LimitExceeded {
        kind: CodecLimitKind::OutputBytes,
        observed: u64::MAX,
        limit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(text: &str, format: SourceFormat) -> Result<Vec<u8>, CodecError> {
        encode(text, format, &EncodeLimits::default(), &CancelToken::new())
    }

    #[test]
    fn prepends_signature_only_when_requested() {
        let with_bom = SourceFormat::new(FileEncoding::Utf8, true).unwrap();
        let without_bom = SourceFormat::new(FileEncoding::Utf8, false).unwrap();
        assert_eq!(enc("hi", with_bom).unwrap(), [0xEF, 0xBB, 0xBF, b'h', b'i']);
        assert_eq!(enc("hi", without_bom).unwrap(), b"hi");

        let le = SourceFormat::new(FileEncoding::Utf16Le, true).unwrap();
        assert_eq!(
            enc("A", le).unwrap(),
            [0xFF, 0xFE, 0x41, 0x00],
            "explicit endian serialization, never native"
        );
        let be = SourceFormat::new(FileEncoding::Utf16Be, false).unwrap();
        assert_eq!(enc("A", be).unwrap(), [0x00, 0x41]);
    }

    #[test]
    fn strict_ascii_never_substitutes() {
        let ascii = SourceFormat::new(FileEncoding::Ascii, false).unwrap();
        assert_eq!(enc("plain text", ascii).unwrap(), b"plain text");
        let err = enc("héllo", ascii).expect_err("unencodable");
        assert_eq!(err, CodecError::Unencodable { text_byte: 1 });
    }

    #[test]
    fn surrogate_pairs_encode_as_two_units() {
        let be = SourceFormat::new(FileEncoding::Utf16Be, false).unwrap();
        // U+1F600 = D83D DE00
        assert_eq!(enc("\u{1F600}", be).unwrap(), [0xD8, 0x3D, 0xDE, 0x00]);
    }

    #[test]
    fn output_limit_refuses_before_allocation() {
        let le = SourceFormat::new(FileEncoding::Utf16Le, false).unwrap();
        let limits = EncodeLimits {
            max_input_utf8_bytes: u64::MAX,
            max_output_bytes: 3,
        };
        // "abc" expands to six bytes as UTF-16 — refused before writing.
        let err = encode("abc", le, &limits, &CancelToken::new()).expect_err("over output budget");
        assert!(matches!(
            err,
            CodecError::LimitExceeded {
                kind: CodecLimitKind::OutputBytes,
                ..
            }
        ));
    }

    #[test]
    fn input_limit_refuses_oversize_text() {
        let limits = EncodeLimits {
            max_input_utf8_bytes: 2,
            max_output_bytes: u64::MAX,
        };
        let err = encode(
            "three",
            SourceFormat::new(FileEncoding::Utf8, false).unwrap(),
            &limits,
            &CancelToken::new(),
        )
        .expect_err("over input budget");
        assert_eq!(
            err,
            CodecError::LimitExceeded {
                kind: CodecLimitKind::InputUtf8Bytes,
                observed: 5,
                limit: 2,
            }
        );
    }

    #[test]
    fn cancellation_fires_before_work() {
        let token = CancelToken::new();
        token.cancel();
        assert_eq!(
            encode(
                "x",
                SourceFormat::new(FileEncoding::Utf8, false).unwrap(),
                &EncodeLimits::default(),
                &token
            )
            .expect_err("cancelled"),
            CodecError::Cancelled
        );
    }
}
