//! Source-format descriptors, decode/encode limits and typed codec errors.
//!
//! Everything here is validated: an unchecked `ascii` + BOM combination, a
//! wrong required signature, or an exceeded limit is a typed error, never a
//! silent fallback (RFC-0006 §6.10).

use core::fmt;

/// Codec/normalization implementation version recorded in format stamps and
/// index metadata. Bump when byte↔text mapping semantics change.
pub const CODEC_VERSION: u32 = 1;

/// The strict file-content encodings Flavite supports (§6.10).
///
/// UTF-32, Windows-1252/“extended ASCII”, Latin-1 and arbitrary codepages are
/// outside this RFC and outside this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "snake_case")
)]
pub enum FileEncoding {
    /// Strict 7-bit ASCII output policy: no BOM, non-ASCII is `Unencodable`.
    Ascii,
    /// UTF-8 with or without a BOM.
    Utf8,
    /// UTF-16 little-endian, with or without a BOM.
    Utf16Le,
    /// UTF-16 big-endian, with or without a BOM.
    Utf16Be,
}

impl FileEncoding {
    /// Stable machine-readable code used in persisted records.
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

    /// The exact BOM signature bytes this family writes when a BOM is present.
    /// ASCII has none.
    #[must_use]
    pub const fn bom_signature(self) -> &'static [u8] {
        match self {
            Self::Utf8 => &[0xEF, 0xBB, 0xBF],
            Self::Utf16Le => &[0xFF, 0xFE],
            Self::Utf16Be => &[0xFE, 0xFF],
            Self::Ascii => &[],
        }
    }

    /// Whether code units are two bytes (UTF-16), which makes an odd trailing
    /// byte an error and forbids a constant multiply-by-two offset mapping.
    #[must_use]
    pub const fn is_utf16(self) -> bool {
        matches!(self, Self::Utf16Le | Self::Utf16Be)
    }
}

/// A validated encoding + BOM combination.
///
/// Construction validates the combination: `Ascii` never carries a BOM, and
/// every other family may or may not (§6.10). The private fields force callers
/// through the validating constructors, so an unvalidated combination cannot
/// exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceFormat {
    encoding: FileEncoding,
    bom: bool,
}

impl SourceFormat {
    /// Build and validate a descriptor from an encoding and BOM flag.
    ///
    /// # Errors
    ///
    /// Returns [`FormatError::AsciiWithBom`] when `encoding` is ASCII with a
    /// BOM, the only invalid combination.
    pub const fn new(encoding: FileEncoding, bom: bool) -> Result<Self, FormatError> {
        if matches!(encoding, FileEncoding::Ascii) && bom {
            return Err(FormatError::AsciiWithBom);
        }
        Ok(Self { encoding, bom })
    }

    /// UTF-8 without a BOM. The default for newly created notes.
    #[must_use]
    pub const fn utf8() -> Self {
        Self {
            encoding: FileEncoding::Utf8,
            bom: false,
        }
    }

    /// UTF-8 with a BOM.
    #[must_use]
    pub const fn utf8_with_bom() -> Self {
        Self {
            encoding: FileEncoding::Utf8,
            bom: true,
        }
    }

    /// Strict ASCII (never has a BOM).
    #[must_use]
    pub const fn ascii() -> Self {
        Self {
            encoding: FileEncoding::Ascii,
            bom: false,
        }
    }

    /// UTF-16LE without a BOM.
    #[must_use]
    pub const fn utf16le() -> Self {
        Self {
            encoding: FileEncoding::Utf16Le,
            bom: false,
        }
    }

    /// UTF-16LE with a BOM.
    #[must_use]
    pub const fn utf16le_with_bom() -> Self {
        Self {
            encoding: FileEncoding::Utf16Le,
            bom: true,
        }
    }

    /// UTF-16BE without a BOM.
    #[must_use]
    pub const fn utf16be() -> Self {
        Self {
            encoding: FileEncoding::Utf16Be,
            bom: false,
        }
    }

    /// UTF-16BE with a BOM.
    #[must_use]
    pub const fn utf16be_with_bom() -> Self {
        Self {
            encoding: FileEncoding::Utf16Be,
            bom: true,
        }
    }

    #[must_use]
    pub const fn encoding(self) -> FileEncoding {
        self.encoding
    }

    #[must_use]
    pub const fn bom(self) -> bool {
        self.bom
    }

