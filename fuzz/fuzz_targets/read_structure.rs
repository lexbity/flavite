#![no_main]
//! Canonical Read tree and guarded task/property patch invariants.
use flavite_syntax::{properties, read};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 1024 * 1024 {
        return;
    }
    let Ok(text) = read::normalized(data) else {
        return;
    };
    if let Ok(document) = read::parse_normalized(&text) {
        let mut pending: Vec<_> = document.nodes.iter().collect();
        while let Some(node) = pending.pop() {
            assert!(node.range.start <= node.range.end);
            assert!(text.is_char_boundary(node.range.start));
            assert!(text.is_char_boundary(node.range.end));
            pending.extend(&node.children);
            if let read::ReadKind::Task(checked) = node.kind {
                let (range, replacement) =
                    read::task_patch(&text, node.range.start, checked).unwrap();
                let mut edited = text.clone();
                edited.replace_range(range.clone(), &replacement);
                assert_eq!(&edited[..range.start], &text[..range.start]);
                assert_eq!(&edited[range.end..], &text[range.end..]);
                let (range, replacement) =
                    read::task_patch(&edited, node.range.start, !checked).unwrap();
                edited.replace_range(range.clone(), &replacement);
                // Patches are canonicalizing: the round trip lands on the
                // canonical marker for the original state (`[x]` for the
                // checked `x`/`X`, `[ ]` for any whitespace interior the
                // scanner accepts), never on the raw original bytes.
                let original = &text[range.start..range.end];
                let canonical = if original.as_bytes()[1] == b'x'
                    || original.as_bytes()[1] == b'X'
                {
                    "[x]"
                } else {
                    "[ ]"
                };
                assert_eq!(&edited[range.start..range.end], canonical);
            }
        }
    }
    if let Ok(fields) = properties::inspect(&text) {
        for field in fields.iter().filter(|p| p.key.len() <= 8192).take(64) {
            let value = properties::ScalarValue::Text("Fuzz '雪' # scalar".into());
            let (range, replacement) = properties::patch(&text, &field.key, &value).unwrap();
            let mut edited = text.clone();
            edited.replace_range(range.clone(), &replacement);
            assert_eq!(&edited[..range.start], &text[..range.start]);
            assert_eq!(
                &edited[range.start + replacement.len()..],
                &text[range.end..]
            );
            let fields = properties::inspect(&edited).unwrap();
            assert_eq!(
                fields.iter().find(|p| p.key == field.key).unwrap().value,
                value
            );
        }
    }
});
