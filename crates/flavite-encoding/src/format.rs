//! Source format descriptors, decode/encode policies, resource limits and
//! typed codec errors (RFC-0006 §6.10, D-15).

use std::sync::atomic::{AtomicBool, Ordering};

/// Resource-envelope defaults (NFR-18/NFR-19): raw source, decoded UTF-8 and
/// serialized output each have independent 32 MiB limits; the decode-map
/// overhead budget is 4 MiB per 32 MiB input/output-bound pair.
pub const MAX_RAW_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_DECODED_UTF8_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_ENCODE_INPUT_UTF8_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_ENCODE_OUTPUT_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_MAP_BYTES: u64 = 4 * 1024 * 1024;

/// Progress/cancellation checkpoint granularity for CPU work (NFR-19).
pub const CANCEL_CHECK_BYTES: usize = 64 * 1024;

/// File encodings supported by this RFC (§6.10). UTF-32, Windows-1252,
/// Latin-1 and arbitrary codepages are outside the set. Endianness is
/// explicit, never native-machine-dependent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "snake_case")
)]
pub enum FileEncoding {
    /// 7-bit ASCII. Strict output policy: non-ASCII characters refuse to
    /// encode. Its bytes cannot be distinguished from UTF-8, so Auto never
    /// selects this encoding and it never carries a BOM.
    Ascii,
    /// UTF-8.
    Utf8,
    /// UTF-16, little-endian.
    Utf16Le,
    /// UTF-16, big-endian.
    Utf16Be,
}

impl FileEncoding {
    /// The byte-order-mark signature this encoding uses, or an empty slice
    /// when the encoding never carries one (ASCII+BOM is invalid, §6.10).
    #[must_use]
    pub const fn signature(self) -> &'static [u8] {
        match self {
            Self::Ascii => &[],
            Self::Utf8 => &[0xEF, 0xBB, 0xBF],
            Self::Utf16Le => &[0xFF, 0xFE],
            Self::Utf16Be => &[0xFE, 0xFF],
        }
    }

    /// Stable machine-readable code used in persisted records and wire DTOs
    /// (`"ascii"`, `"utf8"`, `"utf16le"`, `"utf16be"`; RFC-0006 §6.1/§6.10).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Ascii => "ascii",
            Self::Utf8 => "utf8",
            Self::Utf16Le => "utf16le",
            Self::Utf16Be => "utf16be",
        }
    }

    /// Inverse of [`FileEncoding::code`].
    #[must_use]
    pub const fn from_code(code: &str) -> Option<Self> {
        match code.as_bytes() {
            b"ascii" => Some(Self::Ascii),
            b"utf8" => Some(Self::Utf8),
            b"utf16le" => Some(Self::Utf16Le),
            b"utf16be" => Some(Self::Utf16Be),
            _ => None,
        }
    }

    /// Whether code units are two bytes (UTF-16), which makes an odd
    /// trailing byte an error and forbids a constant multiply-by-two
    /// offset mapping.
    #[must_use]
    pub const fn is_utf16(self) -> bool {
        matches!(self, Self::Utf16Le | Self::Utf16Be)
    }
}

/// The sole source-format descriptor, shared/re-exported across core and
/// facade types (§6.10). Fields are private; every combination is validated
/// at construction (also at deserialization through the raw pair).
/// ASCII+BOM is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "RawSourceFormat")
)]
pub struct SourceFormat {
    encoding: FileEncoding,
    bom: bool,
}

impl SourceFormat {
    /// Construct a validated descriptor.
    ///
    /// # Errors
    ///
    /// [`CodecError::BomConflict`] when a BOM is requested for
    /// [`FileEncoding::Ascii`] — ASCII never carries a BOM.
    pub fn new(encoding: FileEncoding, bom: bool) -> Result<Self, CodecError> {
        if bom && encoding == FileEncoding::Ascii {
            return Err(CodecError::BomConflict);
        }
        Ok(Self { encoding, bom })
    }

    /// Internal constructor for combinations already proven valid by the
    /// caller's signature-detection logic.
    pub(crate) const fn new_unchecked(encoding: FileEncoding, bom: bool) -> Self {
        Self { encoding, bom }
    }

    /// The encoding half of the descriptor.
    #[must_use]
    pub const fn encoding(&self) -> FileEncoding {
        self.encoding
    }

