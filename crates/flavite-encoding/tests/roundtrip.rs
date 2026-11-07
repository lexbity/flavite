//! Byte-exact round trips, explicit-policy semantics, malformed input
//! classes, limits and streaming split points (RFC-0006 S00: §6.10
//! verification envelope).

use flavite_encoding::{
    CancelToken, CodecError, CodecLimitKind, DecodeLimits, DecodePolicy, EncodeLimits,
    FileEncoding, SourceFormat, StreamingDecoder, StreamingEncoder, decode, encode,
};
use proptest::prelude::*;

fn auto(source: &[u8]) -> Result<flavite_encoding::DecodedContent, CodecError> {
    decode(
        source,
        DecodePolicy::Auto,
        &DecodeLimits::default(),
        &CancelToken::new(),
    )
}

fn enc(text: &str, format: SourceFormat) -> Result<Vec<u8>, CodecError> {
    encode(text, format, &EncodeLimits::default(), &CancelToken::new())
}

/// Known independent byte fixtures (hand-assembled, not produced by the
/// codec under test).
fn utf16le_fixture() -> Vec<u8> {
    // BOM + "héllo\r\n" + U+1F600 + "\n"
    let mut bytes = vec![0xFF, 0xFE];
    let push = |unit: u16, bytes: &mut Vec<u8>| bytes.extend_from_slice(&unit.to_le_bytes());
    for ch in "héllo\r\n\u{1F600}\n".chars() {
        let mut units = [0u16; 2];
        for unit in ch.encode_utf16(&mut units) {
            push(*unit, &mut bytes);
        }
    }
    bytes
}

