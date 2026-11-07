//! Composed codec+EOL coordinate maps over UTF-8/UTF-16 fixtures
//! (RFC-0006 S00, AC-24).

use flavite_encoding::{DecodePolicy, SourceFormat};
use flavite_syntax::text::{Eol, SourceByte, Span, TextByte, TextSnapshot};

fn le(bytes: &[&[u8]]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&[0xFF, 0xFE]);
    for b in bytes {
        v.extend_from_slice(b);
    }
    v
}

#[test]
fn utf16_snapshot_preserves_text_and_descriptor() {
    // "A😀\r\nsecon\r\n" in UTF-16LE + BOM.
    let mut bytes = le(&[]);
    for unit in "A😀\r\nsecon\r\n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let snap = TextSnapshot::with_policy(&bytes, DecodePolicy::Auto).unwrap();
    assert_eq!(snap.format(), Some(SourceFormat::utf16le_with_bom()));
    assert!(snap.has_bom());
    // Normalized text: CRLF folded to LF.
    assert_eq!(snap.text(), "A😀\nsecon\n");
    assert_eq!(
        snap.source(),
        bytes,
        "byte-exact unedited round trip"
    );
    assert_eq!(snap.line_count(), 2);
    assert_eq!(snap.lines()[0].eol, Eol::Crlf);
    assert_eq!(snap.lines()[1].eol, Eol::Crlf);
}

#[test]
fn utf16_line_table_uses_real_scalar_widths() {
    let mut bytes = le(&[]);
    for unit in "A😀\r\nsecon\r\n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let snap = TextSnapshot::with_policy(&bytes, DecodePolicy::Auto).unwrap();
    // Layout: BOM(2) | 'A'(2) | pair(4) | CR(2) LF(2) | "secon"(10) | CR LF(4)
    // Line 0: content starts at 2, ends (CR start) at 8; line 1: 12..22; EOF 26.
    let l0 = &snap.lines()[0];
    assert_eq!(
        (l0.source_start, l0.source_end),
        (SourceByte(2), SourceByte(8))
    );
    let l1 = &snap.lines()[1];
    assert_eq!(
        (l1.source_start, l1.source_end),
        (SourceByte(12), SourceByte(22))
    );
    // Text coordinates are UTF-8 bytes of the normalized text.
    assert_eq!(l0.text_start, TextByte(0));
    assert_eq!(l0.text_end, TextByte(5)); // "A" + 4-byte emoji
    assert_eq!(l1.text_start, TextByte(6));
    assert_eq!(l1.text_end, TextByte(11));
}

#[test]
fn utf16_mapping_inverts_at_every_legal_boundary() {
    let mut bytes = Vec::from([0xFEu8, 0xFFu8]); // UTF-16BE BOM
    for unit in "ab\r\nc😀d\nef".encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    let snap = TextSnapshot::with_policy(&bytes, DecodePolicy::Auto).unwrap();
    assert_eq!(snap.text(), "ab\nc😀d\nef");
    // Every legal text boundary maps; inversion holds on the whole legal
    // domain (§6.10), including the LF terminator start of LF-terminated
    // lines (it maps to the folded LF text position).
    for t in legal_boundaries(snap.text()) {
        let s = snap
            .text_to_source(TextByte(t))
            .unwrap_or_else(|| panic!("text boundary {t} must map"));
        assert_eq!(
            snap.source_to_text(s),
            Some(TextByte(t)),
            "inversion at {t}"
        );
    }
    // The LF side of a CRLF pair stays unaddressable: it is inside the
    // terminator, not a legal boundary.
    let crlf_line = snap
        .lines()
        .iter()
        .find(|l| l.eol == Eol::Crlf)
        .expect("fixture has a CRLF line");
    assert_eq!(
        snap.source_to_text(SourceByte(crlf_line.source_end.0 + 1)),
        None,
        "CRLF interior is refused"
    );
    // EOF pair.
    let eof = snap.text_to_source(TextByte(snap.text().len())).unwrap();
    assert_eq!(eof, SourceByte(bytes.len()));
    assert_eq!(snap.source_to_text(eof), Some(TextByte(snap.text().len())));
}

fn legal_boundaries(text: &str) -> Vec<usize> {
    let mut v: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    v.push(text.len());
    v
}

