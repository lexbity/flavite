//! Device-local workspace persistence codec (RFC-0003 §22, W06, D-02/D-11).
//!
//! The durable projection of a [`WorkspaceState`]: the layout tree, tab
//! groups, view kinds with their durable identities, focus, maximize state,
//! reopen history, and named layouts. Excluded by construction: writing
//! targets, incarnations, buffer ids, publication counters, and plans
//! (FR-26). The file is a canonical JSON payload plus a blake3 checksum,
//! bounded before any recursive decoding (NFR-12), and refused — never
//! overwritten — when its schema is newer than this build (NFR-19).

use crate::reducer::WorkspaceState;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The schema this build writes and reads.
pub const CURRENT_SCHEMA: u32 = 1;
/// The oldest schema this build accepts.
pub const MIN_SUPPORTED_SCHEMA: u32 = 1;
/// Hard byte bound before any parsing (NFR-12: decode workspace ≤4 MiB).
pub const MAX_WORKSPACE_FILE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum JSON nesting depth accepted (NFR-12).
pub const MAX_JSON_DEPTH: usize = 64;
/// Maximum layout tree depth (NFR-01).
pub const MAX_TREE_DEPTH: usize = 16;

/// Why a persisted workspace file could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// The file exceeds the bounded size.
    TooLarge {
        /// Actual byte length.
        size: usize,
    },
    /// The JSON nesting exceeds the bounded depth.
    TooDeep {
        /// Actual depth.
        depth: usize,
    },
    /// The file is not the expected envelope.
    Malformed(String),
    /// The canonical payload does not match its checksum.
    ChecksumMismatch,
    /// The schema is newer than this build; the file is protected.
    FutureSchema {
        /// The schema found in the file.
        found: u32,
    },
    /// The schema predates the supported range.
    UnsupportedSchema {
        /// The schema found in the file.
        found: u32,
    },
    /// The file belongs to a different vault.
    VaultMismatch {
        /// The vault this build expects.
        expected: String,
        /// The vault named in the file.
        found: String,
    },
    /// The decoded structure violates identity/reference invariants.
    InvalidStructure(String),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { size } => {
                write!(
                    f,
                    "workspace file is {size} bytes, over the {MAX_WORKSPACE_FILE_BYTES} byte bound"
                )
            }
            Self::TooDeep { depth } => {
                write!(
                    f,
                    "workspace file nesting depth {depth} exceeds {MAX_JSON_DEPTH}"
                )
            }
            Self::Malformed(message) => write!(f, "workspace file is malformed: {message}"),
            Self::ChecksumMismatch => {
                write!(f, "workspace file checksum does not match its payload")
            }
            Self::FutureSchema { found } => write!(
                f,
                "workspace file schema {found} is newer than this build; the file was left untouched"
            ),
            Self::UnsupportedSchema { found } => {
                write!(
                    f,
                    "workspace file schema {found} is not supported by this build"
                )
            }
            Self::VaultMismatch { expected, found } => {
                write!(
                    f,
                    "workspace file belongs to vault {found}, expected {expected}"
                )
            }
            Self::InvalidStructure(message) => {
                write!(f, "workspace file violates its invariants: {message}")
            }
        }
    }
}

impl std::error::Error for CodecError {}

/// A durable view kind. Unknown later kinds are preserved inert (`Other`)
/// and re-serialized unchanged (RFC §22: unknown kinds are retained).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PersistedKind {
    /// A blank tab.
    Empty,
    /// A source-reading note view.
    Note {
        /// Vault-relative path components.
        locator: Vec<String>,
    },
    /// The vault note list.
    Notes,
    /// Saved source-text preview.
    Preview {
        /// The durable binding.
        binding: PersistedBinding,
    },
    /// Inbound links.
    Backlinks {
        /// The durable binding.
        binding: PersistedBinding,
    },
    /// A kind written by a newer build; preserved and re-emitted untouched.
    Other {
        /// The unknown kind tag.
        name: String,
        /// The unknown payload, preserved verbatim.
        data: serde_json::Value,
    },
}

/// A durable context binding (identities, never live view pointers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "binding", rename_all = "snake_case")]
pub enum PersistedBinding {
    /// Follow the document anchor.
    FollowDocument,
    /// Follow the exploration selection or the document anchor.
    FollowExplorationOrDocument,
    /// Follow a specific source tab.
    FollowSource {
        /// The followed tab id.
        tab: String,
    },
    /// Pin one entity by durable locator.
    Pinned {
        /// Vault-relative path components.
        path: Vec<String>,
    },
}

