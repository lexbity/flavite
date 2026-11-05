//! Whole-buffer strict decoding built on the streaming state machine, so
//! cancellation and limit granularity behave identically for any input shape
//! (RFC-0006 §6.10, NFR-18/19).

use std::sync::Arc;

use crate::format::{CodecError, CodecLimitKind, DecodeLimits, DecodePolicy};
use crate::map::{Checkpoint, DecodeMap};
use crate::stream::{CHECK_INTERVAL, DecodedMeta, Decoder};

/// A complete strict decode of one source buffer.
#[derive(Debug, Clone)]
pub struct DecodedContent {
    /// Decoded UTF-8 with the selected BOM signature removed and original
    /// CR/LF scalars preserved. Never contains U+FFFD produced by a lossy
    /// fallback; a content U+FFFD may of course be present if the file really
    /// contained it.
    text: String,
    /// Boundary map for the shared original bytes.
    map: DecodeMap,
    /// Shared original encoded bytes.
    source: Arc<[u8]>,
}

impl DecodedContent {
    /// The decoded UTF-8 bytes (BOM removed, physical EOLs preserved).
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.text.as_bytes()
    }

    /// The decoded UTF-8 content as a string slice.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The applied descriptor.
    #[must_use]
    pub fn format(&self) -> crate::format::SourceFormat {
        self.map.format()
    }

    /// The `SourceByte ↔ decoded byte` boundary map.
    #[must_use]
    pub const fn map(&self) -> &DecodeMap {
        &self.map
    }

    /// The shared original encoded bytes.
    #[must_use]
    pub fn source(&self) -> &Arc<[u8]> {
        &self.source
    }
}

/// Decode raw file bytes strictly under a policy.
///
/// This is the single codec entry point for note content. It performs BOM
/// handling per [`DecodePolicy`], strict validation (no U+FFFD, no guessing),
/// and produces the boundary [`DecodeMap`] composed later with the EOL map by
/// `flavite-syntax`. It performs no newline normalization and reads no files.
///
/// # Errors
///
/// Every failure is a typed [`CodecError`]; the function has no side effects
/// and the source buffer is never modified (FR-22).
pub fn decode(
    source: &[u8],
    policy: DecodePolicy,
    limits: &DecodeLimits,
    cancel: &mut dyn FnMut() -> bool,
) -> Result<DecodedContent, CodecError> {
    if source.len() as u64 > limits.max_raw_bytes {
        return Err(CodecError::limit(
            CodecLimitKind::RawInput,
            source.len() as u64,
            limits.max_raw_bytes,
        ));
    }
    let shared: Arc<[u8]> = Arc::from(source);
    let mut decoder = Decoder::new(policy, *limits);
    let mut text = String::new();
    let mut offset = 0usize;
    while offset < source.len() {
        if offset > 0 && offset.is_multiple_of(CHECK_INTERVAL as usize) && cancel() {
            return Err(CodecError::Cancelled);
        }
        let end = (offset + CHECK_INTERVAL as usize).min(source.len());
        decoder.feed(&source[offset..end], &mut text)?;
        offset = end;
    }
    let meta = decoder.finish(&mut text)?;
    if cancel() {
        return Err(CodecError::Cancelled);
    }
    let map = build_map(&meta, shared.clone(), limits)?;
    Ok(DecodedContent {
        text,
        map,
        source: shared,
    })
}

fn build_map(
    meta: &DecodedMeta,
    source: Arc<[u8]>,
    limits: &DecodeLimits,
) -> Result<DecodeMap, CodecError> {
    let map_bytes = meta.checkpoints.len() * std::mem::size_of::<Checkpoint>();
    if map_bytes as u64 > limits.max_map_bytes {
        return Err(CodecError::limit(
            CodecLimitKind::MapBytes,
            map_bytes as u64,
            limits.max_map_bytes,
        ));
    }
    let bom_len = u32::try_from(meta.bom_len).map_err(|_| {
        CodecError::limit(CodecLimitKind::RawInput, meta.bom_len, u64::from(u32::MAX))
    })?;
    Ok(if meta.format.encoding().is_utf16() {
        DecodeMap::utf16(
            meta.format.encoding(),
            meta.format,
            source,
            bom_len,
            meta.decoded_len,
            meta.checkpoints.clone(),
        )
    } else {
        DecodeMap::identity(
            meta.format.encoding(),
            meta.format,
            source,
            bom_len,
            meta.decoded_len,
        )
    })
}
