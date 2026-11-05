//! `DecodeMap` boundary/exhaustive-inverse property tests (AC-24, NFR-18).
//!
//! The reference here is built by an independent method (walking
//! `char_indices()` / explicit byte-order arithmetic) and compared against
//! the production sparse-checkpoint map at every byte position of small
//! inputs.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::doc_markdown,
    clippy::needless_range_loop
)]
use flavite_encoding::{
    DecodeLimits, DecodePolicy, EncodeLimits, FileEncoding, MAX_CHECKPOINT_SCALAR_GAP,
    SourceFormat, decode, encode,
};

fn no_cancel() -> impl FnMut() -> bool {
    move || false
}

/// Independent expected boundary tables for one format.
///
/// Returns (`source_boundary` -> decoded, `decoded_boundary` -> source). Only
/// scalar boundaries after BOM removal (and the BOM end / EOF themselves) are
/// legal.
type BoundaryTables = (Vec<(u64, u64)>, Vec<(u64, u64)>);

fn reference_tables(text: &str, format: SourceFormat) -> BoundaryTables {
    let bytes = encode(
        text,
        &format,
        &EncodeLimits::default_envelope(),
        &mut no_cancel(),
    )
    .unwrap();
    let bom = format.bom_len() as u64;
    let mut src: Vec<(u64, u64)> = Vec::new();
    let mut dec: Vec<(u64, u64)> = Vec::new();
    match format.encoding() {
        FileEncoding::Ascii | FileEncoding::Utf8 => {
            for (di, _) in text.char_indices() {
                src.push((di as u64 + bom, di as u64));
                dec.push((di as u64, di as u64 + bom));
            }
            src.push((bytes.len() as u64, text.len() as u64));
            dec.push((text.len() as u64, bytes.len() as u64));
            // Position 0 (inside/before BOM when BOM present) is invalid.
            if bom == 0 {
                src.push((0, 0));
                dec.push((0, 0));
            }
        }
        FileEncoding::Utf16Le | FileEncoding::Utf16Be => {
            let mut d = 0u64;
            let mut s = bom;
            for ch in text.chars() {
                src.push((s, d));
                dec.push((d, s));
                d += ch.len_utf8() as u64;
                s += ch.len_utf16() as u64 * 2;
            }
            src.push((bytes.len() as u64, d));
            dec.push((d, bytes.len() as u64));
        }
    }
    (src, dec)
}

fn check_against_reference(name: &str, text: &str, format: SourceFormat) {
    let bytes = encode(
        text,
        &format,
        &EncodeLimits::default_envelope(),
        &mut no_cancel(),
    )
    .unwrap_or_else(|e| panic!("{name}: encode: {e}"));
    let d = decode(
        &bytes,
        DecodePolicy::Explicit(format),
        &DecodeLimits::default_envelope(),
        &mut no_cancel(),
    )
    .unwrap_or_else(|e| panic!("{name}: decode: {e}"));
    assert_eq!(d.text(), text, "{name}: decoded content");
    assert_eq!(d.format(), format, "{name}: descriptor");
    let (src_table, dec_table) = reference_tables(text, format);
    let legal_src: std::collections::HashMap<u64, u64> = src_table.into_iter().collect();
    let legal_dec: std::collections::HashMap<u64, u64> = dec_table.into_iter().collect();
    // Enumerate EVERY byte position on both sides.
    for s in 0..=bytes.len() as u64 {
        let got = d.map().source_to_decoded(s);
        let want = legal_src.get(&s).copied();
        assert_eq!(got, want, "{name}: source_to_decoded({s})");
        if let Some(x) = want {
            // inversion holds on the legal domain
            assert_eq!(
                d.map().decoded_utf8_to_source(x),
                Some(s),
                "{name}: inversion at {s}"
            );
        }
    }
    for x in 0..=text.len() as u64 {
        let got = d.map().decoded_utf8_to_source(x);
        let want = legal_dec.get(&x).copied();
        assert_eq!(got, want, "{name}: decoded_utf8_to_source({x})");
    }
    // Out-of-range is refused.
    assert_eq!(d.map().source_to_decoded(bytes.len() as u64 + 1), None);
    assert_eq!(d.map().decoded_utf8_to_source(text.len() as u64 + 1), None);
}

#[test]
fn exhaustive_small_inputs_all_families() {
    let cases = [
        ("empty", ""),
        ("lf", "\n"),
        ("cr", "\r"),
        ("crlf", "\r\n"),
        ("mixed-eol", "a\r\nb\nc\r\n"),
        ("ascii", "abc"),
        ("twobyte", "é"),
        ("threebyte", "漢字"),
        ("emoji", "😀"),
        ("emoji-ascii", "A😀\r\n"),
        ("feff-content", "\u{FEFF}x"),
        ("feff-inside", "x\u{FEFF}y\n"),
        ("combining", "e\u{0301}\u{0302}x"),
        ("crmid", "a\rb\r\nc"),
        ("trail", "line\n\n"),
    ];
    let formats = [
        SourceFormat::ascii(),
        SourceFormat::utf8(),
        SourceFormat::utf8_with_bom(),
        SourceFormat::utf16le(),
        SourceFormat::utf16le_with_bom(),
        SourceFormat::utf16be(),
        SourceFormat::utf16be_with_bom(),
    ];
    for (name, text) in cases {
        let ascii_ok = text.is_ascii();
        for format in formats {
            if !ascii_ok && format.encoding() == FileEncoding::Ascii {
                continue; // unencodable by design
            }
            check_against_reference(&format!("{name}/{format}"), text, format);
        }
    }
}

