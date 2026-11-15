//! Workspace codec migrations (RFC-0003 §22, W06, NFR-19).
//!
//! The format must survive one supported previous-major migration and must
//! refuse unknown future majors without overwriting the file. The migration
//! dispatch is the single place a schema transformation lands, and future
//! schemas are refused here so the codec's version gate and this module
//! stay in sync.

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
    // Dispatch per supported source schema.
    let migrated = match schema {
        1 => migrate_v1_to_v2(payload),
        // The current schema decodes as-is.
        CURRENT_SCHEMA => payload.clone(),
        other => {
            return Err(CodecError::UnsupportedSchema { found: other });
        }
    };
    serde_json::from_value(migrated).map_err(|error| CodecError::Malformed(error.to_string()))
}

/// v1 → v2: unresolved link targets render in the graph by default again.
///
/// v1 persisted graph scopes while its own default hid unresolved targets,
/// so a stored `show_unresolved: false` is that broken default, never a
/// deliberate choice — under it the grey nodes never rendered at all, so
/// there was nothing visible for the filter to hide. Every v1 graph scope
/// re-defaults them visible; from v2 on the stored value is a real user
/// decision and is honored as-is.
fn migrate_v1_to_v2(payload: &serde_json::Value) -> serde_json::Value {
    let mut current = payload.clone();
    if let Some(tabs) = current
        .get_mut("tabs")
        .and_then(serde_json::Value::as_array_mut)
    {
        for tab in tabs {
            let is_graph = tab
                .get("kind")
                .and_then(|kind| kind.get("kind"))
                .and_then(serde_json::Value::as_str)
                == Some("graph");
            if !is_graph {
                continue;
            }
            if let Some(scope) = tab.get_mut("kind").and_then(|kind| kind.get_mut("scope"))
                && let Some(object) = scope.as_object_mut()
            {
                object.insert("show_unresolved".to_owned(), serde_json::Value::Bool(true));
            }
        }
    }
    if let Some(object) = current.as_object_mut() {
        object.insert("schema".to_owned(), serde_json::Value::from(CURRENT_SCHEMA));
    }
    current
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
        let decoded = to_current(&payload(CURRENT_SCHEMA)).expect("decodes");
        assert_eq!(decoded.schema, CURRENT_SCHEMA);
        assert_eq!(decoded.focus, "t1");
    }

    #[test]
    fn v1_graph_scopes_redefault_unresolved_visible_and_v2_choices_are_honored() {
        let graph_scope = |unresolved: bool| {
            serde_json::json!({
                "predicate": { "kind": "all" },
                "neighborhood": null,
                "include_attachments": false,
                "include_embeds": false,
                "show_unresolved": unresolved,
            })
        };
        let mut v1 = payload(1);
        let object = v1.as_object_mut().expect("object");
        object["tabs"] = serde_json::json!([
            { "id": "t1", "kind": { "kind": "empty" } },
            { "id": "t2", "kind": { "kind": "graph", "scope": graph_scope(false) } },
        ]);
        object["groups"] = serde_json::json!([
            { "id": "g1", "tabs": ["t1", "t2"], "active": "t2" },
        ]);
        let decoded = to_current(&v1).expect("decodes");
        assert_eq!(decoded.schema, CURRENT_SCHEMA);
        let mut unresolved = decoded.tabs.iter().map(|tab| match &tab.kind {
            crate::codec::PersistedKind::Graph { scope, .. } => scope.show_unresolved,
            _ => true,
        });
        assert_eq!(unresolved.next(), Some(true), "non-graph tab untouched");
        assert_eq!(
            unresolved.next(),
            Some(true),
            "v1 graph scope redefaults unresolved visible"
        );

        // From v2 on, a stored false is a deliberate user choice.
        let mut v2 = payload(CURRENT_SCHEMA);
        let object = v2.as_object_mut().expect("object");
        object["tabs"] = serde_json::json!([
            { "id": "t1", "kind": { "kind": "graph", "scope": graph_scope(false) } },
        ]);
        let decoded = to_current(&v2).expect("decodes");
        match &decoded.tabs[0].kind {
            crate::codec::PersistedKind::Graph { scope, .. } => {
                assert!(!scope.show_unresolved, "v2 choice honored");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
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
