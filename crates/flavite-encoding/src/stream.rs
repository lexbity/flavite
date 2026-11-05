//! Incremental (streaming) codec state machines.
//!
//! The streaming decoder handles partial BOM signatures, partial UTF-16 code
//! units, split surrogate pairs and split UTF-8 sequences across chunk
//! boundaries, and reports incomplete suffixes only at explicit `finish()` —
//! never as a lossy substitution (RFC-0006 §6.10). `decode` in the parent
//! module feeds whole buffers through this same state machine in 64 KiB
//! pieces so cancellation is always honored at that granularity (NFR-19).

use crate::format::{
    CodecError, CodecLimitKind, DecodeLimits, DecodePolicy, EncodeLimits, FileEncoding,
    SourceFormat,
};
use crate::map::{Checkpoint, MAX_CHECKPOINT_SCALAR_GAP};

/// Progress granularity: cancellation and limit checks happen at least this
/// often while streaming bytes (NFR-19).
pub(crate) const CHECK_INTERVAL: u64 = 64 * 1024;

/// Result metadata of a completed [`Decoder::finish`].
#[derive(Debug, Clone)]
pub struct DecodedMeta {
    /// The descriptor that was applied (detected under `Auto`).
    pub format: SourceFormat,
    /// Bytes of BOM signature removed from the source (0, 2 or 3).
    pub bom_len: u64,
    /// Total raw bytes fed.
    pub source_len: u64,
    /// Total decoded UTF-8 bytes produced.
    pub decoded_len: u64,
    /// Sparse scalar-boundary checkpoints for UTF-16 sources; empty for
    /// ASCII/UTF-8 (whose mapping is the BOM offset).
    pub checkpoints: Vec<Checkpoint>,
}

/// One streaming decode session.
///
/// The policy is fixed at construction; chunks may arrive at any byte
/// boundary. Output is appended to a `String` sink, which therefore always
/// holds valid UTF-8 with original CR/LF scalars preserved (no newline
/// folding here — that is `flavite-syntax`'s EOL map).
#[derive(Debug)]
pub struct Decoder {
    policy: DecodePolicy,
    limits: DecodeLimits,
    /// Resolved encoding family after BOM sniff (pre-resolution: provisional).
    encoding: FileEncoding,
    /// Number of signature bytes consumed as BOM at the start of the stream.
    bom_len: u64,
    resolved: bool,
    prefix: Vec<u8>,
    prefix_target: usize,
    /// Raw bytes accepted so far (absolute file positions).
    next_raw: u64,
    /// UTF-8 partial sequence buffer (≤3 bytes).
    utf8_pending: Vec<u8>,
    utf8_pending_start: u64,
    /// UTF-16 partial code unit (1 byte seen).
    unit_pending: Option<u8>,
    unit_start: u64,
    /// UTF-16 pending high surrogate awaiting its low unit.
    high_pending: Option<(u16, u64)>,
    decoded_len: u64,
    next_scalar_source: u64,
    scalars_since_cp: usize,
    checkpoints: Vec<Checkpoint>,
}

impl Decoder {
    /// Start a decode session for one raw source stream under `policy`.
    #[must_use]
    pub fn new(policy: DecodePolicy, limits: DecodeLimits) -> Self {
        let (encoding, prefix_target) = match policy {
            DecodePolicy::Auto => (FileEncoding::Utf8, 4),
            DecodePolicy::Explicit(f) => (f.encoding(), if f.bom() { f.bom_len() } else { 0 }),
        };
        let mut decoder = Self {
            policy,
            limits,
            encoding,
            bom_len: 0,
            resolved: matches!(policy, DecodePolicy::Explicit(_)) && prefix_target == 0,
            prefix: Vec::with_capacity(4),
            prefix_target,
            next_raw: 0,
            utf8_pending: Vec::new(),
            utf8_pending_start: 0,
            unit_pending: None,
            unit_start: 0,
            high_pending: None,
            decoded_len: 0,
            next_scalar_source: 0,
            scalars_since_cp: 0,
            checkpoints: Vec::new(),
        };
        // An explicit no-BOM UTF-16 policy resolves at construction: seed the
        // checkpoint table the same way `resolve_prefix` does for the sniffing
        // paths.
        if decoder.resolved && encoding.is_utf16() {
            decoder.checkpoints.push(Checkpoint {
                decoded_utf8: 0,
                source: 0,
            });
            decoder.next_scalar_source = 0;
        }
        decoder
    }

