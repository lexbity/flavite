#![forbid(unsafe_code)]
//! Tier 1 — durability-critical. Strict byte codecs for the note source
//! formats RFC-0006 supports (ASCII, UTF-8, UTF-16LE/BE), the shared
//! [`SourceFormat`] descriptor and checked encoded↔decoded byte maps
//! (§6.10, D-15).
//!
//! Ownership boundary (§4.1): this crate has no Flavite dependencies, no
//! filesystem IO, no EOL/Markdown/normalization policy and no lossy
//! fallback. All interactive/parser/search text stays normalized UTF-8;
//! codecs apply to serialized **file contents** only.
//!
//! * Decode removes only the signature its [`DecodePolicy`] selects,
//!   preserves every content scalar (including an additional U+FEFF) and
//!   performs no EOL/NFC normalization. Auto recognizes signatures only —
//!   it never infers UTF-16 endianness from NUL patterns and never marks
//!   BOM-less ASCII-only bytes as "strict ASCII".
//! * Encode validates complete representability and the exact output size
//!   before any buffer reservation; strict ASCII never substitutes `?` or
//!   escapes.
//! * The [`DecodeMap`] shares the immutable original bytes, uses sparse
//!   scalar-boundary checkpoints at most 255 scalars apart with bounded
//!   local decoding, and is total/invertible on the legal boundary domain
//!   (BOM/surrogate/CRLF interiors and UTF-8 continuations are rejected).
//!
//! Diagnostics never contain note content — only error kind, byte offset
//! and size.

pub mod decode;
pub mod encode;
pub mod format;
pub mod map;
pub mod stream;

pub use decode::{DecodedContent, decode, decode_shared};
pub use encode::encode;
pub use format::{
    CANCEL_CHECK_BYTES, CODEC_VERSION, CancelToken, CodecError, CodecLimitKind, DecodeLimits,
    DecodePolicy, EncodeLimits, FileEncoding, FormatStamp, MAX_DECODED_UTF8_BYTES,
    MAX_ENCODE_INPUT_UTF8_BYTES, MAX_ENCODE_OUTPUT_BYTES, MAX_MAP_BYTES, MAX_RAW_BYTES,
    NOT_CANCELLED, SourceFormat,
};
pub use map::DecodeMap;
pub use stream::{StreamDecoded, StreamingDecoder, StreamingEncoder};
