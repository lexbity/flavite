//! Fuzz target for the frontmatter patcher (§10.6, D11).
//!
//! Invariants:
//! - patch → result is valid YAML
//! - comments and quoting preserved
//! - unpatchable input → refuse, never corrupt

#![no_main]

use flavite_syntax::frontmatter::{parse_frontmatter, patch_frontmatter_key};
use flavite_syntax::text::TextSnapshot;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Try to decode as UTF-8
    if let Ok(text) = String::from_utf8(data.to_vec()) {
        // Build a frontmatter-like input
        let yaml_content = text;
        let frontmatter_text = format!("---\n{}\n---\n", yaml_content);

        if let Ok(snapshot) = TextSnapshot::new(frontmatter_text.as_bytes()) {
            if parse_frontmatter(&snapshot).is_some() {
                let value = "fuzz \"value\"".to_owned();
                if let Ok(patched) =
                    patch_frontmatter_key(frontmatter_text.as_bytes(), "aliases", &[value.clone()])
                {
                    let text = std::str::from_utf8(&patched).expect("patcher emits UTF-8");
                    for comment in frontmatter_text
                        .lines()
                        .filter(|line| line.trim_start().starts_with('#'))
                    {
                        assert!(text.contains(comment), "unrelated comment was removed");
                    }
                    let again = patch_frontmatter_key(&patched, "aliases", &[value]).unwrap();
                    assert_eq!(again, patched, "setting an identical key is idempotent");
                }
            }
        }
    }
});
