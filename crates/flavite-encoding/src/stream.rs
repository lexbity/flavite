//! Incremental streaming codecs (RFC-0006 §6.10, S00).
//!
//! Decoder/encoder state handles partial BOM, code-unit, surrogate and
//! UTF-8 chunks; incomplete suffixes are reported only at explicit
//! [`StreamingDecoder::finish`]. There is no IO trait and no lossy
//! fallback. Streaming decode yields text and the resolved
//! [`SourceFormat`]; the checked byte map is built by the one-shot
//! [`decode()`](fn@crate::decode) over whole sources.

use crate::decode::LONGER_UNSUPPORTED_SIGNATURES;
use crate::format::{
    CANCEL_CHECK_BYTES, CancelToken, CodecError, CodecLimitKind, DecodeLimits, DecodePolicy,
    EncodeLimits, FileEncoding, SourceFormat, check_limit,
};

/// Byte prefixes that can begin a recognized signature (Auto waits for
/// these leads before deciding, up to the longest signature length).
const SIGNATURE_LEADS: [u8; 8] = [0xEF, 0xFF, 0xFE, 0xF7, 0xDD, 0x0E, 0x2B, 0x00];

/// Longest signature prefix Auto must be able to observe (UTF-32LE
/// `FF FE 00 00` vs UTF-16LE `FF FE`).
const MAX_SIGNATURE_LEN: usize = 4;

/// Decoded text and resolved descriptor from a streaming decode.
#[derive(Debug, Clone)]
pub struct StreamDecoded {
    text: String,
    format: SourceFormat,
}

impl StreamDecoded {
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
}

#[derive(Debug)]
enum Phase {
    /// Signature not yet resolved; the first bytes are buffered.
    Sig {
        pending: Vec<u8>,
    },
    Active(ActiveState),
}

#[derive(Debug)]
enum ActiveState {
    Ascii,
    Utf8(Utf8Incremental),
    Utf16(Utf16Incremental),
}

/// Incremental decoder for byte chunks of unknown boundaries.
#[derive(Debug)]
pub struct StreamingDecoder {
    policy: DecodePolicy,
    limits: DecodeLimits,
    raw_total: usize,
    text: String,
    format: Option<SourceFormat>,
    phase: Phase,
}

impl StreamingDecoder {
    /// A decoder for `policy` under `limits`.
    #[must_use]
    pub fn new(policy: DecodePolicy, limits: DecodeLimits) -> Self {
        Self {
            policy,
            limits,
            raw_total: 0,
            text: String::new(),
            format: None,
            phase: Phase::Sig {
                pending: Vec::new(),
            },
        }
    }

    /// Feed the next source chunk. Partial signatures, code units, UTF-8
    /// sequences and surrogate pairs are held across pushes.
    ///
    /// # Errors
    ///
    /// [`CodecError`] for malformed input (first invalid sequence with its
    /// source byte), exceeded limits and cancellation.
    pub fn push(&mut self, chunk: &[u8], cancel: &CancelToken) -> Result<(), CodecError> {
        if cancel.is_cancelled() {
            return Err(CodecError::Cancelled);
        }
        let new_total =
            self.raw_total
                .checked_add(chunk.len())
                .ok_or(CodecError::LimitExceeded {
                    kind: CodecLimitKind::RawBytes,
                    observed: u64::MAX,
                    limit: self.limits.max_raw_bytes,
                })?;
        check_limit(
            new_total,
            self.limits.max_raw_bytes,
            CodecLimitKind::RawBytes,
        )?;
        let base = self.raw_total;
        self.raw_total = new_total;
        for (offset, sub) in chunk.chunks(CANCEL_CHECK_BYTES).enumerate() {
            if cancel.is_cancelled() {
                return Err(CodecError::Cancelled);
            }
            self.absorb(sub, base + offset * CANCEL_CHECK_BYTES)?;
        }
        Ok(())
    }