    /// The encoding family once resolved; provisional value before.
    #[must_use]
    pub const fn resolved_encoding(&self) -> Option<FileEncoding> {
        if self.resolved {
            Some(self.encoding)
        } else {
            None
        }
    }

    /// Feed one raw chunk, appending decoded UTF-8 to `sink`.
    ///
    /// # Errors
    ///
    /// Returns a typed [`CodecError`] on malformed input, BOM conflict,
    /// limit exceedance or cancellation. On error the session must be
    /// discarded; partial sink contents carry no authority.
    pub fn feed(&mut self, chunk: &[u8], sink: &mut String) -> Result<(), CodecError> {
        let mut cursor = 0usize;
        if !self.resolved {
            let need = self.prefix_target - self.prefix.len();
            if chunk.len() >= need {
                self.prefix.extend_from_slice(&chunk[..need]);
                cursor = need;
                self.next_raw += need as u64;
                let body_start = self.resolve_prefix(sink)?;
                // `resolve_prefix` consumed the signature and may have left
                // remaining prefix bytes for body processing.
                let carried = std::mem::take(&mut self.prefix);
                if !carried[body_start..].is_empty() {
                    let base = self.next_raw - carried.len() as u64 + body_start as u64;
                    self.feed_body(&carried[body_start..], base, sink)?;
                }
            } else {
                self.prefix.extend_from_slice(chunk);
                self.next_raw += chunk.len() as u64;
                return Ok(());
            }
        }
        if cursor < chunk.len() {
            let base = self.next_raw;
            self.feed_body(&chunk[cursor..], base, sink)?;
            self.next_raw += (chunk.len() - cursor) as u64;
        }
        self.check_progress()?;
        Ok(())
    }

    /// Complete the stream.
    ///
    /// # Errors
    ///
    /// A truncated sequence at explicit end-of-input produces the matching
    /// typed error (`InvalidUtf8`, `OddUtf16ByteCount`, `InvalidUtf16`,
    /// `BomConflict` for an unterminated required signature, or
    /// `UnsupportedOrAmbiguousBom`); never a U+FFFD replacement.
    pub fn finish(&mut self, sink: &mut String) -> Result<DecodedMeta, CodecError> {
        if !self.resolved {
            let body_start = self.resolve_prefix(sink)?;
            let carried = std::mem::take(&mut self.prefix);
            if !carried[body_start..].is_empty() {
                let base = self.next_raw - carried.len() as u64 + body_start as u64;
                self.feed_body(&carried[body_start..], base, sink)?;
            }
        }
        // Flush residual partial state as EOF errors.
        match self.encoding {
            FileEncoding::Utf8 | FileEncoding::Ascii => {
                if !self.utf8_pending.is_empty() {
                    return Err(CodecError::InvalidUtf8 {
                        source_byte: self.utf8_pending_start,
                    });
                }
            }
            FileEncoding::Utf16Le | FileEncoding::Utf16Be => {
                if let Some((_, pos)) = self.high_pending {
                    return Err(CodecError::InvalidUtf16 { source_byte: pos });
                }
                if self.unit_pending.is_some() {
                    return Err(CodecError::OddUtf16ByteCount);
                }
            }
        }
        let format = match self.policy {
            DecodePolicy::Explicit(f) => f,
            DecodePolicy::Auto => SourceFormat::new(self.encoding, self.bom_len > 0)
                .map_err(|_| CodecError::BomConflict)?,
        };
        Ok(DecodedMeta {
            format,
            bom_len: self.bom_len,
            source_len: self.next_raw,
            decoded_len: self.decoded_len,
            checkpoints: std::mem::take(&mut self.checkpoints),
        })
    }