/// One tab: opaque persistent id plus its durable kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedTab {
    /// Persistent tab identity (u128 as string).
    pub id: String,
    /// The durable kind.
    pub kind: PersistedKind,
}

/// One tab group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedGroup {
    /// Persistent group identity.
    pub id: String,
    /// Member tab ids, in strip order.
    pub tabs: Vec<String>,
    /// The active tab id.
    pub active: String,
}

/// The layout tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum PersistedNode {
    /// A leaf holding one group.
    Group {
        /// The group id.
        id: String,
    },
    /// An interior split.
    Split {
        /// Persistent split identity.
        id: String,
        /// `side_by_side` or `stacked`.
        arrangement: String,
        /// Desired first-child ratio in ppm.
        ratio_ppm: u32,
        /// First child.
        first: Box<PersistedNode>,
        /// Second child.
        second: Box<PersistedNode>,
    },
}

/// A named layout: an independent durable workspace snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedLayout {
    /// User-facing name.
    pub name: String,
    /// The stored workspace.
    pub workspace: PersistedWorkspace,
}

/// The durable workspace projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedWorkspace {
    /// Schema version.
    pub schema: u32,
    /// The vault this workspace belongs to (D-11: keyed by vault UUID).
    pub vault_uuid: String,
    /// The layout tree.
    pub tree: PersistedNode,
    /// All groups.
    pub groups: Vec<PersistedGroup>,
    /// All tabs.
    pub tabs: Vec<PersistedTab>,
    /// The focused tab id.
    pub focus: String,
    /// The maximized group id, if any.
    pub maximized: Option<String>,
    /// Reopen history: note locators, most recent last.
    #[serde(default)]
    pub recently_closed: Vec<Vec<String>>,
    /// Named layouts saved by the user.
    #[serde(default)]
    pub named_layouts: Vec<NamedLayout>,
}

/// Project a live [`WorkspaceState`] onto the durable form (FR-26: writing
/// targets, incarnations, buffer ids, and publications are excluded by
/// construction — only layout truth and durable identities are read).
#[must_use]
pub fn persist(state: &WorkspaceState, vault_uuid: &str) -> PersistedWorkspace {
    let tabs = state
        .tabs()
        .map(|(id, session)| PersistedTab {
            id: id.raw().to_string(),
            kind: persist_kind(session.kind()),
        })
        .collect();
    let groups = state
        .groups()
        .map(|(id, group)| PersistedGroup {
            id: id.raw().to_string(),
            tabs: group
                .tabs()
                .iter()
                .map(|tab| tab.raw().to_string())
                .collect(),
            active: group.active().raw().to_string(),
        })
        .collect();
    let tree = persist_node(state.tree());
    PersistedWorkspace {
        schema: CURRENT_SCHEMA,
        vault_uuid: vault_uuid.to_owned(),
        tree,
        groups,
        tabs,
        focus: state.focused_tab().raw().to_string(),
        maximized: state.maximized_group().map(|group| group.raw().to_string()),
        recently_closed: state
            .recently_closed()
            .map(|locator| locator.components().to_vec())
            .collect(),
        named_layouts: Vec::new(),
    }
}

fn persist_kind(kind: &crate::view::ViewKind) -> PersistedKind {
    match kind {
        crate::view::ViewKind::Empty => PersistedKind::Empty,
        crate::view::ViewKind::Notes => PersistedKind::Notes,
        crate::view::ViewKind::Note(note) => PersistedKind::Note {
            locator: note.locator().components().to_vec(),
        },
        crate::view::ViewKind::Preview(binding) => PersistedKind::Preview {
            binding: persist_binding(binding),
        },
        crate::view::ViewKind::Backlinks(binding) => PersistedKind::Backlinks {
            binding: persist_binding(binding),
        },
    }
}

fn persist_binding(binding: &crate::context::ContextBinding) -> PersistedBinding {
    match binding {
        crate::context::ContextBinding::FollowDocument => PersistedBinding::FollowDocument,
        crate::context::ContextBinding::FollowExplorationOrDocument => {
            PersistedBinding::FollowExplorationOrDocument
        }
        crate::context::ContextBinding::FollowSource { tab } => PersistedBinding::FollowSource {
            tab: tab.raw().to_string(),
        },
        crate::context::ContextBinding::Pinned(locator) => PersistedBinding::Pinned {
            path: locator.path().components().to_vec(),
        },
    }
}