    /// Finish the stream, reporting incomplete suffixes only here.
    ///
    /// # Errors
    ///
    /// [`CodecError::InvalidUtf8`] / [`CodecError::InvalidUtf16`] /
    /// [`CodecError::OddUtf16ByteCount`] for truncated sequences and
    /// [`CodecError::BomConflict`] for truncated explicit signatures.
    pub fn finish(mut self) -> Result<StreamDecoded, CodecError> {
        if let Phase::Sig { pending } = &mut self.phase {
            let pending = std::mem::take(pending);
            self.resolve_at_end_of_input(&pending)?;
        }
        match &self.phase {
            Phase::Sig { .. } => unreachable!("finish resolves the signature phase"),
            Phase::Active(ActiveState::Ascii) => {}
            Phase::Active(ActiveState::Utf8(state)) => state.finish()?,
            Phase::Active(ActiveState::Utf16(state)) => state.finish()?,
        }
        let format = self.format.unwrap_or_else(|| {
            // No bytes were ever pushed: an empty Auto-decoded file.
            SourceFormat::new_unchecked(FileEncoding::Utf8, false)
        });
        Ok(StreamDecoded {
            text: self.text,
            format,
        })
    }

    fn absorb(&mut self, chunk: &[u8], base: usize) -> Result<(), CodecError> {
        if matches!(self.phase, Phase::Sig { .. }) {
            let pending = match &mut self.phase {
                Phase::Sig { pending } => pending,
                Phase::Active(_) => unreachable!("checked above"),
            };
            pending.extend_from_slice(chunk);
            return self.try_resolve();
        }
        self.absorb_active(chunk, base)
    }

    fn absorb_active(&mut self, chunk: &[u8], base: usize) -> Result<(), CodecError> {
        match &mut self.phase {
            Phase::Sig { .. } => unreachable!("resolved before active absorb"),
            Phase::Active(ActiveState::Ascii) => {
                if !chunk.is_ascii() {
                    let Some(idx) = chunk.iter().position(|b| *b >= 0x80) else {
                        unreachable!("non-ASCII chunk contains a >= 0x80 byte");
                    };
                    return Err(CodecError::InvalidUtf8 {
                        source_byte: base + idx,
                    });
                }
                check_limit(
                    self.text.len() + chunk.len(),
                    self.limits.max_decoded_utf8_bytes,
                    CodecLimitKind::DecodedUtf8Bytes,
                )?;
                // All-ASCII bytes are valid UTF-8.
                let text = std::str::from_utf8(chunk).expect("all-ASCII chunk is valid UTF-8");
                self.text.push_str(text);
                Ok(())
            }
            Phase::Active(ActiveState::Utf8(state)) => {
                let max_decoded = self.limits.max_decoded_utf8_bytes;
                state.feed(chunk, base, &mut self.text, max_decoded)
            }
            Phase::Active(ActiveState::Utf16(state)) => {
                let max_decoded = self.limits.max_decoded_utf8_bytes;
                state.feed(chunk, base, &mut self.text, max_decoded)
            }
        }
    }

    fn try_resolve(&mut self) -> Result<(), CodecError> {
        let pending = match &mut self.phase {
            Phase::Sig { pending } => std::mem::take(pending),
            Phase::Active(_) => return Ok(()),
        };
        if pending.is_empty() {
            self.phase = Phase::Sig { pending };
            return Ok(());
        }
        match self.policy {
            DecodePolicy::Explicit(format) => {
                let sig = format.encoding().signature();
                if !format.bom() {
                    // Explicit BOM-less never strips signatures.
                    self.activate(format, 0, &pending)
                } else if pending.starts_with(sig) {
                    self.activate(format, sig.len(), &pending)
                } else if pending.len() >= sig.len() {
                    Err(CodecError::BomConflict)
                } else {
                    // Still a strict prefix of the required signature.
                    self.phase = Phase::Sig { pending };
                    Ok(())
                }
            }
            DecodePolicy::Auto => self.resolve_auto(pending),
        }
    }