    /// Resolve the BOM signature from the collected prefix. Returns the index
    /// within `self.prefix` where body bytes begin.
    fn resolve_prefix(&mut self, _sink: &mut String) -> Result<usize, CodecError> {
        let bytes = &self.prefix;
        let (encoding, bom_len) = match self.policy {
            DecodePolicy::Auto => {
                if bytes.len() >= 4 {
                    let four: [u8; 4] = [bytes[0], bytes[1], bytes[2], bytes[3]];
                    // Longer unsupported signatures first (UTF-32 LE/BE).
                    if four == [0xFF, 0xFE, 0x00, 0x00] || four == [0x00, 0x00, 0xFE, 0xFF] {
                        return Err(CodecError::UnsupportedOrAmbiguousBom);
                    }
                }
                if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
                    (FileEncoding::Utf8, 3usize)
                } else if bytes.starts_with(&[0xFF, 0xFE]) {
                    (FileEncoding::Utf16Le, 2)
                } else if bytes.starts_with(&[0xFE, 0xFF]) {
                    (FileEncoding::Utf16Be, 2)
                } else {
                    // ASCII-visible bytes are valid UTF-8; Auto never marks a
                    // strict-ASCII policy and never guesses UTF-16 (§6.10).
                    (FileEncoding::Utf8, 0)
                }
            }
            DecodePolicy::Explicit(f) => {
                if f.bom() {
                    let sig = f.encoding().bom_signature();
                    if bytes.len() < sig.len() {
                        if sig.starts_with(bytes) {
                            // Incomplete signature at EOF: required BOM is
                            // absent as a full signature.
                            return Err(CodecError::BomConflict);
                        }
                        return Err(CodecError::BomConflict);
                    }
                    if bytes[..sig.len()] != *sig {
                        return Err(CodecError::BomConflict);
                    }
                    (f.encoding(), sig.len())
                } else {
                    // No-BOM explicit: never strips signature-shaped bytes;
                    // a leading U+FEFF is content.
                    (f.encoding(), 0)
                }
            }
        };
        self.encoding = encoding;
        self.bom_len = bom_len as u64;
        self.resolved = true;
        if encoding.is_utf16() {
            // First scalar starts right after the (possibly zero-length) BOM.
            self.checkpoints.push(Checkpoint {
                decoded_utf8: 0,
                source: self.bom_len as u32,
            });
            self.next_scalar_source = self.bom_len;
        }
        Ok(bom_len)
    }

    fn feed_body(&mut self, data: &[u8], base: u64, sink: &mut String) -> Result<(), CodecError> {
        match self.encoding {
            FileEncoding::Utf8 => self.feed_utf8(data, base, sink),
            FileEncoding::Ascii => self.feed_ascii(data, base, sink),
            FileEncoding::Utf16Le | FileEncoding::Utf16Be => self.feed_utf16(data, base, sink),
        }
    }

    fn feed_utf8(&mut self, data: &[u8], base: u64, sink: &mut String) -> Result<(), CodecError> {
        if self.utf8_pending.is_empty() {
            // Fast path: whole-chunk strict validation.
            match std::str::from_utf8(data) {
                Ok(s) => {
                    sink.push_str(s);
                    self.decoded_len += s.len() as u64;
                    return Ok(());
                }
                Err(err) => {
                    let valid = err.valid_up_to();
                    if err.error_len().is_some() {
                        return Err(CodecError::InvalidUtf8 {
                            source_byte: base + valid as u64,
                        });
                    }
                    if valid > 0 {
                        let head = std::str::from_utf8(&data[..valid])
                            .map_err(|_| CodecError::InvalidUtf8 { source_byte: base })?;
                        sink.push_str(head);
                        self.decoded_len += head.len() as u64;
                    }
                    self.utf8_pending_start = base + valid as u64;
                    self.utf8_pending.extend_from_slice(&data[valid..]);
                    return Ok(());
                }
            }
        }
        // Continuation path: prepend the pending partial sequence.
        let mut buf = std::mem::take(&mut self.utf8_pending);
        buf.extend_from_slice(data);
        let merged_start = self.utf8_pending_start;
        match std::str::from_utf8(&buf) {
            Ok(s) => {
                sink.push_str(s);
                self.decoded_len += s.len() as u64;
                self.utf8_pending_start = base + data.len() as u64;
                Ok(())
            }
            Err(err) => {
                let valid = err.valid_up_to();
                if err.error_len().is_some() {
                    return Err(CodecError::InvalidUtf8 {
                        source_byte: merged_start + valid as u64,
                    });
                }
                if valid > 0 {
                    let head = std::str::from_utf8(&buf[..valid]).map_err(|_| {
                        CodecError::InvalidUtf8 {
                            source_byte: merged_start,
                        }
                    })?;
                    sink.push_str(head);
                    self.decoded_len += head.len() as u64;
                }
                self.utf8_pending_start = merged_start + valid as u64;
                self.utf8_pending.extend_from_slice(&buf[valid..]);
                Ok(())
            }
        }
    }

    fn feed_ascii(&mut self, data: &[u8], base: u64, sink: &mut String) -> Result<(), CodecError> {
        if let Some(idx) = data.iter().position(|b| *b >= 0x80) {
            return Err(CodecError::AsciiViolation {
                source_byte: base + idx as u64,
            });
        }
        let s =
            std::str::from_utf8(data).map_err(|_| CodecError::InvalidUtf8 { source_byte: base })?;
        sink.push_str(s);
        self.decoded_len += s.len() as u64;
        Ok(())
    }

    fn feed_utf16(&mut self, data: &[u8], base: u64, sink: &mut String) -> Result<(), CodecError> {
        let le = self.encoding == FileEncoding::Utf16Le;
        let mut i = 0usize;
        while i < data.len() {
            let abs = base + i as u64;
            if let Some(hi_byte) = self.unit_pending.take() {
                // Combine the parked leading byte with the current byte; the
                // byte order in memory is (earlier, later) for both families.
                let bytes = [hi_byte, data[i]];
                let unit = if le {
                    u16::from_le_bytes(bytes)
                } else {
                    u16::from_be_bytes(bytes)
                };
                let start = self.unit_start;
                i += 1;
                self.process_unit(unit, start, sink)?;
            } else if i + 1 < data.len() {
                let bytes = [data[i], data[i + 1]];
                let unit = if le {
                    u16::from_le_bytes(bytes)
                } else {
                    u16::from_be_bytes(bytes)
                };
                let start = abs;
                i += 2;
                self.process_unit(unit, start, sink)?;
            } else {
                // Trailing half unit: park it; EOF decides OddUtf16ByteCount.
                self.unit_pending = Some(data[i]);
                self.unit_start = abs;
                i += 1;
            }
        }
        Ok(())
    }

    fn process_unit(&mut self, unit: u16, start: u64, sink: &mut String) -> Result<(), CodecError> {
        if (0xD800..0xDC00).contains(&unit) {
            if let Some((_, pos)) = self.high_pending.take() {
                // A high surrogate not followed by a low one is invalid at the
                // *first* unit.
                return Err(CodecError::InvalidUtf16 { source_byte: pos });
            }
            self.high_pending = Some((unit, start));
            return Ok(());
        }
        let ch = if (0xDC00..0xE000).contains(&unit) {
            if let Some((high, high_pos)) = self.high_pending.take() {
                // Combine the surrogate pair into one supplementary scalar.
                let value =
                    0x1_0000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00);
                char::from_u32(value).ok_or(CodecError::InvalidUtf16 {
                    source_byte: high_pos,
                })?
            } else {
                return Err(CodecError::InvalidUtf16 { source_byte: start });
            }
        } else {
            if let Some((_, high_pos)) = self.high_pending.take() {
                return Err(CodecError::InvalidUtf16 {
                    source_byte: high_pos,
                });
            }
            char::from_u32(u32::from(unit))
                .ok_or(CodecError::InvalidUtf16 { source_byte: start })?
        };
        sink.push(ch);
        self.decoded_len += ch.len_utf8() as u64;
        if self.decoded_len > self.limits.max_decoded_utf8_bytes {
            return Err(CodecError::limit(
                CodecLimitKind::DecodedUtf8,
                self.decoded_len,
                self.limits.max_decoded_utf8_bytes,
            ));
        }
        // The scalar just completed ends two bytes after the unit that closed
        // it (the low unit of a pair, or the sole BMP unit).
        self.next_scalar_source = start + 2;
        self.scalars_since_cp += 1;
        if self.scalars_since_cp >= MAX_CHECKPOINT_SCALAR_GAP {
            self.scalars_since_cp = 0;
            let decoded_u32 = u32::try_from(self.decoded_len).map_err(|_| {
                CodecError::limit(
                    CodecLimitKind::DecodedUtf8,
                    self.decoded_len,
                    u64::from(u32::MAX),
                )
            })?;
            let source_u32 = u32::try_from(self.next_scalar_source).map_err(|_| {
                CodecError::limit(
                    CodecLimitKind::RawInput,
                    self.next_scalar_source,
                    u64::from(u32::MAX),
                )
            })?;
            self.checkpoints.push(Checkpoint {
                decoded_utf8: decoded_u32,
                source: source_u32,
            });
        }
        Ok(())
    }

    fn check_progress(&mut self) -> Result<(), CodecError> {
        if self.decoded_len > self.limits.max_decoded_utf8_bytes {
            return Err(CodecError::limit(
                CodecLimitKind::DecodedUtf8,
                self.decoded_len,
                self.limits.max_decoded_utf8_bytes,
            ));
        }
        Ok(())
    }
}

