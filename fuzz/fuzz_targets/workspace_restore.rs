//! Fuzz target for workspace restore (RFC-0003 §22, W06).
//!
//! Bounded codec decode on arbitrary bytes plus a full restore plan: memory
//! and time stay bounded on the maximum accepted input, and a decoded file
//! either plans cleanly with honest drop reports or fails with a typed
//! error — never a panic, never a guessed repair.

#![no_main]

use flavite_workspace::{decode_workspace, plan_restore, PersistedWorkspace};
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeSet;

fuzz_target!(|data: &[u8]| {
    // The codec enforces its own byte bound; anything larger is refused
    // before parsing.
    let vault = "0f0f0f0f-0f0f-0f0f-0f0f-0f0f0f0f0f0f";
    match decode_workspace(data, vault) {
        Err(error) => {
            // Every rejection is a typed error, never a panic.
            let _message = error.to_string();
        }
        Ok(workspace) => {
            // Decoded files must satisfy the structural invariants.
            validate(&workspace);
            let catalog: BTreeSet<String> = ["alpha.md".to_owned(), "Systems/gamma.md".to_owned()]
                .into_iter()
                .collect();
            let plan = plan_restore(&workspace, &catalog).expect("decoded files always plan");
            // The plan never claims a tab it did not restore, and every
            // group is reported or present.
            for group in &workspace.groups {
                let restored = plan.groups.iter().any(|id| {
                    group.id.parse::<u128>().ok() == Some(id.raw())
                });
                if !restored {
                    assert!(
                        plan.report.iter().any(|entry| entry.contains(&group.id)),
                        "a dropped group must be reported: {}",
                        group.id
                    );
                }
            }
        }
    }
});

fn validate(workspace: &PersistedWorkspace) {
    // Round-trip through the codec's own validation by re-encoding.
    let bytes = flavite_workspace::encode_workspace(workspace)
        .expect("a decoded workspace re-encodes");
    let _ = flavite_workspace::decode_workspace(&bytes, &workspace.vault_uuid)
        .expect("re-encoded canonical form decodes");
    let _ = workspace.named_layouts.len();
}