    fn resolve_auto(&mut self, pending: Vec<u8>) -> Result<(), CodecError> {
        if !SIGNATURE_LEADS.contains(&pending[0]) {
            return self.activate(
                SourceFormat::new_unchecked(FileEncoding::Utf8, false),
                0,
                &pending,
            );
        }
        // Longer unsupported signatures are checked before the UTF-16
        // prefixes (§6.10) so UTF-32 is diagnosed, not misdecoded.
        for sig in &LONGER_UNSUPPORTED_SIGNATURES {
            if pending.starts_with(sig) {
                return Err(CodecError::UnsupportedOrAmbiguousBom);
            }
        }
        if pending.starts_with(FileEncoding::Utf8.signature()) {
            return self.activate(
                SourceFormat::new_unchecked(FileEncoding::Utf8, true),
                3,
                &pending,
            );
        }
        if pending.starts_with(FileEncoding::Utf16Le.signature()) {
            // FF FE is also the prefix of the UTF-32LE signature: wait for
            // the fourth byte before committing (§6.10 ambiguity rule).
            if pending.len() >= MAX_SIGNATURE_LEN {
                return self.activate(
                    SourceFormat::new_unchecked(FileEncoding::Utf16Le, true),
                    2,
                    &pending,
                );
            }
            self.phase = Phase::Sig { pending };
            return Ok(());
        }
        if pending.starts_with(FileEncoding::Utf16Be.signature()) {
            return self.activate(
                SourceFormat::new_unchecked(FileEncoding::Utf16Be, true),
                2,
                &pending,
            );
        }
        if pending.len() >= MAX_SIGNATURE_LEN {
            return self.activate(
                SourceFormat::new_unchecked(FileEncoding::Utf8, false),
                0,
                &pending,
            );
        }
        // Could still become a longer signature; keep buffering.
        self.phase = Phase::Sig { pending };
        Ok(())
    }

    fn resolve_at_end_of_input(&mut self, pending: &[u8]) -> Result<(), CodecError> {
        match self.policy {
            DecodePolicy::Explicit(format) => {
                let sig = format.encoding().signature();
                if !format.bom() {
                    return self.activate(format, 0, pending);
                }
                if pending == sig {
                    // Exactly the required signature and nothing else.
                    return self.activate(format, sig.len(), pending);
                }
                Err(CodecError::BomConflict)
            }
            DecodePolicy::Auto => {
                // A UTF-16 BOM prefix that never grew past the signature
                // buffer (`FF FE` + one odd byte) is still a UTF-16 file
                // with a truncated code unit: activate the format so
                // `finish` reports OddUtf16ByteCount — the one-shot
                // decoder's contract (one codec contract, FR-24).
                if pending.starts_with(FileEncoding::Utf16Le.signature())
                    || pending.starts_with(FileEncoding::Utf16Be.signature())
                {
                    let le = pending.starts_with(FileEncoding::Utf16Le.signature());
                    return self.activate(
                        SourceFormat::new_unchecked(
                            if le {
                                FileEncoding::Utf16Le
                            } else {
                                FileEncoding::Utf16Be
                            },
                            true,
                        ),
                        2,
                        pending,
                    );
                }
                if pending == FileEncoding::Utf16Le.signature() {
                    return self.activate(
                        SourceFormat::new_unchecked(FileEncoding::Utf16Le, true),
                        2,
                        pending,
                    );
                }
                if pending == FileEncoding::Utf16Be.signature() {
                    return self.activate(
                        SourceFormat::new_unchecked(FileEncoding::Utf16Be, true),
                        2,
                        pending,
                    );
                }
                if pending == FileEncoding::Utf8.signature() {
                    return self.activate(
                        SourceFormat::new_unchecked(FileEncoding::Utf8, true),
                        3,
                        pending,
                    );
                }
                // Not a complete signature: strict UTF-8 (invalid bytes
                // surface with their source position).
                self.activate(
                    SourceFormat::new_unchecked(FileEncoding::Utf8, false),
                    0,
                    pending,
                )
            }
        }
    }

