//! Typed wire contracts for guarded task and scalar-property edits.
pub use flavite_syntax::properties::{ScalarProperty, ScalarValue, inspect as inspect_properties};
pub use flavite_syntax::read::{OutlineEntry, ReadDocument, ReadKind, ReadNode, plain_text};
use serde::Deserialize;
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropertyEditArgs {
    pub buffer_id: u64,
    pub revision: u64,
    pub key: String,
    pub value: ScalarValue,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskEditArgs {
    pub buffer_id: u64,
    pub revision: u64,
    pub start: usize,
    pub checked: bool,
}
