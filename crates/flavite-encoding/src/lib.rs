//! Strict file-content codecs for Flavite notes (RFC-0006 D-15 / §6.10).
//!
//! This crate owns exactly three things:
//!
//! * validated source-format descriptors ([`SourceFormat`], [`FormatStamp`],
//!   [`DecodePolicy`]) for strict ASCII, UTF-8, UTF-16LE and UTF-16BE;
//! * lossless decoding and encoding between raw file bytes and internal
//!   UTF-8 ([`decode`], [`encode`], [`stream`]);
//! * checked boundary maps between encoded source bytes and decoded UTF-8
//!   bytes ([`DecodeMap`]).
//!
//! It owns **no** filesystem IO, no EOL normalization, no Markdown, no
//! caret/grapheme state, no regex, no revisions and no recovery jobs. All
//! interactive/parser/search text stays normalized UTF-8; UTF-16 support
//! applies to serialized file contents only. `Utf16Offset` in
//! `flavite-syntax` keeps its separate platform-text-API meaning and is
//! never a file `SourceByte` here.
//!
//! The codec algorithms use only the standard library: strict `str` UTF-8
//! validation, `u16::{from_le_bytes, from_be_bytes}`,
//! `char::decode_utf16`, and `str::encode_utf16` with explicit endian
//! serialization. There is no encoding detector: `Auto` accepts a
//! recognized BOM signature and otherwise valid UTF-8; it never guesses
//! UTF-16 endianness from NUL patterns and never falls back lossily.

#![forbid(unsafe_code)]
// Length/position bookkeeping crosses `usize`↔`u64`/`u32` freely on the
// supported 64-bit targets. All *cumulative* offsets use `checked_add`/
// `checked_mul` with typed `LimitExceeded` (the RFC's "checked arithmetic"),
// so the raw casts below carry no overflow risk and are not worth a local
// `allow` at every indexing site.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]

mod decode;
mod encode;
mod format;
mod map;
pub mod stream;

pub use decode::{DecodedContent, decode};
pub use encode::{ascii_representable, encode, encoded_len};
pub use format::{
    CODEC_VERSION, CodecError, CodecLimitKind, DecodeLimits, DecodePolicy, EncodeLimits,
    FileEncoding, FormatError, FormatStamp, SourceFormat,
};
pub use map::{Checkpoint, DecodeMap, MAX_CHECKPOINT_SCALAR_GAP, ScalarSize};