/// One streaming encode session: UTF-8 in, strict bytes out, under an exact
/// descriptor. Output size is enforced as it is produced; use
/// [`crate::encode`] to validate the full output bound before any write.
#[derive(Debug)]
pub struct Encoder {
    format: SourceFormat,
    limits: EncodeLimits,
    written: u64,
    input: u64,
}

impl Encoder {
    /// Start an encode session for `format`.
    #[must_use]
    pub const fn new(format: SourceFormat, limits: EncodeLimits) -> Self {
        Self {
            format,
            limits,
            written: 0,
            input: 0,
        }
    }

    /// The BOM bytes to emit first for this descriptor (possibly empty).
    #[must_use]
    pub fn header(&self) -> &'static [u8] {
        self.format.bom_bytes()
    }

    /// Encode one UTF-8 chunk, appending bytes to `out`.
    ///
    /// # Errors
    ///
    /// `Unencodable` for strict-ASCII violations (before the chunk is
    /// emitted), `LimitExceeded` for input/output bound violations.
    pub fn encode(&mut self, chunk: &str, out: &mut Vec<u8>) -> Result<(), CodecError> {
        self.input = self
            .input
            .checked_add(chunk.len() as u64)
            .ok_or(CodecError::limit(
                CodecLimitKind::EncodeInput,
                self.input.saturating_add(chunk.len() as u64),
                self.limits.max_input_utf8_bytes,
            ))?;
        if self.input > self.limits.max_input_utf8_bytes {
            return Err(CodecError::limit(
                CodecLimitKind::EncodeInput,
                self.input,
                self.limits.max_input_utf8_bytes,
            ));
        }
        match self.format.encoding() {
            FileEncoding::Ascii => {
                if let Some((byte_idx, _)) = chunk.char_indices().find(|(_, c)| !c.is_ascii()) {
                    return Err(CodecError::Unencodable {
                        text_byte: self.input - chunk.len() as u64 + byte_idx as u64,
                    });
                }
                out.extend_from_slice(chunk.as_bytes());
                self.written = self
                    .written
                    .checked_add(chunk.len() as u64)
                    .ok_or_else(|| {
                        CodecError::limit(
                            CodecLimitKind::EncodeOutput,
                            self.written.saturating_add(chunk.len() as u64),
                            self.limits.max_output_bytes,
                        )
                    })?;
            }
            FileEncoding::Utf8 => {
                out.extend_from_slice(chunk.as_bytes());
                self.written = self
                    .written
                    .checked_add(chunk.len() as u64)
                    .ok_or_else(|| {
                        CodecError::limit(
                            CodecLimitKind::EncodeOutput,
                            self.written.saturating_add(chunk.len() as u64),
                            self.limits.max_output_bytes,
                        )
                    })?;
            }
            FileEncoding::Utf16Le | FileEncoding::Utf16Be => {
                let le = self.format.encoding() == FileEncoding::Utf16Le;
                let mut buf = [0u16; 2];
                for c in chunk.chars() {
                    let count = c.encode_utf16(&mut buf).len();
                    for u in &buf[..count] {
                        let bytes = if le { u.to_le_bytes() } else { u.to_be_bytes() };
                        out.extend_from_slice(&bytes);
                    }
                    self.written =
                        self.written
                            .checked_add((count * 2) as u64)
                            .ok_or(CodecError::limit(
                                CodecLimitKind::EncodeOutput,
                                u64::MAX,
                                self.limits.max_output_bytes,
                            ))?;
                    if self.written > self.limits.max_output_bytes {
                        return Err(CodecError::limit(
                            CodecLimitKind::EncodeOutput,
                            self.written,
                            self.limits.max_output_bytes,
                        ));
                    }
                }
            }
        }
        if self.written > self.limits.max_output_bytes {
            return Err(CodecError::limit(
                CodecLimitKind::EncodeOutput,
                self.written,
                self.limits.max_output_bytes,
            ));
        }
        Ok(())
    }

    /// Total bytes produced so far.
    #[must_use]
    pub const fn written(&self) -> u64 {
        self.written
    }

    /// Finish the stream and return the exact byte count.
    #[must_use]
    pub const fn finish(&self) -> u64 {
        self.written
    }
}
