//! RFC-0006 S00 — buffer-level encoding round trips (AC-23).
//!
//! Open every supported descriptor through `TextSnapshot`, edit the middle of
//! the decoded layer, serialize with `try_to_source_bytes`, and prove that a
//! re-decode under the same descriptor yields the edited text with the BOM and
//! physical EOL bytes byte-exact (D-15, §6.10). Expected bytes are built with
//! an independent std-only reference encoder, not the codec under test.

use flavite_buffer::{Buffer, BufferManager, EditOp};
use flavite_encoding::{
    CodecError, DecodeLimits, DecodePolicy, EncodeLimits, FileEncoding, NOT_CANCELLED,
    SourceFormat, decode, encode,
};
use flavite_syntax::text::{Span, TextByte, TextSnapshot};

/// Independent std-only reference encoder for one descriptor. Used to derive
/// expected output bytes without going through `flavite_encoding::encode`.
fn reference_encode(text: &str, format: SourceFormat) -> Vec<u8> {
    let mut out = Vec::from(format.bom_bytes());
    match format.encoding() {
        FileEncoding::Utf8 | FileEncoding::Ascii => out.extend_from_slice(text.as_bytes()),
        FileEncoding::Utf16Le => {
            for unit in text.encode_utf16() {
                out.extend_from_slice(&unit.to_le_bytes());
            }
        }
        FileEncoding::Utf16Be => {
            for unit in text.encode_utf16() {
                out.extend_from_slice(&unit.to_be_bytes());
            }
        }
    }
    out
}

fn fold_crlf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// One open/edit/serialize/re-decode fixture.
struct Fixture {
    name: &'static str,
    /// Raw file bytes exactly as they would sit on disk.
    bytes: &'static [u8],
    /// Decode policy that produces this fixture's descriptor.
    policy: DecodePolicy,
    /// The descriptor the bytes decode under.
    format: SourceFormat,
    /// The decoded layer (physical EOLs, no BOM) of `bytes`.
    decoded: &'static str,
    /// Substring of `decoded` replaced by "X" for the mid-document edit.
    needle: &'static str,
}

fn fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            name: "ascii",
            bytes: b"plain ascii\n",
            policy: DecodePolicy::Auto,
            // ASCII-only Auto is UTF-8, never "strict ASCII" (locked D-15).
            format: SourceFormat::utf8(),
            decoded: "plain ascii\n",
            needle: "ascii",
        },
        Fixture {
            name: "utf8 multibyte",
            bytes: "héllo 雪\nsecond line\n".as_bytes(),
            policy: DecodePolicy::Auto,
            format: SourceFormat::utf8(),
            decoded: "héllo 雪\nsecond line\n",
            needle: "second",
        },
        Fixture {
            name: "utf8+bom",
            bytes: b"\xef\xbb\xbfbom \xc3\xa9text\r\nnext\r\n",
            policy: DecodePolicy::Auto,
            format: SourceFormat::utf8_with_bom(),
            decoded: "bom étext\r\nnext\r\n",
            needle: "text",
        },
        Fixture {
            name: "utf16le+bom",
            bytes: &[
                0xFF, 0xFE, b'A', 0, 0x3D, 0xD8, 0x00, 0xDE, b'\r', 0, b'\n', 0, b's', 0, b'e', 0,
                b'c', 0, 0x6F, 0, b'n', 0, b'd', 0, b'\n', 0,
            ],
            policy: DecodePolicy::Auto,
            format: SourceFormat::utf16le_with_bom(),
            decoded: "A😀\r\nsecond\n",
            needle: "second",
        },
        Fixture {
            name: "utf16be+bom",
            bytes: &[
                0xFE, 0xFF, 0x00, b'A', 0xD8, 0x3D, 0xDE, 0x00, 0x00, b'\r', 0x00, b'\n', 0x00,
                b'B', 0x00, b'\n',
            ],
            policy: DecodePolicy::Auto,
            format: SourceFormat::utf16be_with_bom(),
            decoded: "A😀\r\nB\n",
            needle: "B",
        },
        Fixture {
            name: "utf16le bom-less",
            bytes: &[
                b'o', 0, b'n', 0, b'e', 0, b'\n', 0, b't', 0, b'w', 0, b'o', 0,
            ],
            policy: DecodePolicy::Explicit(SourceFormat::utf16le()),
            format: SourceFormat::utf16le(),
            decoded: "one\ntwo",
            needle: "two",
        },
        Fixture {
            name: "crlf mix utf8",
            bytes: b"one\r\ntwo\nthree\r\n",
            policy: DecodePolicy::Auto,
            format: SourceFormat::utf8(),
            decoded: "one\r\ntwo\nthree\r\n",
            needle: "two",
        },
        Fixture {
            name: "standalone CR utf8",
            bytes: b"one\rtwo\n",
            policy: DecodePolicy::Auto,
            format: SourceFormat::utf8(),
            decoded: "one\rtwo\n",
            needle: "two",
        },
        Fixture {
            name: "crlf mix utf16be+bom",
            bytes: &[
                0xFE, 0xFF, 0x00, b'o', 0x00, b'n', 0x00, b'e', 0x00, b'\r', 0x00, b'\n', 0x00,
                b't', 0x00, b'w', 0x00, b'o', 0x00, b'\n', 0x00, b't', 0x00, b'h', 0x00, b'r',
                0x00, b'e', 0x00, b'e', 0x00, b'\r', 0x00, b'\n',
            ],
            policy: DecodePolicy::Auto,
            format: SourceFormat::utf16be_with_bom(),
            decoded: "one\r\ntwo\nthree\r\n",
            needle: "three",
        },
    ]
}