    fn activate(
        &mut self,
        format: SourceFormat,
        sig_len: usize,
        pending: &[u8],
    ) -> Result<(), CodecError> {
        self.format = Some(format);
        self.phase = Phase::Active(match format.encoding() {
            FileEncoding::Ascii => ActiveState::Ascii,
            FileEncoding::Utf8 => ActiveState::Utf8(Utf8Incremental::new()),
            FileEncoding::Utf16Le | FileEncoding::Utf16Be => ActiveState::Utf16(
                Utf16Incremental::new(format.encoding() == FileEncoding::Utf16Le),
            ),
        });
        self.absorb_active(&pending[sig_len..], sig_len)
    }
}

/// Incremental strict UTF-8 validator: a ≤3-byte incomplete tail plus
/// standard-library validation of `tail + chunk` as one sequence (D-15:
/// standard-library strict validation is the contract, not a private
/// lead/continuation table). Incomplete suffixes are retained and never
/// error before `finish`.
#[derive(Debug)]
struct Utf8Incremental {
    /// Incomplete trailing bytes of earlier pushes (≤3: never a complete
    /// scalar on its own).
    tail: Vec<u8>,
    /// Absolute source byte where `tail[0]` sits.
    tail_start: usize,
}

impl Utf8Incremental {
    fn new() -> Self {
        Self {
            tail: Vec::new(),
            tail_start: 0,
        }
    }

    fn feed(
        &mut self,
        chunk: &[u8],
        base: usize,
        out: &mut String,
        max_decoded: u64,
    ) -> Result<(), CodecError> {
        if chunk.is_empty() {
            return Ok(());
        }
        if self.tail.is_empty() {
            self.tail_start = base;
        }
        self.tail.extend_from_slice(chunk);
        match std::str::from_utf8(&self.tail) {
            Ok(text) => {
                check_limit(
                    out.len() + text.len(),
                    max_decoded,
                    CodecLimitKind::DecodedUtf8Bytes,
                )?;
                out.push_str(text);
                self.tail.clear();
                Ok(())
            }
            Err(e) => {
                let valid = e.valid_up_to();
                check_limit(
                    out.len() + valid,
                    max_decoded,
                    CodecLimitKind::DecodedUtf8Bytes,
                )?;
                let prefix =
                    std::str::from_utf8(&self.tail[..valid]).expect("valid_up_to is a boundary");
                out.push_str(prefix);
                if e.error_len().is_some() {
                    // A definite encoding error at this source position.
                    return Err(CodecError::InvalidUtf8 {
                        source_byte: self.tail_start + valid,
                    });
                }
                // Incomplete suffix: retained for the next push (or
                // reported by `finish`).
                self.tail.drain(..valid);
                Ok(())
            }
        }
    }

    fn finish(&self) -> Result<(), CodecError> {
        if self.tail.is_empty() {
            Ok(())
        } else {
            Err(CodecError::InvalidUtf8 {
                source_byte: self.tail_start,
            })
        }
    }
}

fn push_scalar(out: &mut String, scalar: char, max_decoded: u64) -> Result<(), CodecError> {
    check_limit(
        out.len() + scalar.len_utf8(),
        max_decoded,
        CodecLimitKind::DecodedUtf8Bytes,
    )?;
    out.push(scalar);
    Ok(())
}

/// Incremental strict UTF-16 decoder holding odd trailing bytes and pending
/// high surrogates across pushes.
#[derive(Debug)]
struct Utf16Incremental {
    le: bool,
    /// An odd trailing byte and the absolute source position it was read
    /// at, held across pushes.
    pending_byte: Option<(u8, usize)>,
    pending_high: Option<(u16, usize)>,
}

