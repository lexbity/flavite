//! Workspace codec migrations (RFC-0003 §22, W06, NFR-19).
//!
//! The format must survive one supported previous-major migration and must
//! refuse unknown future majors without overwriting the file. Schema 1 is
//! the first durable schema; the migration dispatch is the single place a
//! schema 1 → 2 transformation will land, and future schemas are refused
//! here so the codec's version gate and this module stay in sync.

use crate::codec::{CURRENT_SCHEMA, CodecError, MIN_SUPPORTED_SCHEMA, PersistedWorkspace};

/// Migrate a canonical payload to the current schema and decode it.
///
/// # Errors
/// Returns [`CodecError::FutureSchema`] for schemas newer than this build
/// (the caller must never overwrite such a file), [`CodecError::UnsupportedSchema`]
/// for schemas older than the supported range, and [`CodecError::Malformed`]
/// when the payload does not deserialize.
pub fn to_current(payload: &serde_json::Value) -> Result<PersistedWorkspace, CodecError> {
    let schema_raw = payload
        .get("schema")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| CodecError::Malformed("payload missing schema".into()))?;
    let schema = u32::try_from(schema_raw).unwrap_or(u32::MAX);
    if schema > CURRENT_SCHEMA {
        return Err(CodecError::FutureSchema { found: schema });
    }
    if schema < MIN_SUPPORTED_SCHEMA {
        return Err(CodecError::UnsupportedSchema { found: schema });
    }
    // Dispatch per supported source schema; schema 1 is the identity.
    let migrated = match schema {
        1 => payload.clone(),
        other => {
            return Err(CodecError::UnsupportedSchema { found: other });
        }
    };
    serde_json::from_value(migrated).map_err(|error| CodecError::Malformed(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(schema: u32) -> serde_json::Value {
        serde_json::json!({
            "schema": schema,
            "vault_uuid": "01234567-89ab-cdef-0123-456789abcdef",
            "tree": { "node": "group", "id": "g1" },
            "groups": [{ "id": "g1", "tabs": ["t1"], "active": "t1" }],
            "tabs": [{ "id": "t1", "kind": { "kind": "empty" } }],
            "focus": "t1",
            "maximized": null,
        })
    }

    #[test]
    fn current_schema_decodes_identity() {
        let decoded = to_current(&payload(1)).expect("decodes");
        assert_eq!(decoded.schema, 1);
        assert_eq!(decoded.focus, "t1");
    }

    #[test]
    fn future_schema_is_refused_without_overwrite() {
        assert!(matches!(
            to_current(&payload(CURRENT_SCHEMA + 1)),
            Err(CodecError::FutureSchema { .. })
        ));
    }

    #[test]
    fn prehistoric_schema_is_refused() {
        assert!(matches!(
            to_current(&payload(MIN_SUPPORTED_SCHEMA - 1)),
            Err(CodecError::UnsupportedSchema { .. })
        ));
    }

    #[test]
    fn missing_schema_is_malformed() {
        let mut value = payload(1);
        value
            .as_object_mut()
            .expect("object")
            .remove("schema")
            .expect("removed");
        assert!(matches!(to_current(&value), Err(CodecError::Malformed(_))));
    }
}
