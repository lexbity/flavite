//! Bounded framed IPC protocol for the isolated source-matching helper
//! (RFC-0006 §6.2/D-14, S03).
//!
//! The helper is a pure matcher process: the parent streams one declared
//! source, the helper streams back match/capture/expansion pages. Frames
//! are length-prefixed with a hard [`FRAME_MAX_BYTES`] bound — a decoder
//! rejecting a frame never allocates past the bound, so protocol fuzzing
//! can neither truncate nor oversize its way into memory (AC-17/AC-20).
//!
//! The request enum intentionally supports ONLY matching/template work and
//! cancellation: cursor movement, Markdown commands, edits and file
//! operations are protocol kinds that do not exist here, and a helper that
//! received one could only reject it as an unknown tag. The parent owns
//! time/memory enforcement; the helper additionally enforces the declared
//! input/output totals it was told about.
//!
//! Handshake binds the protocol version to the pinned engine semantics
//! ([`MATCH_SEMANTICS_VERSION`]); a mismatch is a typed refusal, never a
//! fallback to a different engine.

use std::io::{Read, Write};

use crate::matcher::{MATCH_SEMANTICS_VERSION, MatchMode};

/// Version of this wire protocol; bump on any framing/message change.
/// Version 3 adds a match-ordinal continuation offset to S03 pages; older
/// helpers are a typed protocol refusal, never a reinterpretation.
/// Version 2 added the S03 request-kind, frozen selection scope, report cap
/// and per-request output bound to `ToHelper::Begin`; a version-1 helper
/// is a typed protocol refusal, never a reinterpretation of old frames.
pub const PROTOCOL_VERSION: u32 = 3;

/// Hard frame bound: length prefix excluded, payload never exceeds this.
/// Parent frames and helper frames obey the same bound (§6.2).
pub const FRAME_MAX_BYTES: usize = 64 * 1024;

/// Declared source total bound for one helper request (NFR-8/NFR-19).
pub const SOURCE_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Matches streamed per page frame; pages keep every frame within the
/// frame bound even with maximal expansions.
pub const MATCHES_PER_PAGE: usize = 128;

/// Engine identity bound at handshake: the pinned matching semantics this
/// crate implements. The worker crate is built from this same crate, so a
/// helper reporting a different id is a foreign engine (typed refusal).
#[must_use]
pub const fn engine_version() -> u32 {
    MATCH_SEMANTICS_VERSION
}

/// Wire match entry: spans in `TextByte` coordinates over the streamed
/// source, capture groups included, plus the per-match expansion bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireMatch {
    /// Whole-match span (capture 0), half-open.
    pub start: u64,
    pub end: u64,
    /// Capture groups after group 0; `None` = group did not participate.
    pub groups: Vec<Option<(u64, u64)>>,
    /// Expansion of the replacement template for this match.
    pub expansion: Vec<u8>,
}

fn put_wire_match(out: &mut Vec<u8>, hit: &WireMatch) -> Result<(), FrameError> {
    put_u64(out, hit.start);
    put_u64(out, hit.end);
    put_u32(
        out,
        u32::try_from(hit.groups.len()).map_err(|_| FrameError::Oversize {
            observed: hit.groups.len(),
        })?,
    );
    for group in &hit.groups {
        match group {
            Some((start, end)) => {
                put_u8(out, 1);
                put_u64(out, *start);
                put_u64(out, *end);
            }
            None => put_u8(out, 0),
        }
    }
    put_bytes(out, &hit.expansion);
    Ok(())
}

fn read_wire_match(reader: &mut Reader<'_>) -> Result<WireMatch, FrameError> {
    let start = reader.u64()?;
    let end = reader.u64()?;
    let group_count = reader.u32()? as usize;
    if group_count > crate::matcher::REGEX_MAX_GROUPS {
        return Err(FrameError::Malformed);
    }
    let mut groups = Vec::with_capacity(group_count);
    for _ in 0..group_count {
        match reader.u8()? {
            0 => groups.push(None),
            1 => {
                let start = reader.u64()?;
                groups.push(Some((start, reader.u64()?)));
            }
            _ => return Err(FrameError::Malformed),
        }
    }
    Ok(WireMatch {
        start,
        end,
        groups,
        expansion: reader.bytes()?.to_vec(),
    })
}