impl Utf16Incremental {
    fn new(le: bool) -> Self {
        Self {
            le,
            pending_byte: None,
            pending_high: None,
        }
    }

    /// Assemble a unit from its bytes in stream order (LE: low first,
    /// BE: high first).
    fn read_unit(&self, first: u8, second: u8) -> u16 {
        if self.le {
            u16::from_le_bytes([first, second])
        } else {
            u16::from_be_bytes([first, second])
        }
    }

    fn feed(
        &mut self,
        chunk: &[u8],
        base: usize,
        out: &mut String,
        max_decoded: u64,
    ) -> Result<(), CodecError> {
        if chunk.is_empty() {
            return Ok(());
        }
        let mut idx = 0usize;
        if let Some((held, held_pos)) = self.pending_byte.take() {
            let unit = self.read_unit(held, chunk[0]);
            self.process_unit(unit, held_pos, out, max_decoded)?;
            idx = 1;
        }
        while idx + 1 < chunk.len() {
            let unit = self.read_unit(chunk[idx], chunk[idx + 1]);
            self.process_unit(unit, base + idx, out, max_decoded)?;
            idx += 2;
        }
        if idx < chunk.len() {
            self.pending_byte = Some((chunk[chunk.len() - 1], base + chunk.len() - 1));
        }
        Ok(())
    }

    fn process_unit(
        &mut self,
        unit: u16,
        source_byte: usize,
        out: &mut String,
        max_decoded: u64,
    ) -> Result<(), CodecError> {
        if let Some((high, high_source_byte)) = self.pending_high {
            if (0xDC00..=0xDFFF).contains(&unit) {
                self.pending_high = None;
                let combined =
                    0x1_0000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00);
                let scalar = char::from_u32(combined).ok_or(CodecError::InvalidUtf16 {
                    source_byte: high_source_byte,
                })?;
                push_scalar(out, scalar, max_decoded)?;
                return Ok(());
            }
            return Err(CodecError::InvalidUtf16 {
                source_byte: high_source_byte,
            });
        }
        if (0xD800..=0xDBFF).contains(&unit) {
            self.pending_high = Some((unit, source_byte));
        } else if (0xDC00..=0xDFFF).contains(&unit) {
            return Err(CodecError::InvalidUtf16 { source_byte });
        } else {
            let scalar =
                char::from_u32(u32::from(unit)).ok_or(CodecError::InvalidUtf16 { source_byte })?;
            push_scalar(out, scalar, max_decoded)?;
        }
        Ok(())
    }

    fn finish(&self) -> Result<(), CodecError> {
        if self.pending_byte.is_some() {
            return Err(CodecError::OddUtf16ByteCount);
        }
        if let Some((_, source_byte)) = self.pending_high {
            return Err(CodecError::InvalidUtf16 { source_byte });
        }
        Ok(())
    }
}

/// Incremental encoder producing bytes for the descriptor's format.
#[derive(Debug)]
pub struct StreamingEncoder {
    format: SourceFormat,
    limits: EncodeLimits,
    written: u64,
    input_pos: usize,
    signature_written: bool,
}

impl StreamingEncoder {
    /// An encoder for `format` under `limits`.
    #[must_use]
    pub fn new(format: SourceFormat, limits: EncodeLimits) -> Self {
        Self {
            format,
            limits,
            written: 0,
            input_pos: 0,
            signature_written: false,
        }
    }