/// Edit the middle of the decoded layer, serialize, and re-decode.
#[test]
fn open_edit_serialize_redecode_round_trips_every_format() {
    for fixture in fixtures() {
        let snapshot =
            TextSnapshot::with_policy(fixture.bytes, fixture.policy).expect("fixture decodes");
        assert_eq!(
            snapshot.format(),
            Some(fixture.format),
            "fixture {} descriptor",
            fixture.name
        );
        let mut buffer = Buffer::from_snapshot(snapshot);
        assert_eq!(
            buffer.model().source_format(),
            fixture.format,
            "fixture {} model descriptor",
            fixture.name
        );

        // The unedited buffer re-serializes byte-exactly.
        assert_eq!(
            buffer.model().try_to_source_bytes().expect("re-encode"),
            fixture.bytes,
            "fixture {} unedited serialization",
            fixture.name
        );

        // One edit in the middle of the decoded (normalized) text.
        let text = buffer.model().text().to_owned();
        let start = text.find(fixture.needle).expect("needle in text");
        let end = start + fixture.needle.len();
        let revision_before = buffer.revision();
        let (revision, _transition) = buffer
            .apply_local_edits(&[EditOp::new(
                Span::new(TextByte(start), TextByte(end)),
                "X".to_owned(),
            )])
            .expect("edit is valid");
        assert!(revision > revision_before);

        // Expected outcomes, derived independently of the codec.
        let expected_decoded = fixture.decoded.replacen(fixture.needle, "X", 1);
        let expected_bytes = reference_encode(&expected_decoded, fixture.format);
        let expected_normalized = fold_crlf(&expected_decoded);

        // The normalized layer saw exactly the edit.
        assert_eq!(
            buffer.text(),
            expected_normalized,
            "fixture {} text",
            fixture.name
        );

        // Serialization preserves descriptor, BOM and physical EOL bytes.
        let serialized = buffer
            .model()
            .try_to_source_bytes()
            .unwrap_or_else(|e| panic!("fixture {} encode failed: {e}", fixture.name));
        assert_eq!(
            serialized, expected_bytes,
            "fixture {} edited serialization",
            fixture.name
        );
        assert!(
            serialized.starts_with(fixture.format.bom_bytes()),
            "fixture {} BOM bytes preserved",
            fixture.name
        );

        // Re-decode under the same descriptor: the edited text round-trips.
        let redecoded = decode(
            &serialized,
            fixture.policy,
            &DecodeLimits::default(),
            &NOT_CANCELLED,
        )
        .unwrap_or_else(|e| panic!("fixture {} re-decode failed: {e}", fixture.name));
        assert_eq!(
            redecoded.text(),
            expected_decoded,
            "fixture {} re-decode",
            fixture.name
        );
        assert_eq!(
            redecoded.format(),
            fixture.format,
            "fixture {} format kept",
            fixture.name
        );
    }
}