    /// Number of BOM signature bytes physically present in a source that uses
    /// this descriptor (0 or the signature length).
    #[must_use]
    pub const fn bom_len(self) -> usize {
        if self.bom {
            // `bom_signature` returns the exact family signature; ASCII+bom is
            // unconstructable, so this is always the real length.
            self.encoding.bom_signature().len()
        } else {
            0
        }
    }

    /// The BOM bytes to prepend when serializing under this descriptor.
    #[must_use]
    pub fn bom_bytes(self) -> &'static [u8] {
        if self.bom {
            self.encoding.bom_signature()
        } else {
            &[]
        }
    }

    /// Stable machine-readable code for persisted records, e.g. `"utf16le"`.
    #[must_use]
    pub const fn encoding_code(self) -> &'static str {
        self.encoding.code()
    }
}

impl fmt::Display for SourceFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.bom {
            write!(f, "{}+bom", self.encoding.code())
        } else {
            f.write_str(self.encoding.code())
        }
    }
}

impl Default for SourceFormat {
    /// Newly created notes are UTF-8 without a BOM (§6.10 default).
    fn default() -> Self {
        Self::utf8()
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for SourceFormat {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&RawSourceFormat::from(*self), s)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for SourceFormat {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = <RawSourceFormat as serde::Deserialize>::deserialize(d)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// Serde transport shape for [`SourceFormat`]; validation runs in `TryFrom`.
#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSourceFormat {
    encoding: FileEncoding,
    bom: bool,
}

#[cfg(feature = "serde")]
impl From<SourceFormat> for RawSourceFormat {
    fn from(f: SourceFormat) -> Self {
        Self {
            encoding: f.encoding,
            bom: f.bom,
        }
    }
}

#[cfg(feature = "serde")]
impl TryFrom<RawSourceFormat> for SourceFormat {
    type Error = FormatError;
    fn try_from(raw: RawSourceFormat) -> Result<Self, Self::Error> {
        Self::new(raw.encoding, raw.bom)
    }
}

/// Invalid descriptor combination supplied to [`SourceFormat::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// ASCII content never carries a BOM.
    AsciiWithBom,
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AsciiWithBom => f.write_str("strict ASCII cannot have a BOM"),
        }
    }
}

impl std::error::Error for FormatError {}

/// Versioned, cache-stampable provenance of a source format (§6.1, §6.10).
///
/// A format stamp is what invalidates raw-hash/source-span observations even
/// when the normalized text is byte-identical: two files with the same
/// decoded text but different descriptors produce different stamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(deny_unknown_fields)
)]
pub struct FormatStamp {
    /// The authoritative descriptor.
    pub format: SourceFormat,
    /// Codec implementation version in effect when this stamp was created.
    pub codec_version: u32,
    /// Generation of the applicable device-local encoding-hint record, if this
    /// format came from a user-confirmed hint rather than raw auto-detection.
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

    /// True when both stamps describe the same descriptor and codec version;
    /// the hint generation participates so a re-confirmed hint can invalidate
    /// stale observations.
    #[must_use]
    pub fn is_compatible(&self, other: &Self) -> bool {
        self.format == other.format
            && self.codec_version == other.codec_version
            && self.hint_generation == other.hint_generation
    }
}

/// How a decoder chooses a descriptor when reading raw bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodePolicy {
    /// Accept a recognized BOM signature; otherwise require valid UTF-8.
    ///
    /// Auto never infers UTF-16 endianness from NUL patterns and never marks
    /// ASCII-only bytes as “strict ASCII” (they are simply valid UTF-8).
    Auto,
    /// Decode under an explicit descriptor. A `bom: true` explicit descriptor
    /// requires and removes exactly that signature; a `bom: false` descriptor
    /// does **not** strip a leading signature-shaped byte sequence — it treats
    /// a leading U+FEFF as content.
    Explicit(SourceFormat),
}

impl DecodePolicy {
    /// The descriptor this policy will force, if any.
    #[must_use]
    pub const fn explicit_format(self) -> Option<SourceFormat> {
        match self {
            Self::Auto => None,
            Self::Explicit(f) => Some(f),
        }
    }
}