    /// Encode the next text chunk into `out`. The requested signature is
    /// written once before the first content bytes.
    ///
    /// # Errors
    ///
    /// [`CodecError::Unencodable`] under strict ASCII,
    /// [`CodecError::LimitExceeded`] for input/output budgets and
    /// [`CodecError::Cancelled`] when the token fires. On error, `out`
    /// retains only what was written before it.
    pub fn push(
        &mut self,
        text: &str,
        out: &mut Vec<u8>,
        cancel: &CancelToken,
    ) -> Result<(), CodecError> {
        if cancel.is_cancelled() {
            return Err(CodecError::Cancelled);
        }
        let new_pos = self
            .input_pos
            .checked_add(text.len())
            .ok_or(CodecError::LimitExceeded {
                kind: CodecLimitKind::InputUtf8Bytes,
                observed: u64::MAX,
                limit: self.limits.max_input_utf8_bytes,
            })?;
        check_limit(
            new_pos,
            self.limits.max_input_utf8_bytes,
            CodecLimitKind::InputUtf8Bytes,
        )?;
        if !self.signature_written {
            self.signature_written = true;
            if self.format.bom() {
                let sig = self.format.encoding().signature();
                self.write_bytes(sig, out)?;
            }
        }
        let mut write_chunk = |sub: &str| -> Result<(), CodecError> {
            if cancel.is_cancelled() {
                return Err(CodecError::Cancelled);
            }
            self.push_sub(sub, out)
        };
        for_each_char_boundary_chunk(text, CANCEL_CHECK_BYTES, &mut write_chunk)?;
        self.input_pos = new_pos;
        Ok(())
    }

    fn push_sub(&mut self, text: &str, out: &mut Vec<u8>) -> Result<(), CodecError> {
        match self.format.encoding() {
            FileEncoding::Ascii => {
                if let Some(idx) = text.as_bytes().iter().position(|b| *b >= 0x80) {
                    return Err(CodecError::Unencodable {
                        text_byte: self.input_pos + idx,
                    });
                }
                self.write_bytes(text.as_bytes(), out)
            }
            FileEncoding::Utf8 => self.write_bytes(text.as_bytes(), out),
            FileEncoding::Utf16Le | FileEncoding::Utf16Be => {
                let le = self.format.encoding() == FileEncoding::Utf16Le;
                for ch in text.chars() {
                    let mut units = [0u16; 2];
                    let count = ch.encode_utf16(&mut units).len();
                    let encoded = count * 2;
                    self.check_output(encoded)?;
                    for unit in &units[..count] {
                        let bytes = if le {
                            unit.to_le_bytes()
                        } else {
                            unit.to_be_bytes()
                        };
                        out.extend_from_slice(&bytes);
                    }
                    self.record_written(encoded);
                }
                Ok(())
            }
        }
    }

    /// Checked-then-written byte append (check precedes any growth).
    fn write_bytes(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> Result<(), CodecError> {
        self.check_output(bytes.len())?;
        out.extend_from_slice(bytes);
        self.record_written(bytes.len());
        Ok(())
    }

    fn check_output(&self, extra: usize) -> Result<(), CodecError> {
        let extra64 = u64::try_from(extra).unwrap_or(u64::MAX);
        match self.written.checked_add(extra64) {
            Some(next) if next <= self.limits.max_output_bytes => Ok(()),
            Some(next) => Err(CodecError::LimitExceeded {
                kind: CodecLimitKind::OutputBytes,
                observed: next,
                limit: self.limits.max_output_bytes,
            }),
            None => Err(CodecError::LimitExceeded {
                kind: CodecLimitKind::OutputBytes,
                observed: u64::MAX,
                limit: self.limits.max_output_bytes,
            }),
        }
    }

    fn record_written(&mut self, extra: usize) {
        self.written = self
            .written
            .saturating_add(u64::try_from(extra).unwrap_or(u64::MAX));
    }

    /// Finish the stream; all supported formats encode per scalar, so only
    /// the total byte count remains.
    ///
    /// # Errors
    ///
    /// Never fails for correctly pushed streams; kept for API symmetry with
    /// [`StreamingDecoder::finish`].
    pub fn finish(self) -> Result<u64, CodecError> {
        Ok(self.written)
    }

    /// Output bytes written so far.
    #[must_use]
    pub const fn written(&self) -> u64 {
        self.written
    }
}

/// Run `f` over slices of `text` of at most `max_bytes`, each ending on a
/// char boundary, without materializing the slice list.
fn for_each_char_boundary_chunk(
    text: &str,
    max_bytes: usize,
    f: &mut dyn FnMut(&str) -> Result<(), CodecError>,
) -> Result<(), CodecError> {
    let bytes = text.as_bytes();
    let mut start = 0usize;
    while start < bytes.len() {
        let mut end = bytes.len().min(start + max_bytes);
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            // A single scalar larger than max_bytes; take it whole.
            end = start
                + text[start..]
                    .chars()
                    .next()
                    .map_or(bytes.len() - start, char::len_utf8);
        }
        f(&text[start..end])?;
        start = end;
    }
    Ok(())
}

