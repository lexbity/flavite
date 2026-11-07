//! Decode-map properties: total/invertible boundary mapping, rejection of
//! illegal interiors, sparse-checkpoint edge sizes and an independent
//! oracle comparison over generated UTF-16 sources (RFC-0006 S00, NFR-18).

use std::collections::BTreeSet;

use flavite_encoding::{
    CancelToken, DecodeLimits, DecodePolicy, FileEncoding, SourceFormat, decode, encode,
};
use proptest::prelude::*;

fn decode_explicit(bytes: &[u8], format: SourceFormat) -> flavite_encoding::DecodedContent {
    decode(
        bytes,
        DecodePolicy::Explicit(format),
        &DecodeLimits::default(),
        &CancelToken::new(),
    )
    .expect("decodes")
}

fn utf16le(format_has_bom: bool) -> SourceFormat {
    SourceFormat::new(FileEncoding::Utf16Le, format_has_bom).unwrap()
}

#[test]
fn enumerates_all_byte_positions_of_small_lebe_inputs() {
    for text in [
        "",
        "a",
        "ab\r\ncd",
        "\u{0800}",
        "e\u{0301}x",
        "a\u{1F600}b",
        "cr\rstandalone",
        "\u{FEFF}bom-content",
    ] {
        for (format, bom) in [(utf16le(true), true), (utf16le(false), false)] {
            let mut bytes = Vec::new();
            if bom {
                bytes.extend_from_slice(&[0xFF, 0xFE]);
            }
            for unit in text.encode_utf16() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            let decoded = decode_explicit(&bytes, format);
            let map = decoded.map();

            // Independent oracle: legal source boundaries and their decoded
            // partners, built from the text itself.
            let mut legal_source: BTreeSet<usize> = BTreeSet::new();
            let mut legal_decoded: BTreeSet<usize> = BTreeSet::new();
            let bom_len = if bom { 2 } else { 0 };
            let mut unit_index = 0usize;
            let mut decoded_byte = 0usize;
            for ch in text.chars() {
                legal_source.insert(bom_len + 2 * unit_index);
                legal_decoded.insert(decoded_byte);
                unit_index += ch.len_utf16();
                decoded_byte += ch.len_utf8();
            }
            legal_source.insert(bytes.len()); // EOF
            legal_decoded.insert(decoded.text().len());

            for sb in 0..=bytes.len() {
                let mapped = map.source_to_decoded(sb);
                assert_eq!(
                    mapped.is_some(),
                    legal_source.contains(&sb),
                    "source {sb} of {text:?} (bom {bom})"
                );
                if let Some(db) = mapped {
                    assert_eq!(
                        map.decoded_to_source(db),
                        Some(sb),
                        "inversion failed at source {sb} of {text:?} (bom {bom})"
                    );
                }
            }
            for db in 0..=decoded.text().len() {
                let mapped = map.decoded_to_source(db);
                assert_eq!(
                    mapped.is_some(),
                    legal_decoded.contains(&db),
                    "decoded {db} of {text:?} (bom {bom})"
                );
                if let Some(sb) = mapped {
                    assert_eq!(
                        map.source_to_decoded(sb),
                        Some(db),
                        "inversion failed at decoded {db} of {text:?} (bom {bom})"
                    );
                }
            }
        }
    }
}

#[test]
fn sparse_checkpoint_edges_at_254_255_and_256_scalars() {
    for scalar_count in [254usize, 255, 256, 511, 512] {
        // ASCII-range scalars keep the walk cheap, and a supplementary
        // scalar exercises pair-sized checkpoints at the end.
        let mut text = "x".repeat(scalar_count.saturating_sub(1));
        text.push('\u{1F600}');
        let format = utf16le(true);
        let mut bytes = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let decoded = decode_explicit(&bytes, format);
        let map = decoded.map();

        // Every scalar boundary inverts exactly through the sparse table.
        let mut decoded_byte = 0usize;
        for (content_end, ch) in text.char_indices() {
            let source = 2 + 2 * text[..content_end].encode_utf16().count();
            let mapped = map.source_to_decoded(source).expect("legal boundary");
            assert_eq!(mapped, decoded_byte);
            assert_eq!(map.decoded_to_source(decoded_byte), Some(source));
            decoded_byte += ch.len_utf8();
        }
        assert_eq!(map.source_to_decoded(bytes.len()), Some(decoded_byte));
        assert_eq!(map.decoded_to_source(decoded_byte), Some(bytes.len()));
    }
}

