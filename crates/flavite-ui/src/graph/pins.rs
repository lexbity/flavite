//! Versioned graph pin state: the in-memory codec and identity rules
//! (RFC-0003 D-11, TD-11, W07).
//!
//! Pins are user state, never cache state. Stable IDs are used only when
//! the caller has established they are unconflicted; all other entities
//! keep the exact native path bytes supplied by the index. Unknown JSON
//! fields are retained at both the document and pin-record levels when the
//! file is updated (§4.12).
//!
//! TD-11 moved persistence behind the API: this module no longer touches
//! the filesystem (the W07 contract gates fail if it ever does). The UI
//! decodes the document the API read, mutates the in-memory store through
//! [`PinCommand`]s, and re-encodes bytes for the API's generation-checked
//! write. The on-disk location and byte format (
//! `<vault>/.flavite/graph.json`) are unchanged — an existing pin file
//! keeps its identity semantics and unknown fields across the migration.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current on-disk graph-state schema.
pub const GRAPH_STATE_VERSION: u32 = 1;

/// Stable, reversible identity for a pinned graph entity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PinKey {
    /// An unconflicted frontmatter stable ID.
    StableId(String),
    /// Hex encoding of the exact native relative path key bytes (§4.16.2).
    NativePath(String),
}

impl PinKey {
    /// Use a stable ID only when it is present and has no duplicate
    /// conflict. Otherwise preserve identity using the lossless native
    /// path bytes.
    #[must_use]
    pub fn for_note(stable_id: Option<&str>, id_conflict: bool, native_path: &[u8]) -> Self {
        if !id_conflict && let Some(stable_id) = stable_id.filter(|id| !id.is_empty()) {
            return Self::StableId(stable_id.to_owned());
        }
        Self::from_native_path(native_path)
    }

    /// Construct a key from reversible native path bytes.
    #[must_use]
    pub fn from_native_path(native_path: &[u8]) -> Self {
        Self::NativePath(hex_encode(native_path))
    }

    /// Decode the native path bytes from a path key.
    pub fn native_path(&self) -> Option<Vec<u8>> {
        match self {
            Self::StableId(_) => None,
            Self::NativePath(encoded) => hex_decode(encoded),
        }
    }
}

/// One pin and any unrecognized fields from its JSON record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PinRecord {
    /// World-space graph position.
    pub position: [f32; 2],
    /// Fields introduced by newer versions are retained on round-trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// User action for pinning or unpinning a graph entity.
#[derive(Debug, Clone, PartialEq)]
pub enum PinCommand {
    /// Pin a node at a graph-world position.
    Pin { key: PinKey, position: [f32; 2] },
    /// Remove the node's pin.
    Unpin { key: PinKey },
}

