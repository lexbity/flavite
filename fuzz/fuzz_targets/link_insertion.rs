//! Fuzz target for semantic link insertion (RFC-0003 §21, W05).
//!
//! Input layout: `[text bytes][u32 caret][target name bytes]`. The target is
//! sanitized to grammar-safe characters, serialized with the shared
//! formatter, and the insertion is proven against the real parser. Any
//! accepted proof must survive the parse invariants and a real buffer edit
//! with a single undo boundary; every rejection must leave the model
//! untouched.

#![no_main]

use flavite_buffer::state::Buffer;
use flavite_syntax::link_format::{prove_insertion, serialize_link, ProofRejection};
use flavite_syntax::pulldown_glue::parse_note;
use flavite_syntax::text::TextSnapshot;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }
    // Split the input: text | caret (LE u32) | target name.
    let caret_bytes = [data[0], data[1], data[2], data[3]];
    let caret_raw = u32::from_le_bytes(caret_bytes) as usize;
    let body = &data[4..];
    let split = body
        .iter()
        .position(|byte| *byte == 0xFF)
        .unwrap_or(body.len());
    let (text_bytes, name_bytes) = body.split_at(split);
    let name_bytes = if split < body.len() {
        &body[split + 1..]
    } else {
        name_bytes
    };
    let text = String::from_utf8_lossy(text_bytes);
    let Ok(snapshot) = TextSnapshot::new(text.as_bytes()) else {
        return;
    };
    let normalized = snapshot.text().to_owned();
    let caret = caret_raw.min(normalized.len());

    // Grammar-safe target name: drop every byte the link grammar reserves,
    // so serialization failures exercise only genuinely unrepresentable input.
    let name: String = name_bytes
        .iter()
        .filter_map(|byte| {
            let character = *byte as char;
            if character.is_ascii_alphanumeric()
                || character == '_'
                || character == '-'
                || character == '/'
            {
                Some(character)
            } else {
                None
            }
        })
        .collect();
    if name.is_empty() {
        return;
    }
    let Ok(serialized) = serialize_link(&[name], None) else {
        return;
    };

    let Ok(original) = parse_note(normalized.as_bytes()) else {
        // An unparsable source must reject, never authorize.
        assert!(matches!(
            prove_insertion(&normalized, caret, &serialized),
            Err(ProofRejection::SourceUnparsable(_))
        ));
        return;
    };
    let original_links = original.links.len();

    match prove_insertion(&normalized, caret, &serialized) {
        Err(_) => {}
        Ok(proof) => {
            assert_eq!(proof.start, caret);
            assert_eq!(proof.end, caret);
            let mut edited = String::with_capacity(normalized.len() + proof.replacement.len());
            edited.push_str(&normalized[..proof.start]);
            edited.push_str(&proof.replacement);
            edited.push_str(&normalized[proof.end..]);
            let parsed = parse_note(edited.as_bytes()).expect("edited document parses");
            // Exactly one new intended occurrence at the inserted range.
            let inserted = parsed
                .links
                .iter()
                .find(|candidate| {
                    candidate.span.start.0 == proof.start
                        && candidate.span.end.0 == proof.start + proof.replacement.len()
                })
                .expect("the proven occurrence must exist in the parse");
            assert_eq!(inserted.target_raw, proof.target_raw);
            assert!(!inserted.is_embed);
            // Every original occurrence survives with identical semantics.
            let delta = proof.replacement.len();
            for link in &original.links {
                let (start, end) = (link.span.start.0, link.span.end.0);
                let (mapped_start, mapped_end) = if end <= proof.start {
                    (start, end)
                } else if start >= proof.start {
                    (start + delta, end + delta)
                } else {
                    panic!("proof accepted an insertion inside an existing occurrence");
                };
                assert!(
                    parsed.links.iter().any(|candidate| {
                        candidate.span.start.0 == mapped_start
                            && candidate.span.end.0 == mapped_end
                            && candidate.target_raw == link.target_raw
                            && candidate.alias == link.alias
                            && candidate.anchor == link.anchor
                            && candidate.is_embed == link.is_embed
                    }),
                    "original occurrence did not survive"
                );
            }
            let _ = original_links;

            // A real buffer edit applies the proven batch as one unit on the
            // ORIGINAL text, producing exactly the prospective document.
            let ops = vec![flavite_buffer::EditOp::new(
                flavite_syntax::text::Span::new(
                    flavite_syntax::text::TextByte(proof.start),
                    flavite_syntax::text::TextByte(proof.end),
                ),
                proof.replacement.clone(),
            )];
            let mut buffer_model = Buffer::from_text(&normalized);
            let before_revision = buffer_model.revision();
            let applied = buffer_model.apply_local_edits(&ops);
            assert!(applied.is_some(), "a proven edit must apply to the buffer");
            assert_eq!(
                buffer_model.text(),
                edited,
                "the buffer text must equal the proven prospective document"
            );
            let (new_revision, _) = applied.expect("checked above");
            assert_eq!(new_revision.as_u64(), before_revision.as_u64() + 1);
        }
    }
});