#[test]
fn utf16_rejects_illegal_interiors() {
    // BOM(2) + 'A😀'
    let bytes: Vec<u8> = {
        let mut v = vec![0xFF, 0xFE];
        for unit in "A😀".encode_utf16() {
            v.extend_from_slice(&unit.to_le_bytes());
        }
        v
    };
    let snap = TextSnapshot::with_policy(&bytes, DecodePolicy::Auto).unwrap();
    // Odd source positions (inside a code unit) are refused.
    assert_eq!(snap.source_to_text(SourceByte(1)), None);
    assert_eq!(snap.source_to_text(SourceByte(5)), None);
    // Inside the surrogate pair (low unit) is refused.
    assert_eq!(snap.source_to_text(SourceByte(6)), None);
    // Scalar starts are accepted: BOM end = 2, pair start = 4, EOF = 8.
    assert_eq!(snap.source_to_text(SourceByte(2)), Some(TextByte(0)));
    assert_eq!(snap.source_to_text(SourceByte(4)), Some(TextByte(1)));
    assert_eq!(snap.source_to_text(SourceByte(8)), Some(TextByte(5)));
    // And decoded-space text→source refuses continuation interiors.
    assert_eq!(snap.text_to_source(TextByte(2)), None); // inside the emoji
    assert_eq!(snap.text_to_source(TextByte(3)), None);
    assert_eq!(snap.text_to_source(TextByte(6)), None); // out of range
}

#[test]
fn standalone_cr_and_leading_content_feff_survive_utf16_and_utf8() {
    // Standalone CR is content in both spaces; it must not fold or shift.
    let src = "a\rb\r\n";
    for format in [SourceFormat::utf8(), SourceFormat::utf16le_with_bom()] {
        let bytes = flavite_encoding::encode(
            src,
            format,
            &flavite_encoding::EncodeLimits::default(),
            &flavite_encoding::NOT_CANCELLED,
        )
        .unwrap();
        let snap = TextSnapshot::with_policy(&bytes, DecodePolicy::Explicit(format)).unwrap();
        assert_eq!(snap.text(), "a\rb\n");
        assert_eq!(snap.source(), bytes);
        // The content CR maps 1:1 onto itself through the composed map.
        let cr = snap.text_to_source(TextByte(1)).unwrap();
        assert_eq!(snap.source_to_text(cr), Some(TextByte(1)));
    }
}

#[test]
fn utf8_bom_offsets_match_legacy_affine_semantics() {
    // Regression: the retired affine shortcut still produced these results
    // for UTF-8; the composed map must agree byte-for-byte.
    let src = "\u{FEFF}one\r\ntwo\nthree\r\n";
    let snap = TextSnapshot::new(src.as_bytes()).unwrap();
    assert_eq!(snap.text_to_source(TextByte(0)), Some(SourceByte(3)));
    assert_eq!(snap.source_to_text(SourceByte(0)), None);
    assert_eq!(snap.source_to_text(SourceByte(3)), Some(TextByte(0)));
    // "one" ends at text 3 → source 6 (CR start); LF interior refused.
    assert_eq!(snap.text_to_source(TextByte(3)), Some(SourceByte(6)));
    assert_eq!(snap.source_to_text(SourceByte(7)), None);
    assert_eq!(snap.source_to_text(SourceByte(8)), Some(TextByte(4)));
}

#[test]
fn parser_output_equal_across_encodings_with_different_raw_offsets() {
    use flavite_syntax::pulldown_glue;
    let markdown = "# Title\n\nSee [[Target]] and https://example.com\n";
    let utf8_bytes = markdown.as_bytes().to_vec();
    let utf16_bytes: Vec<u8> = {
        let mut v = vec![0xFF, 0xFE];
        for unit in markdown.encode_utf16() {
            v.extend_from_slice(&unit.to_le_bytes());
        }
        v
    };
    let p8 = pulldown_glue::parse_note(&utf8_bytes).unwrap();
    let p16 = pulldown_glue::parse_note(&utf16_bytes).unwrap();
    // Semantically equal parses over identical normalized text…
    assert_eq!(p8.snapshot.text(), p16.snapshot.text());
    assert_eq!(
        p8.links.iter().map(|l| l.span).collect::<Vec<_>>(),
        p16.links.iter().map(|l| l.span).collect::<Vec<_>>()
    );
    // …but the raw source coordinates and hashes MUST differ (§6.10: FR-24).
    let link8 = p8.links[0].span;
    let s8 = p8.snapshot.span_to_source(link8).unwrap();
    let s16 = p16.snapshot.span_to_source(link8).unwrap();
    assert_ne!(s8, s16, "encoded spans differ");
    // Each maps to its own bytes: [[Target]] in both.
    let raw8 = &p8.snapshot.source()[s8.start.0..s8.end.0];
    let raw16 = &p16.snapshot.source()[s16.start.0..s16.end.0];
    let decoded8 = String::from_utf8(raw8.to_vec()).unwrap();
    let decoded16 = flavite_encoding::decode(
        raw16,
        DecodePolicy::Explicit(SourceFormat::utf16le()),
        &flavite_encoding::DecodeLimits::default(),
        &flavite_encoding::NOT_CANCELLED,
    )
    .unwrap();
    assert_eq!(decoded8, decoded16.text(), "same link target bytes");
    assert_ne!(
        blake3_hash(p8.snapshot.source()),
        blake3_hash(p16.snapshot.source())
    );
}