fn persist_node(node: &crate::layout::LayoutNode) -> PersistedNode {
    match node {
        crate::layout::LayoutNode::Group(id) => PersistedNode::Group {
            id: id.raw().to_string(),
        },
        crate::layout::LayoutNode::Split {
            id,
            arrangement,
            desired_ratio_ppm,
            first,
            second,
        } => PersistedNode::Split {
            id: id.raw().to_string(),
            arrangement: match arrangement {
                crate::layout::Arrangement::SideBySide => "side_by_side",
                crate::layout::Arrangement::Stacked => "stacked",
            }
            .to_owned(),
            ratio_ppm: *desired_ratio_ppm,
            first: Box::new(persist_node(first)),
            second: Box::new(persist_node(second)),
        },
    }
}

/// Encode a persisted workspace into the on-disk envelope: a canonical JSON
/// payload plus its blake3 checksum.
///
/// # Errors
/// Returns [`CodecError::Malformed`] when canonical serialization fails.
pub fn encode(workspace: &PersistedWorkspace) -> Result<Vec<u8>, CodecError> {
    // The checksum is taken over the canonical (sorted-key) payload, so it
    // matches whatever serialization order the reader reconstructs.
    let payload_value = serde_json::to_value(workspace)
        .map_err(|error| CodecError::Malformed(error.to_string()))?;
    let payload = serde_json::to_vec(&payload_value)
        .map_err(|error| CodecError::Malformed(error.to_string()))?;
    let checksum = blake3::hash(&payload).to_hex().to_string();
    let envelope = serde_json::json!({ "checksum": checksum, "payload": payload_value });
    let bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|error| CodecError::Malformed(error.to_string()))?;
    if bytes.len() > MAX_WORKSPACE_FILE_BYTES {
        return Err(CodecError::TooLarge { size: bytes.len() });
    }
    Ok(bytes)
}

/// Decode and fully validate an on-disk workspace file.
///
/// # Errors
/// Returns the first violated bound or invariant; a future schema is
/// refused so the file is never overwritten by an older build.
pub fn decode(bytes: &[u8], expected_vault_uuid: &str) -> Result<PersistedWorkspace, CodecError> {
    if bytes.len() > MAX_WORKSPACE_FILE_BYTES {
        return Err(CodecError::TooLarge { size: bytes.len() });
    }
    preflight_depth(bytes)?;
    let envelope: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| CodecError::Malformed(error.to_string()))?;
    let checksum = envelope
        .get("checksum")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| CodecError::Malformed("missing checksum".into()))?;
    let payload = envelope
        .get("payload")
        .ok_or_else(|| CodecError::Malformed("missing payload".into()))?;
    let canonical =
        serde_json::to_vec(payload).map_err(|error| CodecError::Malformed(error.to_string()))?;
    if blake3::hash(&canonical).to_hex().as_str() != checksum {
        return Err(CodecError::ChecksumMismatch);
    }
    let workspace = crate::migrate::to_current(payload)?;
    if workspace.vault_uuid != expected_vault_uuid {
        return Err(CodecError::VaultMismatch {
            expected: expected_vault_uuid.to_owned(),
            found: workspace.vault_uuid,
        });
    }
    validate(&workspace)?;
    Ok(workspace)
}

