//! Typed semantic link insertion commands (RFC-0003 §9, W05).
//!
//! `link.prepare` builds a bounded, expiring plan; `link.insert` consumes it
//! once. Both commands require the vault write policy unconditionally, and
//! the raw edit payload shape (`ops`) is rejected with an explicit pointer
//! to `apply_edits` — there is no ambiguous union with the old raw shape.

use crate::Flavite;
use serde::Deserialize;

/// One gesture identity as presented by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkIntentArgs {
    /// Core-issued client namespace token.
    pub client: u128,
    /// Monotonic gesture sequence.
    pub sequence: u64,
}

/// The collapsed source caret a prepare is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkSourceArgs {
    /// Source buffer.
    pub buffer_id: u64,
    /// Exact source revision.
    pub revision: u64,
    /// Collapsed caret in normalized text bytes.
    pub caret: usize,
}

/// `link.prepare` arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkPrepareArgs {
    /// Gesture identity.
    pub intent: LinkIntentArgs,
    /// Source binding.
    pub source: LinkSourceArgs,
    /// Vault-relative target path.
    pub target_path: String,
    /// `disabled` or `prefer_stable` (D-06).
    pub stable_link_policy: String,
}

/// `link.insert` arguments. Only intent + plan ID: everything else lives in
/// the consumed plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkInsertArgs {
    /// Gesture identity.
    pub intent: LinkIntentArgs,
    /// The plan capability returned by `link.prepare`.
    pub plan_id: u128,
}

/// Validate `link.insert` arguments: the raw edit shape (`ops`) is rejected
/// with an explicit pointer before typed decoding, so the legacy payload
/// never fails with an ambiguous field error (RFC §9.1).
///
/// # Errors
/// Returns the rejection text for the registry schema.
pub fn validate_link_insert_args(value: &serde_json::Value) -> Result<(), String> {
    if value.get("ops").is_some() {
        return Err("use apply_edits for raw replacements".into());
    }
    serde_json::from_value::<LinkInsertArgs>(value.clone())
        .map(|_| ())
        .map_err(|error| error.to_string())
}

impl Flavite {
    /// Issue a core-semantic-link client namespace token for this host
    /// connection. One token per connection; gestures sequence inside it.
    #[must_use]
    pub fn new_link_client(&self) -> u128 {
        self.session.new_link_client().0
    }

    /// The typed command id for the raw-edit rejection carried by the
    /// `link.insert` schema, so the negative test and the registry agree.
    #[must_use]
    pub const fn raw_link_insert_rejection() -> &'static str {
        "use apply_edits for raw replacements"
    }
}