fn utf16be_fixture() -> Vec<u8> {
    let mut bytes = vec![0xFE, 0xFF];
    for ch in "wörld\r\n".chars() {
        let mut units = [0u16; 2];
        for unit in ch.encode_utf16(&mut units) {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
    }
    bytes
}

#[test]
fn known_independent_fixtures_round_trip_byte_exactly() {
    for (bytes, encoding) in [
        (utf16le_fixture(), FileEncoding::Utf16Le),
        (utf16be_fixture(), FileEncoding::Utf16Be),
    ] {
        let decoded = auto(&bytes).expect("decodes");
        assert_eq!(decoded.format().encoding(), encoding);
        assert!(decoded.format().bom());
        assert_eq!(enc(decoded.text(), decoded.format()).unwrap(), bytes);
    }
}

#[test]
fn empty_and_bom_only_files_round_trip() {
    for bytes in [
        Vec::new(),
        vec![0xEF, 0xBB, 0xBF],
        vec![0xFF, 0xFE],
        vec![0xFE, 0xFF],
    ] {
        let decoded = auto(&bytes).expect("decodes");
        assert_eq!(decoded.text(), "");
        assert_eq!(enc("", decoded.format()).unwrap(), bytes);
    }
}

#[test]
fn mixed_eol_and_standalone_cr_survive_every_format() {
    let text = "one\r\ntwo\nthree\rfour";
    for format in [
        SourceFormat::new(FileEncoding::Utf8, false).unwrap(),
        SourceFormat::new(FileEncoding::Utf8, true).unwrap(),
        SourceFormat::new(FileEncoding::Utf16Le, false).unwrap(),
        SourceFormat::new(FileEncoding::Utf16Be, true).unwrap(),
    ] {
        let bytes = enc(text, format).unwrap();
        let decoded = decode(
            &bytes,
            DecodePolicy::Explicit(format),
            &DecodeLimits::default(),
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(decoded.text(), text, "no EOL normalization for {format:?}");
    }
}

#[test]
fn auto_treats_bom_less_bytes_as_utf8_even_when_all_ascii() {
    let decoded = auto(b"ascii only").unwrap();
    let utf8 = SourceFormat::new(FileEncoding::Utf8, false).unwrap();
    assert_eq!(decoded.format(), utf8, "ASCII is never auto-selected");
}

#[test]
fn explicit_bom_les_utf8_preserves_leading_signature_shaped_bytes() {
    // BOM-less explicit policy: EF BB BF is content U+FEFF, not a signature.
    let format = SourceFormat::new(FileEncoding::Utf8, false).unwrap();
    let bytes = [0xEF, 0xBB, 0xBF, b'a'];
    let decoded = decode(
        &bytes,
        DecodePolicy::Explicit(format),
        &DecodeLimits::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(decoded.text(), "\u{FEFF}a");
    assert_eq!(enc(decoded.text(), format).unwrap(), bytes);
}

#[test]
fn explicit_utf16le_interprets_a_valid_leading_nul() {
    // FF FE 00 00 is ambiguous under Auto but decodable under explicit LE.
    let format = SourceFormat::new(FileEncoding::Utf16Le, false).unwrap();
    let decoded = decode(
        &[0xFF, 0xFE, 0x00, 0x00],
        DecodePolicy::Explicit(format),
        &DecodeLimits::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(decoded.text(), "\u{FEFF}\u{0}");
}

#[test]
fn auto_diagnoses_longer_unsupported_signatures() {
    let sigs: [&'static [u8]; 6] = [
        &[0xFF, 0xFE, 0x00, 0x00], // UTF-32LE
        &[0x00, 0x00, 0xFE, 0xFF], // UTF-32BE
        &[0xF7, 0x64, 0x4C],       // UTF-1
        &[0xDD, 0x76, 0x67],       // UTF-EBCDIC
        &[0x0E, 0xFE, 0xFF],       // SCSU
        &[0x2B, 0x2F, 0x76],       // UTF-7
    ];
    for sig in sigs {
        assert_eq!(
            auto(sig).expect_err("diagnosed"),
            CodecError::UnsupportedOrAmbiguousBom,
            "signature {sig:?}"
        );
    }
}

#[test]
fn malformed_classes_are_typed_and_positioned() {
    // Truncated UTF-8 continuation (€ cut short) at byte 0.
    assert_eq!(
        auto(&[0xE2, 0x82]).expect_err("truncated"),
        CodecError::InvalidUtf8 { source_byte: 0 }
    );
    // Lone continuation byte.
    assert_eq!(
        auto(&[b'a', 0x80]).expect_err("lone continuation"),
        CodecError::InvalidUtf8 { source_byte: 1 }
    );
    // UTF-8-encoded surrogate lead ED A0 80 after a BOM: position includes
    // the signature.
    assert_eq!(
        auto(&[0xEF, 0xBB, 0xBF, 0xED, 0xA0, 0x80]).expect_err("surrogate scalar"),
        CodecError::InvalidUtf8 { source_byte: 3 }
    );
    // Odd UTF-16 byte count after the signature.
    assert_eq!(
        auto(&[0xFF, 0xFE, 0x41, 0x00, 0x42]).expect_err("odd count"),
        CodecError::OddUtf16ByteCount
    );
    // Trailing high surrogate: error points at its source byte.
    assert_eq!(
        auto(&[0xFF, 0xFE, 0x3D, 0xD8]).expect_err("dangling high"),
        CodecError::InvalidUtf16 { source_byte: 2 }
    );
    // Lone low surrogate.
    assert_eq!(
        auto(&[0xFF, 0xFE, 0x00, 0xDE]).expect_err("lone low"),
        CodecError::InvalidUtf16 { source_byte: 2 }
    );
}

#[test]
fn decode_failure_is_never_lossy() {
    // Malformed input never yields U+FFFD content: it fails or decodes
    // strictly.
    let cases: [&'static [u8]; 3] = [
        &[0xFF, 0xFE, 0x00, 0xD8, 0x41, 0x00],
        &[0xE2, 0x82],
        &[b'a', 0xFF],
    ];
    for bytes in cases {
        if let Ok(decoded) = auto(bytes) {
            let has_replacement = decoded.text().contains('\u{FFFD}');
            assert!(!has_replacement);
        }
    }
}

#[test]
fn explicit_bom_conflicts() {
    let be = SourceFormat::new(FileEncoding::Utf16Be, true).unwrap();
    assert_eq!(
        decode(
            &[0xFF, 0xFE, 0x41, 0x00],
            DecodePolicy::Explicit(be),
            &DecodeLimits::default(),
            &CancelToken::new(),
        )
        .expect_err("wrong signature"),
        CodecError::BomConflict
    );
    let utf8 = SourceFormat::new(FileEncoding::Utf8, true).unwrap();
    assert_eq!(
        decode(
            b"no signature here",
            DecodePolicy::Explicit(utf8),
            &DecodeLimits::default(),
            &CancelToken::new(),
        )
        .expect_err("missing signature"),
        CodecError::BomConflict
    );
}

#[test]
fn strict_ascii_output_refuses_the_first_non_ascii_byte() {
    let ascii = SourceFormat::new(FileEncoding::Ascii, false).unwrap();
    assert_eq!(
        enc("oké", ascii).expect_err("unencodable"),
        CodecError::Unencodable { text_byte: 2 }
    );
    // Supplementary and unpaired-range characters are never substituted.
    let decoded = auto("plain".as_bytes()).unwrap();
    assert_eq!(enc(decoded.text(), ascii).unwrap(), b"plain");
}

#[test]
fn every_limit_refuses_before_effects() {
    // Raw limit.
    let limits = DecodeLimits {
        max_raw_bytes: 2,
        ..DecodeLimits::default()
    };
    assert!(matches!(
        decode(
            b"too long",
            DecodePolicy::Auto,
            &limits,
            &CancelToken::new()
        ),
        Err(CodecError::LimitExceeded {
            kind: CodecLimitKind::RawBytes,
            ..
        })
    ));

    // Decoded limit on UTF-16 expansion (BMP scalars grow 2 → 3 bytes).
    let format = SourceFormat::new(FileEncoding::Utf16Le, false).unwrap();
    let text = "\u{0800}".repeat(4); // 4×2 units = 8 bytes; 12 decoded bytes
    let bytes = enc(&text, format).unwrap();
    let limits = DecodeLimits {
        max_decoded_utf8_bytes: 10,
        ..DecodeLimits::default()
    };
    assert!(matches!(
        decode(
            &bytes,
            DecodePolicy::Explicit(format),
            &limits,
            &CancelToken::new()
        ),
        Err(CodecError::LimitExceeded {
            kind: CodecLimitKind::DecodedUtf8Bytes,
            ..
        })
    ));

    // Map budget on a UTF-16 source.
    let limits = DecodeLimits {
        max_map_bytes: 4,
        ..DecodeLimits::default()
    };
    assert!(matches!(
        decode(
            &bytes,
            DecodePolicy::Explicit(format),
            &limits,
            &CancelToken::new()
        ),
        Err(CodecError::LimitExceeded {
            kind: CodecLimitKind::MapBytes,
            ..
        })
    ));

    // Encode output limit: ASCII expands 2× into UTF-16.
    let limits = EncodeLimits {
        max_input_utf8_bytes: u64::MAX,
        max_output_bytes: 3,
    };
    assert!(matches!(
        encode("abcd", format, &limits, &CancelToken::new()),
        Err(CodecError::LimitExceeded {
            kind: CodecLimitKind::OutputBytes,
            ..
        })
    ));
}

#[test]
fn cancellation_fires_immediately_and_after_progress() {
    let token = CancelToken::new();
    token.cancel();
    assert_eq!(
        decode(b"a", DecodePolicy::Auto, &DecodeLimits::default(), &token).expect_err("cancelled"),
        CodecError::Cancelled
    );

    let mut decoder = StreamingDecoder::new(DecodePolicy::Auto, DecodeLimits::default());
    decoder
        .push(
            "repeat".as_bytes().repeat(200_000).as_slice(),
            &CancelToken::new(),
        )
        .unwrap();
    token.cancel();
    assert_eq!(
        decoder
            .push(b"more", &token)
            .expect_err("cancelled mid-stream"),
        CodecError::Cancelled
    );
}

#[test]
fn every_streaming_split_point_decodes_identically() {
    let fixtures: [(&[u8], DecodePolicy); 7] = [
        (b"plain ascii text\r\nsecond\n", DecodePolicy::Auto),
        (
            &[0xEF, 0xBB, 0xBF, b'a', b'\r', b'\n', b'b'],
            DecodePolicy::Auto,
        ),
        (&utf16le_fixture(), DecodePolicy::Auto),
        (&utf16be_fixture(), DecodePolicy::Auto),
        (&[0xFF, 0xFE, 0x00, 0xDE, 0x3D, 0xD8], DecodePolicy::Auto), // lone low
        (
            &[0xFF, 0xFE, 0x41, 0x00],
            DecodePolicy::Explicit(SourceFormat::new(FileEncoding::Utf16Le, true).unwrap()),
        ),
        (
            &[0xEF, 0xBB, 0xBF, 0x61],
            DecodePolicy::Explicit(SourceFormat::new(FileEncoding::Utf8, false).unwrap()),
        ),
    ];
    for (bytes, policy) in fixtures {
        let one_shot = decode(bytes, policy, &DecodeLimits::default(), &CancelToken::new());
        for split in 0..=bytes.len() {
            let mut decoder = StreamingDecoder::new(policy, DecodeLimits::default());
            // UTF-16 errors surface as soon as the invalid unit completes,
            // so capture errors wherever they appear.
            let mut push_err = None;
            if let Err(e) = decoder.push(&bytes[..split], &CancelToken::new()) {
                push_err = Some(e);
            }
            if push_err.is_none()
                && let Err(e) = decoder.push(&bytes[split..], &CancelToken::new())
            {
                push_err = Some(e);
            }
            let streamed = match push_err {
                Some(e) => Err(e),
                None => decoder.finish(),
            };
            match (&one_shot, &streamed) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(a.text(), b.text(), "split {split} of {policy:?}");
                    assert_eq!(a.format(), b.format(), "split {split} of {policy:?}");
                }
                (Err(a), Err(b)) => assert_eq!(a, b, "split {split} of {policy:?}"),
                _ => panic!("one-shot/streaming mismatch at split {split} of {policy:?}"),
            }
        }
    }
}

#[test]
fn streaming_encoder_matches_one_shot_at_char_boundaries() {
    let text = "héllo\r\n\u{1F600} wörld\r\nagain";
    for format in [
        SourceFormat::new(FileEncoding::Utf8, true).unwrap(),
        SourceFormat::new(FileEncoding::Utf16Le, false).unwrap(),
        SourceFormat::new(FileEncoding::Utf16Be, true).unwrap(),
    ] {
        let one_shot = enc(text, format).unwrap();
        // Push successive chunks split at every char boundary.
        let mut boundaries: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        boundaries.push(text.len());
        let mut encoder = StreamingEncoder::new(format, EncodeLimits::default());
        let mut out = Vec::new();
        for pair in boundaries.windows(2) {
            encoder
                .push(&text[pair[0]..pair[1]], &mut out, &CancelToken::new())
                .unwrap();
        }
        assert_eq!(
            encoder.finish().unwrap(),
            u64::try_from(one_shot.len()).unwrap()
        );
        assert_eq!(out, one_shot, "format {format:?}");
    }
}

proptest::proptest! {
    #[test]
    fn explicit_round_trips_preserve_text(text in arb_text()) {
        for format in [
            SourceFormat::new(FileEncoding::Utf8, false).unwrap(),
            SourceFormat::new(FileEncoding::Utf8, true).unwrap(),
            SourceFormat::new(FileEncoding::Utf16Le, false).unwrap(),
            SourceFormat::new(FileEncoding::Utf16Le, true).unwrap(),
            SourceFormat::new(FileEncoding::Utf16Be, false).unwrap(),
            SourceFormat::new(FileEncoding::Utf16Be, true).unwrap(),
        ] {
            let bytes = enc(&text, format).unwrap();
            let decoded = decode(
                &bytes,
                DecodePolicy::Explicit(format),
                &DecodeLimits::default(),
                &CancelToken::new(),
            )
            .unwrap();
            proptest::prop_assert_eq!(decoded.text(), text.as_str());
        }
    }

    #[test]
    fn auto_decode_reencodes_byte_exact(bytes in proptest::collection::vec(proptest::num::u8::ANY, 0..=200)) {
        if let Ok(decoded) = auto(&bytes) {
            let re = enc(decoded.text(), decoded.format()).unwrap();
            proptest::prop_assert_eq!(re, bytes, "Auto-decoded inputs are byte-exact");
            let has_replacement = decoded.text().contains('\u{FFFD}');
            prop_assert!(!has_replacement);
        }
    }
}

fn arb_text() -> impl proptest::strategy::Strategy<Value = String> {
    proptest::collection::vec(proptest::char::any(), 0..=120)
        .prop_map(|chars| chars.into_iter().collect())
}