#[test]
fn rejects_interiors_bom_surrogate_continuation_and_odd() {
    // UTF-16LE with BOM: "A😀" → interiors of the surrogate pair and odd
    // offsets must be None, while the pair start/end must be Some.
    let bytes = encode(
        "A😀",
        &SourceFormat::utf16le_with_bom(),
        &EncodeLimits::default_envelope(),
        &mut no_cancel(),
    )
    .unwrap();
    let d = decode(
        &bytes,
        DecodePolicy::Auto,
        &DecodeLimits::default_envelope(),
        &mut no_cancel(),
    )
    .unwrap();
    // BOM [0,2) invalid; 2 = 'A' start valid.
    assert_eq!(d.map().source_to_decoded(0), None);
    assert_eq!(d.map().source_to_decoded(1), None); // odd
    assert_eq!(d.map().source_to_decoded(2), Some(0)); // after BOM = scalar start
    assert_eq!(d.map().source_to_decoded(4), Some(1)); // start of high surrogate
    assert_eq!(d.map().source_to_decoded(6), None); // interior (low surrogate start)
    assert_eq!(d.map().source_to_decoded(8), Some(5)); // after pair / EOF
    assert_eq!(d.map().source_to_decoded(9), None); // beyond source_len
    // decoded side: interior continuation bytes of the emoji are refused.
    assert_eq!(d.map().decoded_utf8_to_source(2), None); // inside 4-byte emoji
    assert_eq!(d.map().decoded_utf8_to_source(3), None);
    assert_eq!(d.map().decoded_utf8_to_source(5), Some(bytes.len() as u64));
    // UTF-8 with BOM: positions inside the BOM are refused; a mid-sequence
    // continuation position is refused on both sides.
    let bytes = encode(
        "é\n",
        &SourceFormat::utf8_with_bom(),
        &EncodeLimits::default_envelope(),
        &mut no_cancel(),
    )
    .unwrap();
    let d = decode(
        &bytes,
        DecodePolicy::Auto,
        &DecodeLimits::default_envelope(),
        &mut no_cancel(),
    )
    .unwrap();
    for inside in [0u64, 1, 2, 3] {
        assert_eq!(
            d.map().source_to_decoded(inside),
            if inside == 3 { Some(0) } else { None },
            "BOM interior {inside}"
        );
    }
    assert_eq!(d.map().source_to_decoded(4), None); // inside é
    // Layout: BOM(3) + é(2 bytes: C3 A9) + '\n'(1) = len 6.
    // Boundaries: 3→0, 5→2 (start of '\n'), 6→3 (EOF).
    assert_eq!(d.map().source_to_decoded(5), Some(2));
    assert_eq!(d.map().source_to_decoded(6), Some(3));
}

#[test]
fn sparse_checkpoint_boundaries() {
    // Text with 254, 255, 256 and 511 scalars exercises the first checkpoint
    // interval, the boundary exactly at 255, the start of a new interval, and
    // the final short interval.
    for count in [253usize, 254, 255, 256, 257, 510, 511, 766] {
        let text = "a".repeat(count);
        for format in [SourceFormat::utf16le(), SourceFormat::utf16be_with_bom()] {
            check_against_reference(&format!("cp/{count}/{format}"), &text, format);
        }
        // With a supplementary char right after a checkpoint boundary.
        let mut t = "a".repeat(count);
        t.push('😀');
        t.push('b');
        check_against_reference(&format!("cp-emoji/{count}"), &t, SourceFormat::utf16le());
        // The recorded gap never exceeds the documented bound.
        let bytes = encode(
            &text,
            &SourceFormat::utf16le(),
            &EncodeLimits::default_envelope(),
            &mut no_cancel(),
        )
        .unwrap();
        let d = decode(
            &bytes,
            DecodePolicy::Explicit(SourceFormat::utf16le()),
            &DecodeLimits::default_envelope(),
            &mut no_cancel(),
        )
        .unwrap();
        let cps = d.map().checkpoints();
        assert_eq!(cps.first().map(|c| c.source), Some(0));
        let expected = count / MAX_CHECKPOINT_SCALAR_GAP + 1;
        assert_eq!(cps.len(), expected, "checkpoint count for {count}");
        // Every gap ≤ 255 scalars ⇒ every query decodes at most 255 locally.
        for w in cps.windows(2) {
            let scalar_gap = (w[1].source - w[0].source) / 2;
            assert!(scalar_gap <= MAX_CHECKPOINT_SCALAR_GAP as u32);
        }
    }
}