    /// Whether the descriptor carries the encoding's BOM signature.
    #[must_use]
    pub const fn bom(&self) -> bool {
        self.bom
    }

    /// UTF-8 without a BOM. The default for newly created notes.
    #[must_use]
    pub const fn utf8() -> Self {
        Self::new_unchecked(FileEncoding::Utf8, false)
    }

    /// UTF-8 with a BOM.
    #[must_use]
    pub const fn utf8_with_bom() -> Self {
        Self::new_unchecked(FileEncoding::Utf8, true)
    }

    /// Strict ASCII (never has a BOM).
    #[must_use]
    pub const fn ascii() -> Self {
        Self::new_unchecked(FileEncoding::Ascii, false)
    }

    /// UTF-16LE without a BOM.
    #[must_use]
    pub const fn utf16le() -> Self {
        Self::new_unchecked(FileEncoding::Utf16Le, false)
    }

    /// UTF-16LE with a BOM.
    #[must_use]
    pub const fn utf16le_with_bom() -> Self {
        Self::new_unchecked(FileEncoding::Utf16Le, true)
    }

    /// UTF-16BE without a BOM.
    #[must_use]
    pub const fn utf16be() -> Self {
        Self::new_unchecked(FileEncoding::Utf16Be, false)
    }

    /// UTF-16BE with a BOM.
    #[must_use]
    pub const fn utf16be_with_bom() -> Self {
        Self::new_unchecked(FileEncoding::Utf16Be, true)
    }

    /// Number of BOM signature bytes physically present in a source that
    /// uses this descriptor (0 or the signature length).
    #[must_use]
    pub const fn bom_len(&self) -> usize {
        if self.bom {
            // ASCII+bom is unconstructable, so this is always the real length.
            self.encoding.signature().len()
        } else {
            0
        }
    }

    /// The BOM bytes to prepend when serializing under this descriptor.
    #[must_use]
    pub fn bom_bytes(&self) -> &'static [u8] {
        if self.bom {
            self.encoding.signature()
        } else {
            &[]
        }
    }

    /// Stable machine-readable code for persisted records, e.g. `"utf16le"`.
    #[must_use]
    pub const fn encoding_code(&self) -> &'static str {
        self.encoding.code()
    }
}

impl Default for SourceFormat {
    /// Newly created notes are UTF-8 without a BOM (§6.10 default).
    fn default() -> Self {
        Self::utf8()
    }
}

impl std::fmt::Display for SourceFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.bom {
            write!(f, "{}+bom", self.encoding.code())
        } else {
            f.write_str(self.encoding.code())
        }
    }
}

#[cfg(feature = "serde")]
#[derive(serde::Deserialize)]
struct RawSourceFormat {
    encoding: FileEncoding,
    bom: bool,
}

#[cfg(feature = "serde")]
impl TryFrom<RawSourceFormat> for SourceFormat {
    type Error = CodecError;

    fn try_from(raw: RawSourceFormat) -> Result<Self, Self::Error> {
        Self::new(raw.encoding, raw.bom)
    }
}

/// Version of the codec implementation recorded in every
/// [`FormatStamp`] and in the index cache marker (§6.10). Bump when a codec
/// change invalidates persisted stamps or derived caches.
pub const CODEC_VERSION: u32 = 1;

/// Versioned, cache-stampable provenance of a source format (§6.1, §6.10).
///
/// A format stamp is what invalidates raw-hash/source-span observations even
/// when the normalized text is byte-identical: two files with the same
/// decoded text but different descriptors produce different stamps.
/// Persisted records serialize the descriptor fields separately, so this
/// type carries no serde representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FormatStamp {
    /// The authoritative descriptor.
    pub format: SourceFormat,
    /// Codec implementation version in effect when this stamp was created.
    pub codec_version: u32,
    /// Generation of the applicable device-local encoding-hint record, if
    /// this format came from a user-confirmed hint rather than detection.
    pub hint_generation: Option<u64>,
}

impl FormatStamp {
    /// Stamp a freshly detected/confirmed format at the current codec version.
    #[must_use]
    pub const fn detected(format: SourceFormat) -> Self {
        Self {
            format,
            codec_version: CODEC_VERSION,
            hint_generation: None,
        }
    }