/// Why a helper refused or failed one request. Codes are stable and
/// redacted: no pattern, source or expansion text ever travels back
/// (§6.7 — default logs and IPC carry typed codes only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCode {
    /// Helper handshake/protocol state is unusable.
    Protocol,
    /// The request exceeded a declared bound.
    Limit,
    /// The matcher refused the pattern/options (empty query, oversize,
    /// invalid regex) — mirrors [`crate::matcher::MatchError`] kinds.
    Pattern,
    /// The template was refused before any expansion.
    Template,
    /// The declared source totals were violated.
    Source,
    /// The request was cancelled by the parent.
    Cancelled,
}

/// What kind of work one helper request performs (§6.2: the helper is a
/// pure matcher — matching only, with or without template expansion).
///
/// [`RequestKind::Find`] streams span pages with empty expansions and
/// requires no `Template` frame; [`RequestKind::Replace`] additionally
/// expands the declared template for every reported match and checks the
/// declared output bound before any page ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// Report match spans (expansions empty); exact total still counted.
    Find,
    /// Report match spans with per-match template expansions (replacement).
    Replace,
}

impl FailureCode {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Protocol => "protocol",
            Self::Limit => "limit",
            Self::Pattern => "pattern",
            Self::Template => "template",
            Self::Source => "source",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Parent → helper messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToHelper {
    /// Initial handshake; the helper replies once.
    Handshake {
        protocol_version: u32,
        engine_version: u32,
    },
    /// Begin one match request. The pattern rides here (≤ 8 KiB); with
    /// [`RequestKind::Replace`] the template (≤ 64 KiB) arrives in its own
    /// frame next. `scope` is the frozen selection range: matching happens
    /// against the FULL streamed source (so whole-word boundaries use the
    /// whole-text semantics, D-05) but only matches fully contained in the
    /// scope are reported, never clipped. `report_matches` caps the matches
    /// that ship in pages while the exact total is still counted; `0` means
    /// "report every match". `max_output` bounds the expanded replacement
    /// output before anything ships (checked arithmetic, §6.2).
    Begin {
        request_id: u64,
        kind: RequestKind,
        mode: MatchMode,
        case_sensitive: bool,
        whole_word: bool,
        max_matches: u64,
        report_matches: u64,
        skip_matches: u64,
        max_output: u64,
        scope: Option<(u64, u64)>,
        source_total: u64,
        pattern: Vec<u8>,
    },
    /// The replacement template for the request (literal or dollar).
    Template { request_id: u64, template: Vec<u8> },
    /// One source chunk at the given decoded-byte offset.
    SourceChunk {
        request_id: u64,
        offset: u64,
        bytes: Vec<u8>,
    },
    /// End of source; the helper finishes matching and streams pages.
    EndOfSource { request_id: u64 },
    /// Cancel an in-flight request; the helper stops at the next boundary.
    Cancel { request_id: u64 },
}

/// Helper → parent messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromHelper {
    /// Handshake result; `Err` carries a stable reason code.
    HandshakeReply(Result<(u32, u32), FailureCode>),
    /// The request was admitted and the helper is streaming replies.
    /// Distinct from "the pattern compiled": pattern/template refusals
    /// arrive as typed `Failed` frames after this point, so admission
    /// alone is never a completed match.
    Accepted { request_id: u64 },
    /// One page of matches (ascending, non-overlapping).
    Page {
        request_id: u64,
        matches: Vec<WireMatch>,
    },
    /// Start one replacement result. The expansion follows in bounded
    /// `ExpansionChunk` frames, allowing a single expansion up to the note
    /// output limit without violating the 64 KiB frame bound.
    MatchStart {
        request_id: u64,
        index: u64,
        matched: WireMatch,
        expansion_len: u64,
    },
    /// Contiguous chunk of one match expansion. The receiver checks the
    /// ordinal and offset before appending and publishes only at Done.
    ExpansionChunk {
        request_id: u64,
        index: u64,
        offset: u64,
        bytes: Vec<u8>,
    },
    /// Terminal proof for one request: exact total and checked output
    /// length (the sum the parent must reproduce).
    Done {
        request_id: u64,
        total_matches: u64,
        output_len: u64,
    },
    /// Typed refusal/failure; no note text.
    Failed {
        request_id: u64,
        failure: FailureCode,
    },
}

