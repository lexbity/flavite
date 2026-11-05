//! Typed encoding commands (RFC-0006 §6.10 / FR-22–FR-25): `note.reopen_encoding`
//! and `note.convert_encoding`.
//!
//! Reopen reinterprets one note's disk bytes under an explicit descriptor
//! (ReadNotes, exact current raw hash, no pending save, no unsaved text).
//! Convert changes an open buffer's save format with one isolated Undo
//! boundary (WriteNotes, exact revision); disk is untouched until the next
//! normal save. Both are explicit user actions — nothing here re-encodes
//! silently, and malformed input never replaces bytes with U+FFFD.

use crate::command::Invocation;
use crate::flavite::decode;
use flavite_vault::session::{Session, SessionCommand, SessionResult};
use serde::Deserialize;
use std::path::PathBuf;

/// BOM intent on the wire. Discriminants are explicit snake_case strings
/// (§6.1); `auto` strips a matching signature for the requested family and
/// otherwise treats the file as absent, `present` requires the signature,
/// `absent` preserves signature-shaped leading bytes as content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BomModeArg {
    Auto,
    Present,
    Absent,
}

impl BomModeArg {
    #[must_use]
    pub const fn into_core(self) -> flavite_vault::encoding::BomMode {
        match self {
            Self::Auto => flavite_vault::encoding::BomMode::Auto,
            Self::Present => flavite_vault::encoding::BomMode::Present,
            Self::Absent => flavite_vault::encoding::BomMode::Absent,
        }
    }
}

/// Arguments for `note.reopen_encoding`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReopenEncodingArgs {
    /// Vault-relative note path.
    pub path: PathBuf,
    /// Exact current raw-byte hash the caller observed (64 lowercase hex).
    pub raw_hash: String,
    /// Encoding family: `ascii` | `utf8` | `utf16le` | `utf16be`.
    pub encoding: String,
    pub bom: BomModeArg,
}

/// Arguments for `note.convert_encoding`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConvertEncodingArgs {
    pub buffer_id: u64,
    pub revision: u64,
    /// Encoding family: `ascii` | `utf8` | `utf16le` | `utf16be`.
    pub encoding: String,
    /// BOM intent for the converted output. `auto` is not meaningful for a
    /// conversion (there are no bytes to match) and is treated as `absent`.
    pub bom: BomModeArg,
}

/// `note.reopen_encoding` (ReadNotes): decode the exact observed bytes under
/// an explicit descriptor. The Session enforces the dirty/pending-save/raw-hash
/// guards and returns typed refusals.
pub(crate) fn handle_reopen_encoding(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: ReopenEncodingArgs = decode(invocation)?;
    let expected_raw_hash = decode_hash32(&args.raw_hash)?;
    let encoding = decode_encoding(&args.encoding)?;
    Ok(session.execute(SessionCommand::ReopenEncoding {
        path: args.path,
        expected_raw_hash,
        encoding,
        bom: args.bom.into_core(),
    }))
}

/// `note.convert_encoding` (WriteNotes on source): validate the exact target
/// descriptor, then convert under the Session's revision guard. Whole-note
/// representability is checked before any metadata is installed; an
/// unencodable target is a typed refusal with zero effects.
pub(crate) fn handle_convert_encoding(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: ConvertEncodingArgs = decode(invocation)?;
    let encoding = decode_encoding(&args.encoding)?;
    let bom = match args.bom {
        BomModeArg::Present => true,
        BomModeArg::Auto | BomModeArg::Absent => false,
    };
    let target = flavite_encoding::SourceFormat::new(encoding, bom)
        .map_err(|error| format!("invalid target format: {error}"))?;
    Ok(session.execute(SessionCommand::ConvertEncoding {
        buffer_id: flavite_buffer::revision::BufferId::from(args.buffer_id),
        revision: flavite_buffer::revision::Revision::new(args.revision),
        target,
    }))
}

/// Parse a 64-character lowercase hex raw hash into 32 bytes.
fn decode_hash32(value: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(value).map_err(|_| "raw_hash must be 64 hex characters")?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "raw_hash must be 64 hex characters")?;
    Ok(bytes)
}

/// Parse an encoding family code (§6.10: explicit, never guessed).
fn decode_encoding(value: &str) -> Result<flavite_encoding::FileEncoding, String> {
    flavite_encoding::FileEncoding::from_code(value)
        .ok_or_else(|| "encoding must be one of ascii|utf8|utf16le|utf16be".to_owned())
}