/// Streaming encode convenience: encode `text` in one push.
///
/// # Errors
///
/// Same as [`StreamingEncoder::push`].
pub fn encode_stream(
    text: &str,
    format: SourceFormat,
    limits: &EncodeLimits,
    cancel: &CancelToken,
) -> Result<Vec<u8>, CodecError> {
    let mut encoder = StreamingEncoder::new(format, *limits);
    let mut out = Vec::new();
    encoder.push(text, &mut out, cancel)?;
    let total = encoder.finish()?;
    debug_assert_eq!(u64::try_from(out.len()).unwrap_or(u64::MAX), total);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::encode;

    #[test]
    fn utf8_validator_matches_the_known_sequence_classes() {
        let cases: [(&[u8], Option<&str>); 9] = [
            (b"a", Some("a")),
            ("é".as_bytes(), Some("é")),
            ("€".as_bytes(), Some("€")),
            ("\u{1F600}".as_bytes(), Some("\u{1F600}")),
            (&[0xC0, 0x80], None),             // overlong
            (&[0xED, 0xA0, 0x80], None),       // surrogate
            (&[0xF4, 0x90, 0x80, 0x80], None), // > U+10FFFF
            (&[0x80], None),                   // lone continuation
            (&[0xC2], None),                   // truncated
        ];
        for (bytes, expected) in cases {
            let mut decoder = StreamingDecoder::new(DecodePolicy::Auto, DecodeLimits::default());
            let result = decoder
                .push(bytes, &CancelToken::new())
                .and_then(|()| decoder.finish());
            match expected {
                Some(text) => assert_eq!(result.expect("decodes").text(), text),
                None => assert!(result.is_err(), "bytes {bytes:?} must fail"),
            }
        }
    }

    #[test]
    fn utf16_pair_and_odd_trailing_byte() {
        let mut decoder = StreamingDecoder::new(DecodePolicy::Auto, DecodeLimits::default());
        // BOM + high surrogate D83D split across pushes + low surrogate DE00.
        decoder
            .push(&[0xFF, 0xFE, 0x3D, 0xD8], &CancelToken::new())
            .unwrap();
        decoder.push(&[0x00], &CancelToken::new()).unwrap();
        decoder.push(&[0xDE], &CancelToken::new()).unwrap();
        let outcome = decoder.finish().unwrap();
        assert_eq!(outcome.text(), "\u{1F600}");
        assert!(outcome.format().bom());

        let mut decoder = StreamingDecoder::new(DecodePolicy::Auto, DecodeLimits::default());
        decoder
            .push(&[0xFF, 0xFE, 0x41, 0x00, 0x42], &CancelToken::new())
            .unwrap();
        assert_eq!(
            decoder.finish().expect_err("odd trailing byte"),
            CodecError::OddUtf16ByteCount
        );
    }

    #[test]
    fn auto_waits_for_the_fourth_byte_before_committing_utf16le() {
        let mut decoder = StreamingDecoder::new(DecodePolicy::Auto, DecodeLimits::default());
        // FF FE 00 alone must not yet resolve as UTF-16LE...
        decoder
            .push(&[0xFF, 0xFE, 0x00], &CancelToken::new())
            .unwrap();
        // ...and FF FE 00 00 is the UTF-32LE signature, not UTF-16LE:
        // diagnosed as soon as the fourth byte arrives.
        let err = decoder
            .push(&[0x00], &CancelToken::new())
            .expect_err("utf-32le signature");
        assert_eq!(err, CodecError::UnsupportedOrAmbiguousBom);

        let mut decoder = StreamingDecoder::new(DecodePolicy::Auto, DecodeLimits::default());
        decoder
            .push(&[0xFF, 0xFE, 0x00], &CancelToken::new())
            .unwrap();
        decoder
            .push(&[0x05, 0x00, 0x00], &CancelToken::new())
            .unwrap();
        let outcome = decoder.finish().unwrap();
        assert_eq!(outcome.text(), "\u{0500}\u{0}");
        assert_eq!(outcome.format().encoding(), FileEncoding::Utf16Le);
    }

    #[test]
    fn encoder_streams_the_same_bytes_as_one_shot() {
        let text = "héllo\r\n\u{1F600}\r\nwörld";
        let format = SourceFormat::new(FileEncoding::Utf16Le, true).unwrap();
        let limits = EncodeLimits::default();
        let one_shot = encode(text, format, &limits, &CancelToken::new()).expect("one-shot");
        let streamed = encode_stream(text, format, &limits, &CancelToken::new()).expect("streamed");
        assert_eq!(streamed, one_shot);
        assert_eq!(streamed.len() % 2, 0);
    }

    #[test]
    fn streaming_encoder_enforces_the_output_budget() {
        let format = SourceFormat::new(FileEncoding::Utf8, false).unwrap();
        let limits = EncodeLimits {
            max_input_utf8_bytes: u64::MAX,
            max_output_bytes: 2,
        };
        let mut encoder = StreamingEncoder::new(format, limits);
        let mut out = Vec::new();
        encoder.push("ab", &mut out, &CancelToken::new()).unwrap();
        let err = encoder
            .push("cdef", &mut out, &CancelToken::new())
            .expect_err("over output budget");
        assert!(matches!(err, CodecError::LimitExceeded { .. }));
        assert_eq!(out.len(), 2, "only admitted bytes were written");
        assert_eq!(encoder.written(), 2);
    }

    #[test]
    fn chunk_at_char_boundary_respects_scalars() {
        let text = "a\u{1F600}b";
        let mut joined = String::new();
        let mut offset = 0usize;
        for_each_char_boundary_chunk(text, 2, &mut |slice| {
            joined.push_str(slice);
            assert!(text.is_char_boundary(offset));
            offset += slice.len();
            assert!(text.is_char_boundary(offset));
            Ok(())
        })
        .expect("chunks collect");
        assert_eq!(joined, text);
        assert_eq!(offset, text.len());
    }

    #[test]
    fn explicit_bom_only_and_bom_conflict_at_finish() {
        // Exactly the required signature and nothing else: empty content.
        let policy = DecodePolicy::Explicit(SourceFormat::new(FileEncoding::Utf8, true).unwrap());
        let mut decoder = StreamingDecoder::new(policy, DecodeLimits::default());
        decoder
            .push(&[0xEF, 0xBB, 0xBF], &CancelToken::new())
            .unwrap();
        let outcome = decoder.finish().unwrap();
        assert_eq!(outcome.text(), "");
        assert!(outcome.format().bom());

        // Truncated signature at finish is a conflict, not a guess.
        let mut decoder = StreamingDecoder::new(policy, DecodeLimits::default());
        decoder.push(&[0xEF, 0xBB], &CancelToken::new()).unwrap();
        assert_eq!(
            decoder.finish().expect_err("truncated signature"),
            CodecError::BomConflict
        );
    }
}