#[test]
fn multibyte_decoded_growth_across_checkpoints() {
    // Three-byte decoded scalars: checkpoint positions on the decoded axis
    // must remain exact at interval edges.
    for count in [254usize, 255, 256, 300] {
        let text = "漢".repeat(count); // 3 UTF-8 bytes, 1 UTF-16 unit each
        check_against_reference(&format!("han/{count}"), &text, SourceFormat::utf16be());
        let bytes = encode(
            &text,
            &SourceFormat::utf16be(),
            &EncodeLimits::default_envelope(),
            &mut no_cancel(),
        )
        .unwrap();
        let d = decode(
            &bytes,
            DecodePolicy::Explicit(SourceFormat::utf16be()),
            &DecodeLimits::default_envelope(),
            &mut no_cancel(),
        )
        .unwrap();
        let cps = d.map().checkpoints();
        for (i, c) in cps.iter().enumerate() {
            if i == 0 {
                assert_eq!((c.decoded_utf8, c.source), (0, 0));
            } else {
                assert_eq!(c.decoded_utf8 as usize % 3, 0);
                assert_eq!(
                    c.source as usize,
                    (i * MAX_CHECKPOINT_SCALAR_GAP) as u32 as usize * 2
                );
            }
        }
    }
}

// ---- Randomized property: strict round trip + inversion -----------------

use proptest::prelude::*;

proptest! {
    #[test]
    fn random_text_round_trips_under_every_family(ref text in ".{0,400}") {
        let formats = [
            SourceFormat::utf8(),
            SourceFormat::utf8_with_bom(),
            SourceFormat::utf16le(),
            SourceFormat::utf16le_with_bom(),
            SourceFormat::utf16be(),
            SourceFormat::utf16be_with_bom(),
        ];
        for format in formats {
            let bytes = encode(text, &format, &EncodeLimits::default_envelope(), &mut no_cancel()).unwrap();
            let d = decode(&bytes, DecodePolicy::Explicit(format), &DecodeLimits::default_envelope(), &mut no_cancel()).unwrap();
            prop_assert_eq!(d.text(), text.as_str());
            prop_assert_eq!(d.format(), format);
            // inversion at every scalar boundary
            let mut dec = 0u64;
            let mut src = d.format().bom_len() as u64;
            let utf16_family = format.encoding().is_utf16();
            for ch in text.chars() {
                prop_assert_eq!(d.map().decoded_utf8_to_source(dec), Some(src));
                prop_assert_eq!(d.map().source_to_decoded(src), Some(dec));
                dec += ch.len_utf8() as u64;
                src += if utf16_family {
                    ch.len_utf16() as u64 * 2
                } else {
                    ch.len_utf8() as u64
                };
            }
            prop_assert_eq!(d.map().decoded_utf8_to_source(dec), Some(bytes.len() as u64));
            prop_assert_eq!(d.map().source_to_decoded(bytes.len() as u64), Some(dec));
        }
        // ASCII only when representable.
        if text.is_ascii() {
            let bytes = encode(text, &SourceFormat::ascii(), &EncodeLimits::default_envelope(), &mut no_cancel()).unwrap();
            let d = decode(&bytes, DecodePolicy::Explicit(SourceFormat::ascii()), &DecodeLimits::default_envelope(), &mut no_cancel()).unwrap();
            prop_assert_eq!(d.text(), text);
        } else {
            let err = encode(text, &SourceFormat::ascii(), &EncodeLimits::default_envelope(), &mut no_cancel());
            prop_assert!(err.is_err());
        }
    }

    #[test]
    fn random_text_utf8_identity_boundaries(ref text in ".{0,300}") {
        for format in [SourceFormat::utf8(), SourceFormat::utf8_with_bom()] {
            let bytes = encode(text, &format, &EncodeLimits::default_envelope(), &mut no_cancel()).unwrap();
            let d = decode(&bytes, DecodePolicy::Explicit(format), &DecodeLimits::default_envelope(), &mut no_cancel()).unwrap();
            prop_assert_eq!(d.text(), text.as_str());
            let bom = d.format().bom_len() as u64;
            for (i, _) in text.char_indices() {
                prop_assert_eq!(d.map().source_to_decoded(i as u64 + bom), Some(i as u64));
                prop_assert_eq!(d.map().decoded_utf8_to_source(i as u64), Some(i as u64 + bom));
            }
            // every continuation byte interior is refused
            for (i, ch) in text.char_indices() {
                for k in 1..ch.len_utf8() {
                    let s = (i + k) as u64 + bom;
                    prop_assert_eq!(d.map().source_to_decoded(s), None);
                    prop_assert_eq!(d.map().decoded_utf8_to_source((i + k) as u64), None);
                }
            }
        }
    }
}