fn blake3_hash(bytes: &[u8]) -> [u8; 32] {
    // Cheap distinctness witness without adding a dev-dependency: the
    // encodings are structurally different, any injective digest suffices.
    let mut h = [0u8; 32];
    for (i, b) in bytes.iter().enumerate() {
        h[i % 32] = h[i % 32].rotate_left(3) ^ b;
    }
    h
}

#[test]
fn span_to_source_endpoints_match_text_to_source() {
    let src = "a\r\nb";
    let snap = TextSnapshot::new(src.as_bytes()).unwrap();
    // Text "a\nb": span endpoints map exactly like text_to_source; a span
    // starting at the folded LF (text 1) legitimately covers the physical
    // CRLF pair in the source (§6.10: begin/end of CRLF are legal).
    assert_eq!(
        snap.span_to_source(Span::new(TextByte(1), TextByte(1))),
        Some(Span::new(SourceByte(1), SourceByte(1)))
    );
    assert_eq!(
        snap.span_to_source(Span::new(TextByte(1), TextByte(2))),
        Some(Span::new(SourceByte(1), SourceByte(3))),
        "span covering the folded LF includes both CRLF bytes"
    );
    assert_eq!(
        snap.span_to_source(Span::new(TextByte(0), TextByte(3))),
        Some(Span::new(SourceByte(0), SourceByte(4)))
    );
    // Illegal interiors still refuse (inside the emoji on both sides).
    let wide = TextSnapshot::new("x\u{1F600}y".as_bytes()).unwrap();
    assert_eq!(wide.span_to_source(Span::new(TextByte(2), TextByte(3))), None);
}

#[test]
fn empty_and_bom_only_files_map_eof() {
    for policy in [
        DecodePolicy::Auto,
        DecodePolicy::Explicit(SourceFormat::utf16be_with_bom()),
    ] {
        let bytes = match policy {
            DecodePolicy::Auto => Vec::new(),
            DecodePolicy::Explicit(_) => vec![0xFE, 0xFF],
        };
        let snap = TextSnapshot::with_policy(&bytes, policy).unwrap();
        assert_eq!(snap.text(), "");
        assert_eq!(snap.line_count(), 1);
        assert_eq!(snap.lines()[0].eol, Eol::None);
        assert_eq!(
            snap.text_to_source(TextByte(0)),
            Some(SourceByte(snap.source().len()))
        );
        assert_eq!(
            snap.source_to_text(SourceByte(snap.source().len())),
            Some(TextByte(0))
        );
        assert_eq!(snap.source(), bytes);
    }
}

#[test]
fn dense_newline_utf16_lines_walk_monotonically() {
    // 1000 empty CRLF lines: the single-pass builder must land every source
    // boundary exactly (each CRLF = 4 UTF-16 bytes).
    let src = "\r\n".repeat(1000);
    let bytes: Vec<u8> = {
        let mut v = Vec::new();
        for unit in src.encode_utf16() {
            v.extend_from_slice(&unit.to_le_bytes());
        }
        v
    };
    let snap =
        TextSnapshot::with_policy(&bytes, DecodePolicy::Explicit(SourceFormat::utf16le())).unwrap();
    assert_eq!(snap.line_count(), 1000);
    for (i, line) in snap.lines().iter().enumerate() {
        // Empty content: source_start == source_end at the CR unit start.
        assert_eq!(line.source_start, SourceByte(i * 4));
        assert_eq!(line.source_end, SourceByte(i * 4));
        assert_eq!(line.eol, Eol::Crlf);
        assert_eq!(line.text_start, TextByte(i));
        assert_eq!(line.text_end, TextByte(i)); // empty content
    }
    assert_eq!(snap.source(), bytes);
}

#[test]
fn from_normalized_stays_a_synthetic_view() {
    let snap = TextSnapshot::from_normalized("a\nb");
    assert_eq!(snap.format(), None, "no disk-decoding claim");
    assert!(!snap.has_bom());
    assert!(snap.map().is_none());
    assert_eq!(snap.text_to_source(TextByte(2)), Some(SourceByte(2)));
    // A leading BOM *character* is content here and must survive.
    let snap = TextSnapshot::from_normalized("\u{FEFF}x");
    assert_eq!(snap.text(), "\u{FEFF}x");
    assert_eq!(snap.source(), "\u{FEFF}x".as_bytes());
}
