//! Real headless activation admission for API integration fixtures.
#![allow(dead_code)]
pub fn admit(
    api: &flavite_api::Flavite,
    mut invocation: flavite_api::Invocation,
) -> flavite_api::Invocation {
    if matches!(
        invocation.command_id,
        "apply_edits" | "buffer.undo" | "buffer.redo" | "buffer.restore_state"
    ) && invocation.args.get("operation").is_none()
        && let Some(buffer) = invocation
            .args
            .get("buffer_id")
            .and_then(serde_json::Value::as_u64)
        && let Ok(revision) = api
            .get_buffer_view(buffer.into(), None)
            .map(|view| view.revision)
    {
        let operation = api
            .issue_editor_operation(buffer, revision, None)
            .expect("headless fixture activation");
        invocation.args["operation"] = serde_json::to_value(operation).unwrap();
    }
    invocation
}