/// One framed encode error: oversized payloads are refused before any
/// write, so an encoder can never emit a frame the peer must reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// The payload exceeded [`FRAME_MAX_BYTES`].
    Oversize { observed: usize },
    /// The peer's frame length prefix exceeded the bound.
    PeerOversize { observed: u64 },
    /// The stream ended mid-frame or mid-message.
    Truncated,
    /// The bytes did not decode as a known message.
    Malformed,
}

/// Encode one message as a length-prefixed frame.
///
/// # Errors
///
/// [`FrameError::Oversize`] when the encoded payload exceeds the frame
/// bound (checked before any write).
pub fn encode(message: &ToHelper) -> Result<Vec<u8>, FrameError> {
    finish_frame(&encode_to_helper(message))
}

/// Encode one helper → parent message.
///
/// # Errors
///
/// [`FrameError::Oversize`] when the encoded payload exceeds the frame
/// bound (checked before any write).
pub fn encode_reply(message: &FromHelper) -> Result<Vec<u8>, FrameError> {
    finish_frame(&encode_from_helper(message)?)
}

fn finish_frame(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let len = payload.len();
    if len > FRAME_MAX_BYTES {
        return Err(FrameError::Oversize { observed: len });
    }
    let mut frame = Vec::with_capacity(4 + len);
    // The bound above guarantees the conversion; the workspace targets are
    // 64-bit, but stay explicit rather than casting silently.
    let Ok(len) = u32::try_from(len) else {
        return Err(FrameError::Oversize { observed: len });
    };
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Decode one message from `bytes` (the frame payload without the length
/// prefix). Bounds are checked before allocation-heavy parsing.
///
/// # Errors
///
/// [`FrameError::Malformed`] for unknown tags or invalid fields.
pub fn decode(bytes: &[u8]) -> Result<ToHelper, FrameError> {
    decode_to_helper(bytes)
}

/// Decode one helper → parent frame payload.
///
/// # Errors
///
/// [`FrameError::Malformed`] for unknown tags or invalid fields.
pub fn decode_reply(bytes: &[u8]) -> Result<FromHelper, FrameError> {
    decode_from_helper(bytes)
}

/// Read exactly one frame from `reader` and decode it (parent side).
///
/// # Errors
///
/// [`FrameError::PeerOversize`] for a length prefix past the bound;
/// [`FrameError::Truncated`]/[`FrameError::Malformed`] accordingly.
pub fn read_reply(reader: &mut dyn Read) -> Result<FromHelper, FrameError> {
    let payload = read_frame(reader)?;
    decode_reply(&payload)
}

/// Read exactly one frame from `reader` and decode it (helper side).
///
/// # Errors
///
/// Same bounds as [`read_reply`].
pub fn read_request(reader: &mut dyn Read) -> Result<ToHelper, FrameError> {
    let payload = read_frame(reader)?;
    decode(&payload)
}

fn read_frame(reader: &mut dyn Read) -> Result<Vec<u8>, FrameError> {
    let mut prefix = [0u8; 4];
    reader
        .read_exact(&mut prefix)
        .map_err(|_| FrameError::Truncated)?;
    let len = u32::from_le_bytes(prefix);
    if u64::from(len) > FRAME_MAX_BYTES as u64 {
        return Err(FrameError::PeerOversize {
            observed: u64::from(len),
        });
    }
    let mut payload = vec![0u8; len as usize];
    reader
        .read_exact(&mut payload)
        .map_err(|_| FrameError::Truncated)?;
    Ok(payload)
}

/// Write one frame.
///
/// # Errors
///
/// Propagates [`std::io::Error`] from the stream.
pub fn write_frame(writer: &mut dyn Write, frame: &[u8]) -> std::io::Result<()> {
    writer.write_all(frame)
}

// --- wire encoding -------------------------------------------------------

fn put_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u64(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], FrameError> {
        if self.bytes.len() < len {
            return Err(FrameError::Malformed);
        }
        let (head, tail) = self.bytes.split_at(len);
        self.bytes = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, FrameError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, FrameError> {
        let raw = self.take(4)?;
        Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    fn u64(&mut self) -> Result<u64, FrameError> {
        let raw = self.take(8)?;
        let mut array = [0u8; 8];
        array.copy_from_slice(raw);
        Ok(u64::from_le_bytes(array))
    }

    fn bytes(&mut self) -> Result<&'a [u8], FrameError> {
        let len = self.u64()?;
        if len > FRAME_MAX_BYTES as u64 {
            return Err(FrameError::Malformed);
        }
        let Ok(len) = usize::try_from(len) else {
            return Err(FrameError::Malformed);
        };
        self.take(len)
    }
}

const TAG_HANDSHAKE: u8 = 0x01;
const TAG_BEGIN: u8 = 0x02;
const TAG_TEMPLATE: u8 = 0x03;
const TAG_SOURCE_CHUNK: u8 = 0x04;
const TAG_END_OF_SOURCE: u8 = 0x05;
const TAG_CANCEL: u8 = 0x06;

const TAG_HANDSHAKE_REPLY: u8 = 0x81;
const TAG_ACCEPTED: u8 = 0x82;
const TAG_PAGE: u8 = 0x83;
const TAG_DONE: u8 = 0x84;
const TAG_FAILED: u8 = 0x85;
const TAG_MATCH_START: u8 = 0x86;
const TAG_EXPANSION_CHUNK: u8 = 0x87;

fn encode_to_helper(message: &ToHelper) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    match message {
        ToHelper::Handshake {
            protocol_version,
            engine_version,
        } => {
            put_u8(&mut out, TAG_HANDSHAKE);
            put_u32(&mut out, *protocol_version);
            put_u32(&mut out, *engine_version);
        }
        ToHelper::Begin {
            request_id,
            kind,
            mode,
            case_sensitive,
            whole_word,
            max_matches,
            report_matches,
            skip_matches,
            max_output,
            scope,
            source_total,
            pattern,
        } => {
            put_u8(&mut out, TAG_BEGIN);
            put_u64(&mut out, *request_id);
            put_u8(
                &mut out,
                match kind {
                    RequestKind::Find => 0,
                    RequestKind::Replace => 1,
                },
            );
            put_u8(&mut out, u8::from(matches!(mode, MatchMode::Regex)));
            put_u8(&mut out, u8::from(*case_sensitive));
            put_u8(&mut out, u8::from(*whole_word));
            put_u64(&mut out, *max_matches);
            put_u64(&mut out, *report_matches);
            put_u64(&mut out, *skip_matches);
            put_u64(&mut out, *max_output);
            match scope {
                None => put_u8(&mut out, 0),
                Some((start, end)) => {
                    put_u8(&mut out, 1);
                    put_u64(&mut out, *start);
                    put_u64(&mut out, *end);
                }
            }
            put_u64(&mut out, *source_total);
            put_bytes(&mut out, pattern);
        }
        ToHelper::Template {
            request_id,
            template,
        } => {
            put_u8(&mut out, TAG_TEMPLATE);
            put_u64(&mut out, *request_id);
            put_bytes(&mut out, template);
        }
        ToHelper::SourceChunk {
            request_id,
            offset,
            bytes,
        } => {
            put_u8(&mut out, TAG_SOURCE_CHUNK);
            put_u64(&mut out, *request_id);
            put_u64(&mut out, *offset);
            put_bytes(&mut out, bytes);
        }
        ToHelper::EndOfSource { request_id } => {
            put_u8(&mut out, TAG_END_OF_SOURCE);
            put_u64(&mut out, *request_id);
        }
        ToHelper::Cancel { request_id } => {
            put_u8(&mut out, TAG_CANCEL);
            put_u64(&mut out, *request_id);
        }
    }
    out
}

fn decode_to_helper(bytes: &[u8]) -> Result<ToHelper, FrameError> {
    let mut reader = Reader { bytes };
    let message = match reader.u8()? {
        TAG_HANDSHAKE => ToHelper::Handshake {
            protocol_version: reader.u32()?,
            engine_version: reader.u32()?,
        },
        TAG_BEGIN => {
            let request_id = reader.u64()?;
            let kind = match reader.u8()? {
                0 => RequestKind::Find,
                1 => RequestKind::Replace,
                _ => return Err(FrameError::Malformed),
            };
            let mode = match reader.u8()? {
                0 => MatchMode::Literal,
                1 => MatchMode::Regex,
                _ => return Err(FrameError::Malformed),
            };
            let case = reader.u8()?;
            let word = reader.u8()?;
            if case > 1 || word > 1 {
                return Err(FrameError::Malformed);
            }
            let max_matches = reader.u64()?;
            let report_matches = reader.u64()?;
            let skip_matches = reader.u64()?;
            let max_output = reader.u64()?;
            let scope = match reader.u8()? {
                0 => None,
                1 => {
                    let start = reader.u64()?;
                    let end = reader.u64()?;
                    if end < start {
                        return Err(FrameError::Malformed);
                    }
                    Some((start, end))
                }
                _ => return Err(FrameError::Malformed),
            };
            ToHelper::Begin {
                request_id,
                kind,
                mode,
                case_sensitive: case == 1,
                whole_word: word == 1,
                max_matches,
                report_matches,
                skip_matches,
                max_output,
                scope,
                source_total: reader.u64()?,
                pattern: reader.bytes()?.to_vec(),
            }
        }
        TAG_TEMPLATE => {
            let request_id = reader.u64()?;
            ToHelper::Template {
                request_id,
                template: reader.bytes()?.to_vec(),
            }
        }
        TAG_SOURCE_CHUNK => {
            let request_id = reader.u64()?;
            let offset = reader.u64()?;
            ToHelper::SourceChunk {
                request_id,
                offset,
                bytes: reader.bytes()?.to_vec(),
            }
        }
        TAG_END_OF_SOURCE => ToHelper::EndOfSource {
            request_id: reader.u64()?,
        },
        TAG_CANCEL => ToHelper::Cancel {
            request_id: reader.u64()?,
        },
        _ => return Err(FrameError::Malformed),
    };
    // Strict framing: a message must consume its entire frame. Trailing
    // bytes mean a foreign or forged layout and are a typed refusal, never
    // silently ignored (§6.2: reject unrecognized frames before allocation
    // or interpretation).
    if !reader.bytes.is_empty() {
        return Err(FrameError::Malformed);
    }
    Ok(message)
}

fn encode_from_helper(message: &FromHelper) -> Result<Vec<u8>, FrameError> {
    let mut out = Vec::with_capacity(64);
    match message {
        FromHelper::HandshakeReply(Ok((protocol, engine))) => {
            put_u8(&mut out, TAG_HANDSHAKE_REPLY);
            put_u8(&mut out, 1);
            put_u32(&mut out, *protocol);
            put_u32(&mut out, *engine);
        }
        FromHelper::HandshakeReply(Err(failure)) => {
            put_u8(&mut out, TAG_HANDSHAKE_REPLY);
            put_u8(&mut out, 0);
            put_u8(&mut out, failure_tag(*failure));
        }
        FromHelper::Accepted { request_id } => {
            put_u8(&mut out, TAG_ACCEPTED);
            put_u64(&mut out, *request_id);
        }
        FromHelper::Page {
            request_id,
            matches,
        } => {
            put_u8(&mut out, TAG_PAGE);
            put_u64(&mut out, *request_id);
            put_u32(
                &mut out,
                u32::try_from(matches.len()).map_err(|_| FrameError::Oversize {
                    observed: matches.len(),
                })?,
            );
            for hit in matches {
                put_wire_match(&mut out, hit)?;
            }
        }
        FromHelper::MatchStart {
            request_id,
            index,
            matched,
            expansion_len,
        } => {
            put_u8(&mut out, TAG_MATCH_START);
            put_u64(&mut out, *request_id);
            put_u64(&mut out, *index);
            put_wire_match(&mut out, matched)?;
            put_u64(&mut out, *expansion_len);
        }
        FromHelper::ExpansionChunk {
            request_id,
            index,
            offset,
            bytes,
        } => {
            put_u8(&mut out, TAG_EXPANSION_CHUNK);
            put_u64(&mut out, *request_id);
            put_u64(&mut out, *index);
            put_u64(&mut out, *offset);
            put_bytes(&mut out, bytes);
        }
        FromHelper::Done {
            request_id,
            total_matches,
            output_len,
        } => {
            put_u8(&mut out, TAG_DONE);
            put_u64(&mut out, *request_id);
            put_u64(&mut out, *total_matches);
            put_u64(&mut out, *output_len);
        }
        FromHelper::Failed {
            request_id,
            failure,
        } => {
            put_u8(&mut out, TAG_FAILED);
            put_u64(&mut out, *request_id);
            put_u8(&mut out, failure_tag(*failure));
        }
    }
    Ok(out)
}

fn failure_tag(failure: FailureCode) -> u8 {
    match failure {
        FailureCode::Protocol => 1,
        FailureCode::Limit => 2,
        FailureCode::Pattern => 3,
        FailureCode::Template => 4,
        FailureCode::Source => 5,
        FailureCode::Cancelled => 6,
    }
}

fn failure_from_tag(tag: u8) -> Result<FailureCode, FrameError> {
    Ok(match tag {
        1 => FailureCode::Protocol,
        2 => FailureCode::Limit,
        3 => FailureCode::Pattern,
        4 => FailureCode::Template,
        5 => FailureCode::Source,
        6 => FailureCode::Cancelled,
        _ => return Err(FrameError::Malformed),
    })
}

fn decode_from_helper(bytes: &[u8]) -> Result<FromHelper, FrameError> {
    let mut reader = Reader { bytes };
    let message = match reader.u8()? {
        TAG_HANDSHAKE_REPLY => {
            let ok = reader.u8()?;
            if ok == 1 {
                FromHelper::HandshakeReply(Ok((reader.u32()?, reader.u32()?)))
            } else {
                FromHelper::HandshakeReply(Err(failure_from_tag(reader.u8()?)?))
            }
        }
        TAG_ACCEPTED => FromHelper::Accepted {
            request_id: reader.u64()?,
        },
        TAG_PAGE => {
            let request_id = reader.u64()?;
            let count = reader.u32()? as usize;
            let mut matches = Vec::with_capacity(count.min(MATCHES_PER_PAGE));
            for _ in 0..count {
                matches.push(read_wire_match(&mut reader)?);
            }
            FromHelper::Page {
                request_id,
                matches,
            }
        }
        TAG_MATCH_START => {
            let request_id = reader.u64()?;
            let index = reader.u64()?;
            let matched = read_wire_match(&mut reader)?;
            if !matched.expansion.is_empty() {
                return Err(FrameError::Malformed);
            }
            let expansion_len = reader.u64()?;
            if expansion_len > SOURCE_MAX_BYTES {
                return Err(FrameError::Malformed);
            }
            FromHelper::MatchStart {
                request_id,
                index,
                matched,
                expansion_len,
            }
        }
        TAG_EXPANSION_CHUNK => FromHelper::ExpansionChunk {
            request_id: reader.u64()?,
            index: reader.u64()?,
            offset: reader.u64()?,
            bytes: reader.bytes()?.to_vec(),
        },
        TAG_DONE => FromHelper::Done {
            request_id: reader.u64()?,
            total_matches: reader.u64()?,
            output_len: reader.u64()?,
        },
        TAG_FAILED => FromHelper::Failed {
            request_id: reader.u64()?,
            failure: failure_from_tag(reader.u8()?)?,
        },
        _ => return Err(FrameError::Malformed),
    };
    // The parent's decoder is as strict as the helper's: trailing bytes in
    // any reply frame are a typed protocol refusal.
    if !reader.bytes.is_empty() {
        return Err(FrameError::Malformed);
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One past the frame bound as a u32: the hostile length prefix used to
    /// prove the reader rejects oversize headers before allocating.
    /// `FRAME_MAX_BYTES` is 64 KiB, far inside the u32 range.
    #[allow(clippy::cast_possible_truncation)]
    const HOSTILE_PREFIX: u32 = (FRAME_MAX_BYTES as u32) + 1;

    #[allow(clippy::cast_possible_truncation)] // asserted-equal lengths are bounded by the frame check
    #[test]
    fn round_trips_every_parent_message() {
        let messages = [
            ToHelper::Handshake {
                protocol_version: PROTOCOL_VERSION,
                engine_version: engine_version(),
            },
            ToHelper::Begin {
                request_id: 7,
                kind: RequestKind::Replace,
                mode: MatchMode::Regex,
                case_sensitive: true,
                whole_word: true,
                max_matches: 1_000,
                report_matches: 50,
                skip_matches: 25,
                max_output: 4096,
                scope: Some((12, 4096)),
                source_total: 32 * 1024 * 1024,
                pattern: b"a(b)?c".to_vec(),
            },
            ToHelper::Begin {
                request_id: 8,
                kind: RequestKind::Find,
                mode: MatchMode::Regex,
                case_sensitive: false,
                whole_word: false,
                max_matches: 1_000_000,
                report_matches: 0,
                skip_matches: 256,
                max_output: u64::MAX,
                scope: None,
                source_total: 10,
                pattern: b"ab".to_vec(),
            },
            ToHelper::Template {
                request_id: 7,
                template: b"[$1]".to_vec(),
            },
            ToHelper::SourceChunk {
                request_id: 7,
                offset: 4096,
                bytes: vec![b'x'; 4096],
            },
            ToHelper::EndOfSource { request_id: 7 },
            ToHelper::Cancel { request_id: 7 },
        ];
        for message in &messages {
            let frame = encode(message).expect("within bounds");
            let (len, payload) = frame.split_at(4);
            assert_eq!(
                u32::from_le_bytes(len.try_into().expect("prefix")),
                payload.len() as u32
            );
            assert_eq!(&decode(payload).expect("decodes"), message);
        }
    }

    #[test]
    fn round_trips_every_helper_reply_including_pages() {
        let page = FromHelper::Page {
            request_id: 3,
            matches: vec![
                WireMatch {
                    start: 4,
                    end: 9,
                    groups: vec![Some((4, 9)), None, Some((5, 8))],
                    expansion: b"replaced".to_vec(),
                },
                WireMatch {
                    start: 10,
                    end: 10,
                    groups: vec![Some((10, 10))],
                    expansion: Vec::new(),
                },
            ],
        };
        let messages = [
            FromHelper::HandshakeReply(Ok((PROTOCOL_VERSION, engine_version()))),
            FromHelper::HandshakeReply(Err(FailureCode::Protocol)),
            FromHelper::Accepted { request_id: 3 },
            page.clone(),
            FromHelper::MatchStart {
                request_id: 3,
                index: 7,
                matched: WireMatch {
                    start: 0,
                    end: 1,
                    groups: vec![Some((0, 1))],
                    expansion: Vec::new(),
                },
                expansion_len: 70_000,
            },
            FromHelper::ExpansionChunk {
                request_id: 3,
                index: 7,
                offset: 32_768,
                bytes: vec![b'x'; 32 * 1024],
            },
            FromHelper::Done {
                request_id: 3,
                total_matches: 2,
                output_len: 17,
            },
            FromHelper::Failed {
                request_id: 3,
                failure: FailureCode::Pattern,
            },
        ];
        for message in &messages {
            let frame = encode_reply(message).expect("within bounds");
            let payload = &frame[4..];
            assert_eq!(&decode_reply(payload).expect("decodes"), message);
        }
    }

    #[test]
    fn oversize_frames_are_refused_before_allocation() {
        let big = ToHelper::SourceChunk {
            request_id: 1,
            offset: 0,
            bytes: vec![0u8; FRAME_MAX_BYTES + 1],
        };
        assert_eq!(
            encode(&big),
            Err(FrameError::Oversize {
                observed: FRAME_MAX_BYTES + 1 + 8 + 8 + 1 + 8
            })
        );
        // A hostile length prefix is rejected before reading the payload.
        let mut hostile = Vec::new();
        put_u32(&mut hostile, HOSTILE_PREFIX);
        hostile.extend_from_slice(&[0u8; 16]);
        let mut cursor = std::io::Cursor::new(hostile);
        assert!(matches!(
            read_reply(&mut cursor),
            Err(FrameError::PeerOversize { .. })
        ));
    }

    #[test]
    fn truncated_and_malformed_frames_are_typed() {
        let frame = encode(&ToHelper::Cancel { request_id: 9 }).expect("frame");
        let mut short = std::io::Cursor::new(frame[..frame.len() - 2].to_vec());
        assert_eq!(read_reply(&mut short), Err(FrameError::Truncated));
        assert_eq!(decode(&[0xFF, 0]), Err(FrameError::Malformed));
        // Unknown failure tag and unknown mode byte are malformed, not panics.
        assert_eq!(
            decode(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 7]),
            Err(FrameError::Malformed)
        );
    }

    #[test]
    fn trailing_bytes_are_malformed_both_directions() {
        // A valid frame plus one forged extension byte: the strict decoder
        // refuses instead of silently ignoring the unknown tail.
        let mut frame = encode(&ToHelper::EndOfSource { request_id: 3 }).expect("frame");
        frame.push(0x7F);
        let payload = &frame[4..];
        assert_eq!(decode(payload), Err(FrameError::Malformed));
        let mut reply = encode_reply(&FromHelper::Accepted { request_id: 3 }).expect("reply frame");
        reply.push(0x00);
        assert_eq!(decode_reply(&reply[4..]), Err(FrameError::Malformed));
    }

    #[test]
    fn begin_kind_scope_and_bounds_round_trip_both_kinds() {
        for (kind, scope) in [
            (RequestKind::Find, None),
            (RequestKind::Replace, Some((3u64, 9u64))),
        ] {
            let message = ToHelper::Begin {
                request_id: 5,
                kind,
                mode: MatchMode::Regex,
                case_sensitive: false,
                whole_word: true,
                max_matches: 1_000_000,
                report_matches: 10,
                skip_matches: 512,
                max_output: 2048,
                scope,
                source_total: 64,
                pattern: b"a(b)".to_vec(),
            };
            let frame = encode(&message).expect("within bounds");
            assert_eq!(decode(&frame[4..]).expect("decodes"), message);
        }
        // A descending scope pair is malformed, never a silent empty range.
        let mut out = vec![0x02u8];
        out.extend_from_slice(&1u64.to_le_bytes()); // request id
        out.push(1); // kind: replace
        out.push(1); // mode: regex
        out.push(0); // case
        out.push(0); // word
        out.extend_from_slice(&10u64.to_le_bytes()); // max matches
        out.extend_from_slice(&10u64.to_le_bytes()); // report
        out.extend_from_slice(&0u64.to_le_bytes()); // skip
        out.extend_from_slice(&1024u64.to_le_bytes()); // max output
        out.push(1); // scope present
        out.extend_from_slice(&9u64.to_le_bytes()); // start
        out.extend_from_slice(&3u64.to_le_bytes()); // end < start
        out.extend_from_slice(&0u64.to_le_bytes()); // source total
        out.extend_from_slice(&0u64.to_le_bytes()); // pattern length
        assert_eq!(decode(&out), Err(FrameError::Malformed));
    }
}
