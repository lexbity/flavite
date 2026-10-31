//! Human-readable current/saved relationship provenance; never inferred from color.
use crate::host::graph::GraphTabState;
pub fn label(state: &GraphTabState) -> String {
    if state.updating {
        return "Updating current relationships…".into();
    }
    if state.failed_overlay_count > 0 {
        return format!(
            "{} sources have unavailable current relationships; saved history remains separate",
            state.failed_overlay_count
        );
    }
    if state.overlay_count > 0 {
        format!(
            "{} unsaved source projections · saved text search",
            state.overlay_count
        )
    } else {
        "Saved relationships · current open buffers covered by index".into()
    }
}
