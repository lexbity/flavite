//! Link serialization/insertion property tests (RFC-0003 §21, W05).
//!
//! Hostile delimiters, embeds, bracket adjacency, code/math/HTML/
//! frontmatter ranges, Unicode, and CRLF: no proof means no edit; a proof
//! means the actual parser finds exactly the intended new occurrence and
//! every prior occurrence is unchanged.

use flavite_syntax::link_format::{ProofRejection, prove_insertion, serialize_link};
use flavite_syntax::pulldown_glue::parse_note;

fn link(name: &str) -> flavite_syntax::link_format::SerializedLink {
    serialize_link(&[name.to_owned()], None).expect("representable target")
}

/// The contract every accepted proof must satisfy on the real parser.
fn assert_proof_holds(source: &str, caret: usize, name: &str) {
    let serialized = link(name);
    let Ok(original) = parse_note(source.as_bytes()) else {
        panic!("test source must parse: {source:?}");
    };
    let proof =
        prove_insertion(source, caret, &serialized).expect("proof must hold for this input");
    let mut edited = String::with_capacity(source.len() + proof.replacement.len());
    edited.push_str(&source[..proof.start]);
    edited.push_str(&proof.replacement);
    edited.push_str(&source[proof.end..]);
    let parsed = parse_note(edited.as_bytes()).expect("edited document parses");
    // Exactly one new intended occurrence at the inserted range.
    let inserted = parsed
        .links
        .iter()
        .find(|candidate| {
            candidate.span.start.0 == proof.start
                && candidate.span.end.0 == proof.start + proof.replacement.len()
        })
        .unwrap_or_else(|| panic!("no occurrence at the inserted range in {edited:?}"));
    assert_eq!(inserted.target_raw, proof.target_raw);
    assert!(!inserted.is_embed);
    // Every original occurrence survives with an identical interpretation.
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
            "original occurrence {link:?} did not survive in {edited:?}"
        );
    }
}

#[test]
fn table_of_hostile_insertion_points_is_decided_by_the_proof() {
    let cases: &[(&str, usize, bool)] = &[
        // (source, caret, expect_proof)
        ("plain text\n", 10, true),
        ("before\nafter\n", 7, true),
        ("see [[other]] here", 4, true), // directly before an existing link
        ("see [[other]] here", 14, true), // directly after an existing link
        ("see [[other]] here", 5, false), // inside an existing link
        ("see [[other]] here", 9, false), // inside an existing link
        ("see `code here` x", 10, false), // inline code
        ("```\nfence\n```\n", 7, false), // code fence
        ("$$\nmath\n$$\n", 7, false),    // math block
        ("a $x$ b\n", 3, false),         // inside inline math
        ("a $x$ b\n", 6, true),          // right after inline math
        ("<div>html</div>\n", 11, false), // raw HTML
        ("bang !", 6, false),            // `!` directly before the link
        ("bang !here", 7, true),         // `!` not adjacent to the brackets
        ("xx[[", 4, false),              // bracket adjacency opens first
        ("]]", 0, true),                 // trailing close binds to the new link
        ("", 0, true),                   // empty document
        ("héllo wörld\n", 8, true),      // Unicode body
        ("héllo", 2, false),             // caret splits a char (out of bounds)
        ("---\ntitle: x\n---\nbody\n", 22, true),
        ("---\ntitle: x\n---\nbody\n", 7, false), // inside frontmatter
    ];
    // CRLF sources are proven in normalized coordinates (the vault's text
    // coordinate system); the raw string itself is never proof input.
    let crlf = flavite_syntax::text::TextSnapshot::new(b"a\r\nb\r\n")
        .expect("snapshot")
        .text()
        .to_owned();
    assert_eq!(crlf, "a\nb\n");
    for (source, caret, expect_proof) in cases {
        let result = prove_insertion(source, *caret, &link("beta"));
        assert_eq!(
            result.is_ok(),
            *expect_proof,
            "source {source:?} caret {caret}: {result:?}"
        );
        if result.is_ok() {
            assert_proof_holds(source, *caret, "beta");
        }
    }
    // A caret between the two normalized lines hosts a link; the parse
    // proof runs on the normalized document the editor actually edits.
    assert!(prove_insertion(&crlf, 2, &link("beta")).is_ok());
    assert_proof_holds(&crlf, 2, "beta");
}

#[test]
fn randomized_documents_neither_overclaim_nor_mutate_without_proof() {
    // Deterministic xorshift; repeated runs replay the same corpus.
    let mut state = 0x243F_6A88_85A3_08D3u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let fragments = [
        "alpha\n",
        "[[beta]]\n",
        "![[embed]]\n",
        "# Heading\n",
        "`code` ",
        "```\nfence\n```\n",
        "$$\nmath\n$$\n",
        "<em>html</em>",
        "---\nfront: matter\n---\n",
        "text with | pipes # hashes\n",
        "méllöm\n",
        "\r\n",
        "]] [[ junk [[ ]] ",
    ];
    for _ in 0..2_000 {
        let mut source = String::new();
        for _ in 0..(next() % 6 + 1) {
            source.push_str(fragments[(next() as usize) % fragments.len()]);
        }
        let caret = (next() as usize) % (source.len() + 1);
        if let Ok(proof) = prove_insertion(&source, caret, &link("beta")) {
            assert_proof_holds(&source, caret, "beta");
            assert_eq!(proof.start, caret);
            assert_eq!(proof.end, caret);
        }
    }
}

#[test]
fn unrepresentable_targets_are_refused_before_any_proof() {
    assert!(serialize_link(&["a|b".to_owned()], None).is_err());
    let rejection = prove_insertion("plain", 6, &link("beta")).err();
    assert_eq!(rejection, Some(ProofRejection::CaretOutOfBounds));
    // Sanity: interior and end-of-text carets are linkable in plain text.
    assert!(prove_insertion("plain", 3, &link("beta")).is_ok());
    assert!(prove_insertion("plain", 5, &link("beta")).is_ok());
}