/// RFC golden bytes (AC-23): `A😀\r\n` under both UTF-16 byte orders with
/// BOM, asserted against independent hardcoded constants.
#[test]
fn golden_utf16_bytes_for_supplementary_scalar() {
    const LE_WITH_BOM: &[u8] = &[
        0xFF, 0xFE, 0x41, 0x00, 0x3D, 0xD8, 0x00, 0xDE, 0x0D, 0x00, 0x0A, 0x00,
    ];
    const BE_WITH_BOM: &[u8] = &[
        0xFE, 0xFF, 0x00, 0x41, 0xD8, 0x3D, 0xDE, 0x00, 0x00, 0x0D, 0x00, 0x0A,
    ];
    let text = "A😀\r\n";
    let le = encode(
        text,
        SourceFormat::utf16le_with_bom(),
        &EncodeLimits::default(),
        &NOT_CANCELLED,
    )
    .expect("LE encode");
    assert_eq!(le, LE_WITH_BOM);
    let be = encode(
        text,
        SourceFormat::utf16be_with_bom(),
        &EncodeLimits::default(),
        &NOT_CANCELLED,
    )
    .expect("BE encode");
    assert_eq!(be, BE_WITH_BOM);

    // And the golden bytes decode back to exactly the source text.
    for (bytes, format) in [
        (LE_WITH_BOM, SourceFormat::utf16le_with_bom()),
        (BE_WITH_BOM, SourceFormat::utf16be_with_bom()),
    ] {
        let decoded = decode(
            bytes,
            DecodePolicy::Auto,
            &DecodeLimits::default(),
            &NOT_CANCELLED,
        )
        .expect("golden decodes");
        assert_eq!(decoded.text(), text);
        assert_eq!(decoded.format(), format);
    }
}

/// Format-only conversion is one isolated revision-advancing Undo boundary:
/// undo restores the OLD descriptor as well as the text; redo re-lands it.
#[test]
fn format_only_convert_is_isolated_undo_boundary() {
    let mut buffer = Buffer::from_text("hello world\n");
    let r0 = buffer.revision();
    assert_eq!(buffer.model().source_format(), SourceFormat::utf8());

    let r1 = buffer
        .model_mut()
        .try_convert_source_format(SourceFormat::utf16le_with_bom())
        .expect("utf16 conversion");
    assert!(r1 > r0, "conversion advances the revision");
    assert_eq!(buffer.text(), "hello world\n", "text unchanged");
    assert_eq!(
        buffer.model().source_format(),
        SourceFormat::utf16le_with_bom()
    );
    let converted = buffer.model().try_to_source_bytes().expect("utf16 encode");
    assert_eq!(
        converted,
        reference_encode("hello world\n", SourceFormat::utf16le_with_bom())
    );

    // Undo restores the old descriptor too (FR-25).
    let r2 = buffer.model_mut().undo().expect("undo of format boundary");
    assert!(r2 > r1, "undo advances the revision");
    assert_eq!(buffer.model().source_format(), SourceFormat::utf8());
    assert_eq!(
        buffer.model().try_to_source_bytes().expect("utf8 encode"),
        b"hello world\n"
    );

    // Redo flips the format back on.
    let r3 = buffer.model_mut().redo().expect("redo of format boundary");
    assert!(r3 > r2);
    assert_eq!(
        buffer.model().source_format(),
        SourceFormat::utf16le_with_bom()
    );
    assert_eq!(
        buffer.model().try_to_source_bytes().expect("utf16 encode"),
        converted
    );
}

/// A synthetic normalized view can never reset a live disk descriptor, and a
/// pending-draft descriptor override is adopted only through an explicit view.
#[test]
fn normalized_views_do_not_reset_live_descriptor() {
    let mut buffer = Buffer::from_snapshot(
        TextSnapshot::with_policy(&[0xFF, 0xFE, b'o', 0, b'k', 0], DecodePolicy::Auto)
            .expect("utf16le+bom"),
    );
    assert_eq!(
        buffer.model().source_format(),
        SourceFormat::utf16le_with_bom()
    );

    // A plain synthetic view carries no format authority: the live
    // descriptor survives the snapshot replacement.
    buffer
        .model_mut()
        .replace_snapshot(TextSnapshot::from_normalized("edited draft"));
    assert_eq!(
        buffer.model().source_format(),
        SourceFormat::utf16le_with_bom(),
        "synthetic view must not reset the descriptor"
    );

    // An explicitly tagged draft view does carry its save-format intent.
    buffer
        .model_mut()
        .replace_snapshot(TextSnapshot::from_normalized_with_descriptor(
            "tagged",
            SourceFormat::ascii(),
        ));
    assert_eq!(buffer.model().source_format(), SourceFormat::ascii());
}