#[test]
fn standalone_cr_and_content_bom_map_before_lf_and_eof() {
    let text = "\u{FEFF}a\r\nb\r";
    let format = utf16le(true);
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let decoded = decode_explicit(&bytes, format);
    let map = decoded.map();
    assert_eq!(decoded.text(), text, "codec preserves CR and content BOM");
    // The decoded position of the trailing CR maps back to its pair.
    let trailing_cr = decoded.text().len() - 1;
    let source = map.decoded_to_source(trailing_cr).expect("CR boundary");
    assert_eq!(map.source_to_decoded(source), Some(trailing_cr));
}

proptest! {
    #[test]
    fn utf16_map_matches_the_independent_oracle(text in arb_text()) {
        let format = utf16le(true);
        let mut bytes = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let decoded = decode_explicit(&bytes, format);
        let map = decoded.map();

        let mut legal_source = BTreeSet::new();
        let mut legal_decoded = BTreeSet::new();
        let mut unit_index = 0usize;
        let mut decoded_byte = 0usize;
        for ch in text.chars() {
            legal_source.insert(2 + 2 * unit_index);
            legal_decoded.insert(decoded_byte);
            unit_index += ch.len_utf16();
            decoded_byte += ch.len_utf8();
        }
        legal_source.insert(bytes.len());
        legal_decoded.insert(decoded.text().len());

        for sb in 0..=bytes.len() {
            let mapped = map.source_to_decoded(sb);
            prop_assert_eq!(mapped.is_some(), legal_source.contains(&sb));
            if let Some(db) = mapped {
                prop_assert_eq!(map.decoded_to_source(db), Some(sb));
            }
        }
        for db in 0..=decoded.text().len() {
            let mapped = map.decoded_to_source(db);
            prop_assert_eq!(mapped.is_some(), legal_decoded.contains(&db));
            if let Some(sb) = mapped {
                prop_assert_eq!(map.source_to_decoded(sb), Some(db));
            }
        }
    }

    #[test]
    fn utf8_map_is_identity_past_the_signature(text in arb_text()) {
        let format = SourceFormat::new(FileEncoding::Utf8, true).unwrap();
        let bytes = encode(
            &text,
            format,
            &flavite_encoding::EncodeLimits::default(),
            &CancelToken::new(),
        )
        .unwrap();
        let decoded = decode_explicit(&bytes, format);
        let map = decoded.map();
        prop_assert_eq!(map.bom_len(), 3);

        // Identity arithmetic past the signature, restricted to the legal
        // boundary domain: scalar starts plus EOF; UTF-8 continuation
        // interiors are refused on both sides (§6.10), mirroring the
        // UTF-16 oracle above.
        let mut legal_decoded = BTreeSet::new();
        let mut decoded_byte = 0usize;
        for ch in text.chars() {
            legal_decoded.insert(decoded_byte);
            decoded_byte += ch.len_utf8();
        }
        legal_decoded.insert(text.len());
        for db in 0..=text.len() {
            let mapped = map.decoded_to_source(db);
            prop_assert_eq!(mapped.is_some(), legal_decoded.contains(&db));
            if let Some(sb) = mapped {
                prop_assert_eq!(sb, db + 3);
                prop_assert_eq!(map.source_to_decoded(sb), Some(db));
            }
        }
        for sb in 0..3usize {
            prop_assert_eq!(map.source_to_decoded(sb), None, "BOM interior");
        }
    }
}

fn arb_text() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..=80).prop_map(|chars| chars.into_iter().collect())
}