impl PinRecord {
    /// Construct a pin record with no additional fields.
    #[must_use]
    pub fn new(position: [f32; 2]) -> Self {
        Self {
            position,
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct GraphStateFile {
    version: u32,
    #[serde(default)]
    pins: Vec<PinEntry>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PinEntry {
    key: PinKey,
    position: [f32; 2],
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

/// In-memory graph pins with lossless unknown-field preservation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PinStore {
    pins: BTreeMap<PinKey, PinRecord>,
    extra: BTreeMap<String, Value>,
}

impl PinStore {
    /// Decode `<vault>/.flavite/graph.json` bytes; empty bytes mean no
    /// pins (a first-ever read returns an empty store, not an error).
    ///
    /// # Errors
    /// Typed [`PinStoreError`] for a newer schema, malformed keys or
    /// positions, or duplicate identities. A refused decode leaves the
    /// caller's previous store untouched.
    pub fn decode(bytes: &[u8]) -> Result<Self, PinStoreError> {
        if bytes.is_empty() {
            return Ok(Self::default());
        }
        let file: GraphStateFile = serde_json::from_slice(bytes)?;
        if file.version != GRAPH_STATE_VERSION {
            return Err(PinStoreError::UnsupportedVersion(file.version));
        }
        let mut pins = BTreeMap::new();
        for entry in file.pins {
            validate_key(&entry.key)?;
            validate_position(entry.position)?;
            let record = PinRecord {
                position: entry.position,
                extra: entry.extra,
            };
            if pins.insert(entry.key, record).is_some() {
                return Err(PinStoreError::DuplicateKey);
            }
        }
        Ok(Self {
            pins,
            extra: file.extra,
        })
    }

    /// Encode to the established on-disk bytes (pretty JSON, document and
    /// record unknown fields preserved verbatim).
    ///
    /// # Errors
    /// Serialization of a validated store cannot normally fail; the typed
    /// error is surfaced rather than swallowed.
    pub fn encode(&self) -> Result<Vec<u8>, PinStoreError> {
        let file = GraphStateFile {
            version: GRAPH_STATE_VERSION,
            pins: self
                .pins
                .iter()
                .map(|(key, record)| PinEntry {
                    key: key.clone(),
                    position: record.position,
                    extra: record.extra.clone(),
                })
                .collect(),
            extra: self.extra.clone(),
        };
        Ok(serde_json::to_vec_pretty(&file)?)
    }

    /// Get a pin by identity.
    #[must_use]
    pub fn get(&self, key: &PinKey) -> Option<&PinRecord> {
        self.pins.get(key)
    }

    /// Iterate in stable deterministic key order.
    pub fn iter(&self) -> impl Iterator<Item = (&PinKey, &PinRecord)> {
        self.pins.iter()
    }

    /// Number of live pins.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pins.len()
    }

    /// Whether nothing is pinned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }

    /// Set or replace a world-space pin.
    pub fn set(&mut self, key: PinKey, position: [f32; 2]) -> Result<(), PinStoreError> {
        validate_key(&key)?;
        validate_position(position)?;
        self.pins
            .entry(key)
            .and_modify(|record| record.position = position)
            .or_insert_with(|| PinRecord::new(position));
        Ok(())
    }

    /// Remove a pin, returning whether one existed.
    pub fn remove(&mut self, key: &PinKey) -> bool {
        self.pins.remove(key).is_some()
    }

    /// Apply a user pin/unpin action. Hosts persist after a successful
    /// change through the API user-state write.
    pub fn execute(&mut self, command: PinCommand) -> Result<bool, PinStoreError> {
        match command {
            PinCommand::Pin { key, position } => {
                let changed = self.get(&key).is_none_or(|old| old.position != position);
                self.set(key, position)?;
                Ok(changed)
            }
            PinCommand::Unpin { key } => Ok(self.remove(&key)),
        }
    }

    /// Carry a pin to a renamed path only when the rename evidence is
    /// T0–T2. `from` and `to` must be the actual old/new pin identities;
    /// this helper never manufactures a stable ID or infers identity from
    /// content.
    pub fn carry(&mut self, from: &PinKey, to: PinKey, tier: &str) -> bool {
        if !carry_pin_for_tier(tier) || from == &to {
            return false;
        }
        let Some(record) = self.pins.remove(from) else {
            return false;
        };
        self.pins.insert(to, record);
        true
    }

    /// Add or replace a root-level unknown field (useful to preserve
    /// metadata when another component updates the graph state).
    pub fn set_extra(&mut self, key: String, value: Value) {
        self.extra.insert(key, value);
    }

    /// Read a root-level unknown field.
    #[must_use]
    pub fn extra(&self) -> &BTreeMap<String, Value> {
        &self.extra
    }
}

/// Whether rename evidence is authoritative enough to carry identity state.
/// Tier strings match `RenameTier::as_str`; unknown values are denied.
#[must_use]
pub fn carry_pin_for_tier(tier: &str) -> bool {
    matches!(tier, "T0" | "T1" | "T2")
}

/// Error decoding or encoding graph user state.
///
/// The former `Io` variant is gone: persistence moved behind the API's
/// vault user-state store (TD-11), whose typed errors reach the host
/// through [`flavite_api::FlaviteError::UserState`].
#[derive(Debug, thiserror::Error)]
pub enum PinStoreError {
    /// JSON was malformed.
    #[error("graph state JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    /// The file was written by a newer version.
    #[error("unsupported graph state version {0}")]
    UnsupportedVersion(u32),
    /// A native path key is malformed.
    #[error("invalid native path key")]
    InvalidPathKey,
    /// A position contains NaN or infinity.
    #[error("pin positions must be finite")]
    InvalidPosition,
    /// The file contains the same native or stable identity more than once.
    #[error("graph state contains a duplicate pin key")]
    DuplicateKey,
}

fn validate_key(key: &PinKey) -> Result<(), PinStoreError> {
    if let PinKey::NativePath(encoded) = key
        && hex_decode(encoded).is_none()
    {
        return Err(PinStoreError::InvalidPathKey);
    }
    if matches!(key, PinKey::StableId(id) if id.is_empty()) {
        return Err(PinStoreError::InvalidPathKey);
    }
    Ok(())
}

fn validate_position(position: [f32; 2]) -> Result<(), PinStoreError> {
    position
        .iter()
        .all(|coordinate| coordinate.is_finite())
        .then_some(())
        .ok_or(PinStoreError::InvalidPosition)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(HEX[usize::from(byte >> 4)]));
        result.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    result
}

fn hex_decode(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) || !value.is_ascii() {
        return None;
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_use_only_unconflicted_stable_ids_and_preserve_native_bytes() {
        assert_eq!(
            PinKey::for_note(Some("stable"), false, b"notes/a.md"),
            PinKey::StableId("stable".into())
        );
        let native = [0xff, 0x2f, 0x00, 0x80];
        let key = PinKey::for_note(Some("duplicate"), true, &native);
        assert_eq!(key.native_path(), Some(native.to_vec()));
        assert_eq!(key, PinKey::NativePath("ff2f0080".into()));
        assert_eq!(
            PinKey::for_note(None, false, &native).native_path(),
            Some(native.to_vec())
        );
    }

    #[test]
    fn tier_matrix_carries_only_t0_through_t2() {
        for tier in ["T0", "T1", "T2"] {
            assert!(carry_pin_for_tier(tier));
        }
        for tier in ["T3", "T4", "unknown"] {
            assert!(!carry_pin_for_tier(tier));
        }
    }

    #[test]
    fn carry_moves_pin_only_for_authoritative_evidence() {
        let old = PinKey::from_native_path(b"old.md");
        let new = PinKey::from_native_path(b"new.md");
        let mut store = PinStore::default();
        store.set(old.clone(), [1.0, 2.0]).unwrap();
        assert!(!store.carry(&old, new.clone(), "T3"));
        assert!(store.get(&old).is_some());
        assert!(store.carry(&old, new.clone(), "T1"));
        assert!(store.get(&old).is_none());
        assert_eq!(store.get(&new).unwrap().position, [1.0, 2.0]);
    }

    #[test]
    fn pin_and_unpin_commands_report_changes_and_are_idempotent() {
        let key = PinKey::from_native_path(b"note.md");
        let mut store = PinStore::default();
        assert!(
            store
                .execute(PinCommand::Pin {
                    key: key.clone(),
                    position: [2.0, 4.0],
                })
                .unwrap()
        );
        assert!(
            !store
                .execute(PinCommand::Pin {
                    key: key.clone(),
                    position: [2.0, 4.0],
                })
                .unwrap()
        );
        assert!(
            store
                .execute(PinCommand::Unpin { key: key.clone() })
                .unwrap()
        );
        assert!(!store.execute(PinCommand::Unpin { key }).unwrap());
    }

    #[test]
    fn round_trip_preserves_unknown_document_and_record_fields() {
        let original = r#"{"version":1,"future_document":{"enabled":true},"pins":[{"key":{"kind":"stable_id","value":"one"},"position":[3.0,4.0],"future_pin":"keep"}] }"#;
        let mut store = PinStore::decode(original.as_bytes()).unwrap();
        store
            .set(PinKey::StableId("two".into()), [5.0, 6.0])
            .unwrap();
        let bytes = store.encode().unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["future_document"]["enabled"], true);
        assert!(json.to_string().contains("future_pin"));
        let reloaded = PinStore::decode(&bytes).unwrap();
        assert_eq!(
            reloaded
                .get(&PinKey::StableId("two".into()))
                .unwrap()
                .position,
            [5.0, 6.0]
        );
        assert_eq!(reloaded.extra()["future_document"]["enabled"], true);
        assert_eq!(
            reloaded.get(&PinKey::StableId("one".into())).unwrap().extra["future_pin"],
            "keep"
        );
    }

    #[test]
    fn invalid_positions_and_future_versions_are_refused_without_writing() {
        let mut store = PinStore::default();
        assert!(matches!(
            store.set(PinKey::StableId("a".into()), [f32::NAN, 0.0]),
            Err(PinStoreError::InvalidPosition)
        ));
        // Decode refuses a newer schema; the caller keeps its current file
        // bytes protected (the API store never overwrote anything here).
        assert!(matches!(
            PinStore::decode(br#"{"version":9,"pins":[]}"#),
            Err(PinStoreError::UnsupportedVersion(9))
        ));
        assert!(matches!(
            PinStore::decode(br#"{"version":1,"pins":[{"key":{"kind":"native_path","value":"zz"},"position":[0.0,0.0]}]}"#),
            Err(PinStoreError::InvalidPathKey)
        ));
        assert!(matches!(
            PinStore::decode(br#"{"version":1,"pins":[{"key":{"kind":"stable_id","value":"x"},"position":[0.0,0.0]},{"key":{"kind":"stable_id","value":"x"},"position":[1.0,1.0]}]}"#),
            Err(PinStoreError::DuplicateKey)
        ));
    }

    #[test]
    fn empty_bytes_decode_to_an_empty_store() {
        assert_eq!(PinStore::decode(b"").unwrap(), PinStore::default());
    }
}