/// Strict-ASCII conversion of non-ASCII content is refused before any state
/// change; the draft stays intact.
#[test]
fn unencodable_ascii_conversion_leaves_draft_intact() {
    let mut buffer = Buffer::from_text("emoji 😀 here\n");
    let revision = buffer.revision();
    let error = buffer
        .model_mut()
        .try_convert_source_format(SourceFormat::ascii())
        .expect_err("emoji is unencodable as strict ASCII");
    assert!(
        matches!(error, CodecError::Unencodable { .. }),
        "typed unencodable error, got {error:?}"
    );
    assert_eq!(buffer.text(), "emoji 😀 here\n", "content intact");
    assert_eq!(buffer.model().source_format(), SourceFormat::utf8());
    assert_eq!(
        buffer.revision(),
        revision,
        "refused conversion changes nothing"
    );
}

/// The manager's no-empty-fallback contract: malformed bytes produce a typed
/// codec error and never create a buffer; valid UTF-16LE+BOM opens under Auto.
#[test]
fn manager_never_creates_a_buffer_from_malformed_bytes() {
    let mut manager = BufferManager::new();
    let path = std::path::PathBuf::from("/vault/notes/broken.md");

    // Odd UTF-16 byte count under a recognized LE BOM.
    let error = manager
        .open_note(path.clone(), b"\xff\xfe\x00", DecodePolicy::Auto)
        .expect_err("truncated code unit is malformed");
    assert_eq!(error, CodecError::OddUtf16ByteCount);
    assert!(
        manager.get_buffer(&path).is_none(),
        "no empty fallback buffer"
    );

    // Invalid UTF-8 is a typed error with its source position.
    let error = manager
        .open_note(path.clone(), b"hello \xff world", DecodePolicy::Auto)
        .expect_err("invalid UTF-8 is malformed");
    assert!(matches!(error, CodecError::InvalidUtf8 { .. }));
    assert!(manager.get_buffer(&path).is_none());

    // The documented FF FE 00 00 ambiguity is refused, never guessed.
    let error = manager
        .open_note(path.clone(), b"\xff\xfe\x00\x00A", DecodePolicy::Auto)
        .expect_err("UTF-32-shaped signature is ambiguous");
    assert_eq!(error, CodecError::UnsupportedOrAmbiguousBom);
    assert!(manager.get_buffer(&path).is_none());

    // Valid UTF-16LE BOM bytes open cleanly under Auto.
    let bytes = reference_encode("external\n", SourceFormat::utf16le_with_bom());
    let (buffer, _transition) = manager
        .open_note(path.clone(), &bytes, DecodePolicy::Auto)
        .expect("BOM recognized");
    assert_eq!(buffer.text(), "external\n");
    assert_eq!(
        buffer.model().source_format(),
        SourceFormat::utf16le_with_bom()
    );

    // A pre-validated snapshot attaches without a second decode.
    let snapshot =
        TextSnapshot::with_policy(b"already decoded\n", DecodePolicy::Auto).expect("valid utf8");
    let (attached, _transition) = manager.open_note_snapshot(path.with_extension("md2"), snapshot);
    assert_eq!(attached.text(), "already decoded\n");
}

/// A malformed external observation never replaces an existing buffer: state,
/// text and revision are untouched and the error is typed (FR-24).
#[test]
fn malformed_external_change_leaves_existing_buffer_untouched() {
    let mut manager = BufferManager::new();
    let path = std::path::PathBuf::from("/vault/notes/keep.md");
    let (buffer, _transition) = manager
        .open_note(path.clone(), b"saved base\n", DecodePolicy::Auto)
        .expect("open");
    let revision = buffer.revision();
    let text = buffer.text().to_owned();
    let state = buffer.state();

    let error = manager
        .notify_external_change(&path, b"\xff\xfe\x00", DecodePolicy::Auto)
        .expect_err("malformed external bytes are a typed error");
    assert_eq!(error, CodecError::OddUtf16ByteCount);
    let buffer = manager.get_buffer(&path).expect("buffer still present");
    assert_eq!(buffer.text(), text);
    assert_eq!(buffer.revision(), revision);
    assert_eq!(buffer.state(), state);
}