    /// Stamp a format that came from a confirmed hint of the given generation.
    #[must_use]
    pub const fn from_hint(format: SourceFormat, hint_generation: u64) -> Self {
        Self {
            format,
            codec_version: CODEC_VERSION,
            hint_generation: Some(hint_generation),
        }
    }
}

/// Decode policy (§6.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DecodePolicy {
    /// Recognize a supported signature only; everything else is decoded as
    /// strict UTF-8. Auto never infers UTF-16 endianness from NUL patterns,
    /// never chooses a fallback decoder and never marks BOM-less ASCII-only
    /// bytes as "strict ASCII".
    #[default]
    Auto,
    /// A confirmed descriptor. Explicit(format with BOM) requires and
    /// removes exactly that signature; Explicit(format without BOM) does
    /// **not** strip signatures and treats a leading U+FEFF-shaped byte
    /// sequence as content.
    Explicit(SourceFormat),
}

/// Which independent budget a [`CodecError::LimitExceeded`] hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodecLimitKind {
    /// Encoded source bytes offered to [`decode`](crate::decode).
    RawBytes,
    /// Decoded UTF-8 content bytes.
    DecodedUtf8Bytes,
    /// Decode-map checkpoint overhead.
    MapBytes,
    /// UTF-8 text offered to [`encode`](crate::encode).
    InputUtf8Bytes,
    /// Serialized output bytes.
    OutputBytes,
}

/// Typed codec errors (§6.10). Diagnostics never contain note content —
/// only kind, byte offset and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// Strict decode failed; `source_byte` is the offset of the first
    /// invalid sequence in the original encoded source, including any BOM.
    /// Also used for strict-ASCII bytes that are not ASCII.
    InvalidUtf8 {
        /// Offset of the first invalid byte in the original source.
        source_byte: usize,
    },
    /// Unpaired or misordered UTF-16 surrogate.
    InvalidUtf16 {
        /// Offset of the offending surrogate unit in the original source.
        source_byte: usize,
    },
    /// UTF-16 input with a trailing odd byte.
    OddUtf16ByteCount,
    /// Explicit BOM policy not satisfied by the source signature.
    BomConflict,
    /// A longer unsupported signature (e.g. UTF-32) was recognized; Auto
    /// refuses to guess.
    UnsupportedOrAmbiguousBom,
    /// Strict ASCII output refused; `text_byte` is the offset of the first
    /// non-ASCII character in the decoded UTF-8 text.
    Unencodable {
        /// Offset of the first unencodable character in the UTF-8 text.
        text_byte: usize,
    },
    /// An independent budget was exceeded before allocation.
    LimitExceeded {
        /// Which budget was hit.
        kind: CodecLimitKind,
        /// Observed size (or `u64::MAX` on checked-arithmetic overflow).
        observed: u64,
        /// The configured limit.
        limit: u64,
    },
    /// The caller's cancellation token fired.
    Cancelled,
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::InvalidUtf8 { source_byte } => {
                write!(
                    f,
                    "invalid sequence for the requested encoding at source byte {source_byte}"
                )
            }
            Self::InvalidUtf16 { source_byte } => {
                write!(
                    f,
                    "invalid UTF-16 surrogate sequence at source byte {source_byte}"
                )
            }
            Self::OddUtf16ByteCount => {
                write!(
                    f,
                    "UTF-16 input has an odd byte count; the trailing byte is undecodable"
                )
            }
            Self::BomConflict => write!(
                f,
                "byte-order-mark conflict: source signature does not match the requested format"
            ),
            Self::UnsupportedOrAmbiguousBom => {
                write!(f, "unsupported or ambiguous byte-order-mark signature")
            }
            Self::Unencodable { text_byte } => write!(
                f,
                "text is not encodable in the requested strict format at text byte {text_byte}"
            ),
            Self::LimitExceeded {
                kind,
                observed,
                limit,
            } => write!(
                f,
                "codec limit exceeded for {kind:?}: observed {observed}, limit {limit}"
            ),
            Self::Cancelled => write!(f, "codec operation cancelled"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Decode-side resource limits (§6.10). Raw/decoded/map budgets are
/// independent; defaults reflect NFR-18/NFR-19 envelopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    /// Maximum encoded source bytes accepted for one decode call.
    pub max_raw_bytes: u64,
    /// Maximum decoded UTF-8 content bytes.
    pub max_decoded_utf8_bytes: u64,
    /// Maximum decode-map checkpoint overhead.
    pub max_map_bytes: u64,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_raw_bytes: MAX_RAW_BYTES,
            max_decoded_utf8_bytes: MAX_DECODED_UTF8_BYTES,
            max_map_bytes: MAX_MAP_BYTES,
        }
    }
}