/// Bounded preflight: reject deep nesting before recursive parsing.
fn preflight_depth(bytes: &[u8]) -> Result<(), CodecError> {
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut max_depth: usize = 0;
    for &byte in bytes {
        if in_string {
            match byte {
                b'\\' if !escaped => escaped = true,
                _ if escaped => escaped = false,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                max_depth = max_depth.max(depth);
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if max_depth > MAX_JSON_DEPTH {
        return Err(CodecError::TooDeep { depth: max_depth });
    }
    Ok(())
}

/// Structural validation: unique identities, valid references, bounded
/// ratios and depth, exactly the tab multiset the tree and groups name.
///
/// # Errors
/// Returns [`CodecError::InvalidStructure`] on the first violated invariant.
pub fn validate(workspace: &PersistedWorkspace) -> Result<(), CodecError> {
    if workspace.schema < MIN_SUPPORTED_SCHEMA {
        return Err(CodecError::UnsupportedSchema {
            found: workspace.schema,
        });
    }
    if workspace.schema > CURRENT_SCHEMA {
        return Err(CodecError::FutureSchema {
            found: workspace.schema,
        });
    }
    let mut tab_ids = BTreeSet::new();
    for tab in &workspace.tabs {
        if tab.id.is_empty() {
            return Err(CodecError::InvalidStructure("empty tab id".into()));
        }
        if !tab_ids.insert(tab.id.as_str()) {
            return Err(CodecError::InvalidStructure(format!(
                "duplicate tab id {}",
                tab.id
            )));
        }
    }
    let mut group_ids: BTreeSet<String> = BTreeSet::new();
    let mut referenced: BTreeSet<&str> = BTreeSet::new();
    for group in &workspace.groups {
        if group.tabs.is_empty() {
            return Err(CodecError::InvalidStructure(format!(
                "group {} is empty",
                group.id
            )));
        }
        if !group_ids.insert(group.id.clone()) {
            return Err(CodecError::InvalidStructure(format!(
                "duplicate group id {}",
                group.id
            )));
        }
        if !group.tabs.iter().any(|tab| tab == &group.active) {
            return Err(CodecError::InvalidStructure(format!(
                "group {} active tab {} is not a member",
                group.id, group.active
            )));
        }
        for tab in &group.tabs {
            if !tab_ids.contains(tab.as_str()) {
                return Err(CodecError::InvalidStructure(format!(
                    "group {} references unknown tab {}",
                    group.id, tab
                )));
            }
            if !referenced.insert(tab.as_str()) {
                return Err(CodecError::InvalidStructure(format!(
                    "tab {tab} belongs to more than one group"
                )));
            }
        }
    }
    if referenced.len() != workspace.tabs.len() {
        return Err(CodecError::InvalidStructure(
            "some tabs belong to no group".into(),
        ));
    }
    let mut split_ids: BTreeSet<String> = BTreeSet::new();
    check_node(&workspace.tree, &mut split_ids, &group_ids, 1)?;
    if !tab_ids.contains(workspace.focus.as_str()) {
        return Err(CodecError::InvalidStructure(format!(
            "focus {} is not a known tab",
            workspace.focus
        )));
    }
    if let Some(maximized) = &workspace.maximized
        && !group_ids.contains(maximized)
    {
        return Err(CodecError::InvalidStructure(format!(
            "maximized group {maximized} is unknown"
        )));
    }
    for named in &workspace.named_layouts {
        if named.name.trim().is_empty() {
            return Err(CodecError::InvalidStructure("empty layout name".into()));
        }
        if named.workspace.vault_uuid != workspace.vault_uuid {
            return Err(CodecError::VaultMismatch {
                expected: workspace.vault_uuid.clone(),
                found: named.workspace.vault_uuid.clone(),
            });
        }
        validate_named(&named.workspace)?;
    }
    Ok(())
}

fn validate_named(workspace: &PersistedWorkspace) -> Result<(), CodecError> {
    // Named layouts carry their own consistent tree; their vault must match.
    validate(workspace).map_err(|error| match error {
        CodecError::VaultMismatch { expected, found } => {
            CodecError::VaultMismatch { expected, found }
        }
        other => other,
    })
}

#[allow(clippy::too_many_lines)]
fn check_node(
    node: &PersistedNode,
    split_ids: &mut BTreeSet<String>,
    group_ids: &BTreeSet<String>,
    depth: usize,
) -> Result<(), CodecError> {
    if depth > MAX_TREE_DEPTH {
        return Err(CodecError::InvalidStructure(format!(
            "tree depth exceeds {MAX_TREE_DEPTH}"
        )));
    }
    match node {
        PersistedNode::Group { id } => {
            if !group_ids.contains(id) {
                return Err(CodecError::InvalidStructure(format!(
                    "tree references unknown group {id}"
                )));
            }
            Ok(())
        }
        PersistedNode::Split {
            id,
            arrangement,
            ratio_ppm,
            first,
            second,
        } => {
            if !split_ids.insert(id.clone()) {
                return Err(CodecError::InvalidStructure(format!(
                    "duplicate split id {id}"
                )));
            }
            if arrangement != "side_by_side" && arrangement != "stacked" {
                return Err(CodecError::InvalidStructure(format!(
                    "unknown arrangement {arrangement:?}"
                )));
            }
            if *ratio_ppm < crate::layout::MIN_RATIO_PPM
                || *ratio_ppm > crate::layout::MAX_RATIO_PPM
            {
                return Err(CodecError::InvalidStructure(format!(
                    "ratio {ratio_ppm} is outside {}..={}",
                    crate::layout::MIN_RATIO_PPM,
                    crate::layout::MAX_RATIO_PPM
                )));
            }
            check_node(first, split_ids, group_ids, depth + 1)?;
            check_node(second, split_ids, group_ids, depth + 1)
        }
    }
}