/// Which kind of limit was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecLimitKind {
    /// Raw encoded input bytes.
    RawInput,
    /// Decoded UTF-8 output bytes.
    DecodedUtf8,
    /// Decode-map overhead bytes.
    MapBytes,
    /// Encode input UTF-8 bytes.
    EncodeInput,
    /// Encode output bytes.
    EncodeOutput,
}

/// Independent byte-count ceilings. Raw, decoded and serialized output each
/// have their own bound; a code-unit count never substitutes for a byte count
/// (§6.10, NFR-19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    pub max_raw_bytes: u64,
    pub max_decoded_utf8_bytes: u64,
    pub max_map_bytes: u64,
}

impl DecodeLimits {
    /// The RFC-0006 default envelope (32 MiB raw/decoded, 4 MiB map overhead
    /// per 32 MiB input/output-bound pair, NFR-18/19).
    #[must_use]
    pub const fn default_envelope() -> Self {
        Self {
            max_raw_bytes: 32 * 1024 * 1024,
            max_decoded_utf8_bytes: 32 * 1024 * 1024,
            max_map_bytes: 4 * 1024 * 1024,
        }
    }
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self::default_envelope()
    }
}

/// Independent input/output ceilings for encoding (§6.10, NFR-19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeLimits {
    pub max_input_utf8_bytes: u64,
    pub max_output_bytes: u64,
}

impl EncodeLimits {
    /// The RFC-0006 default envelope (32 MiB in, 32 MiB out).
    #[must_use]
    pub const fn default_envelope() -> Self {
        Self {
            max_input_utf8_bytes: 32 * 1024 * 1024,
            max_output_bytes: 32 * 1024 * 1024,
        }
    }
}

impl Default for EncodeLimits {
    fn default() -> Self {
        Self::default_envelope()
    }
}

/// Typed codec failures. Every variant carries enough information for an
/// actionable UI message, and none ever substitute U+FFFD for bad bytes
/// (§6.10, FR-22).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// Malformed UTF-8; `source_byte` is the offending encoded byte offset.
    InvalidUtf8 { source_byte: u64 },
    /// A UTF-16 code unit sequence that cannot be decoded (unpaired
    /// surrogate); `source_byte` is the start of the offending code unit.
    InvalidUtf16 { source_byte: u64 },
    /// UTF-16 byte count is odd (a trailing half code unit).
    OddUtf16ByteCount,
    /// An explicit descriptor with `bom: true` did not find its signature.
    BomConflict,
    /// A longer unsupported BOM signature (e.g. UTF-32) was found before the
    /// recognized ones.
    UnsupportedOrAmbiguousBom,
    /// An explicit ASCII policy encountered a byte that is not 7-bit ASCII at
    /// `source_byte`; ASCII input must be byte-representable.
    AsciiViolation { source_byte: u64 },
    /// Strict ASCII output cannot represent the character at `text_byte`.
    Unencodable { text_byte: u64 },
    /// A byte limit was exceeded.
    LimitExceeded {
        kind: CodecLimitKind,
        observed: u64,
        limit: u64,
    },
    /// Cooperative cancellation was observed.
    Cancelled,
}

impl CodecError {
    /// Build a limit-exceeded error.
    #[must_use]
    pub const fn limit(kind: CodecLimitKind, observed: u64, limit: u64) -> Self {
        Self::LimitExceeded {
            kind,
            observed,
            limit,
        }
    }
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8 { source_byte } => {
                write!(f, "invalid UTF-8 sequence at source byte {source_byte}")
            }
            Self::InvalidUtf16 { source_byte } => {
                write!(f, "invalid UTF-16 code unit at source byte {source_byte}")
            }
            Self::OddUtf16ByteCount => f.write_str("UTF-16 source has an odd byte count"),
            Self::BomConflict => f.write_str("required BOM signature not present"),
            Self::UnsupportedOrAmbiguousBom => {
                f.write_str("unsupported or ambiguous byte-order mark")
            }
            Self::AsciiViolation { source_byte } => {
                write!(f, "byte {source_byte} is not valid strict ASCII input")
            }
            Self::Unencodable { text_byte } => {
                write!(
                    f,
                    "strict ASCII cannot encode the character at byte {text_byte}"
                )
            }
            Self::LimitExceeded {
                kind,
                observed,
                limit,
            } => write!(f, "{kind:?} limit {limit} exceeded (observed {observed})"),
            Self::Cancelled => f.write_str("codec cancelled"),
        }
    }
}

impl std::error::Error for CodecError {}