/// Encode-side resource limits (§6.10). Input and serialized output are
/// independent budgets (UTF-16 expansion of ASCII can double the size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeLimits {
    /// Maximum UTF-8 input bytes accepted for one encode call.
    pub max_input_utf8_bytes: u64,
    /// Maximum serialized output bytes, including a requested signature.
    pub max_output_bytes: u64,
}

impl Default for EncodeLimits {
    fn default() -> Self {
        Self {
            max_input_utf8_bytes: MAX_ENCODE_INPUT_UTF8_BYTES,
            max_output_bytes: MAX_ENCODE_OUTPUT_BYTES,
        }
    }
}

/// Cooperative cancellation for codec work (NFR-19: checked at least every
/// [`CANCEL_CHECK_BYTES`] of raw/decoded progress).
#[derive(Debug, Default)]
pub struct CancelToken(AtomicBool);

impl CancelToken {
    /// A not-cancelled token.
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// Request cancellation of in-flight codec work.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// A never-cancelled token shared by callers with no cancellation intent.
pub static NOT_CANCELLED: CancelToken = CancelToken::new();

/// Convert an in-memory length to a `u64` observation without wrapping.
pub(crate) fn observed(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// Enforce one budget; refuses before allocation.
pub(crate) fn check_limit(
    observed_len: usize,
    limit: u64,
    kind: CodecLimitKind,
) -> Result<(), CodecError> {
    let seen = observed(observed_len);
    if seen > limit {
        return Err(CodecError::LimitExceeded {
            kind,
            observed: seen,
            limit,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_match_the_supported_set() {
        assert_eq!(FileEncoding::Ascii.signature(), b"");
        assert_eq!(FileEncoding::Utf8.signature(), &[0xEF, 0xBB, 0xBF]);
        assert_eq!(FileEncoding::Utf16Le.signature(), &[0xFF, 0xFE]);
        assert_eq!(FileEncoding::Utf16Be.signature(), &[0xFE, 0xFF]);
    }

    #[test]
    fn ascii_with_bom_is_invalid() {
        let err = SourceFormat::new(FileEncoding::Ascii, true).expect_err("ascii+BOM is invalid");
        assert_eq!(err, CodecError::BomConflict);
        // Every other combination validates, including BOM-less UTF-16.
        for encoding in [
            FileEncoding::Ascii,
            FileEncoding::Utf8,
            FileEncoding::Utf16Le,
            FileEncoding::Utf16Be,
        ] {
            assert!(SourceFormat::new(encoding, false).is_ok());
        }
        for encoding in [
            FileEncoding::Utf8,
            FileEncoding::Utf16Le,
            FileEncoding::Utf16Be,
        ] {
            assert!(SourceFormat::new(encoding, true).is_ok());
        }
    }

    #[test]
    fn limits_default_to_the_nfr_envelope() {
        let limits = DecodeLimits::default();
        assert_eq!(limits.max_raw_bytes, MAX_RAW_BYTES);
        assert_eq!(limits.max_decoded_utf8_bytes, MAX_DECODED_UTF8_BYTES);
        assert_eq!(limits.max_map_bytes, MAX_MAP_BYTES);
        let encode_limits = EncodeLimits::default();
        assert_eq!(
            encode_limits.max_input_utf8_bytes,
            MAX_ENCODE_INPUT_UTF8_BYTES
        );
        assert_eq!(encode_limits.max_output_bytes, MAX_ENCODE_OUTPUT_BYTES);
    }

    #[test]
    fn cancel_token_transitions() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn check_limit_refuses_observations_over_the_budget() {
        assert!(check_limit(4, 4, CodecLimitKind::RawBytes).is_ok());
        let err = check_limit(5, 4, CodecLimitKind::RawBytes).expect_err("over budget");
        assert_eq!(
            err,
            CodecError::LimitExceeded {
                kind: CodecLimitKind::RawBytes,
                observed: 5,
                limit: 4,
            }
        );
    }
}
