#![no_main]

//! Fuzz target: `encoding` — strict codec decode/encode/map invariants over
//! arbitrary bytes (RFC-0006 §6.10, §10.6).
//!
//! Invariants: no panic, no overflow, no source mutation (the input slice is
//! never written), and — only for strict successful decodes — byte-exact
//! re-encoding under the applied descriptor plus a stable re-decode. Failure
//! paths must stay typed: a `CodecError` is a finding about the input, never
//! a crash. Decode-map queries are exercised at a few positions for boundary
//! panics.

use libfuzzer_sys::fuzz_target;
use std::sync::Arc;

use flavite_encoding::NOT_CANCELLED;

/// Budget (§10.6 / NFR-19): inputs beyond the codec's own envelope are
/// rejected as non-findings — the codec must return `LimitExceeded`, not
/// OOM, but inputs this large are a producer error, so we skip them.
const MAX_INPUT_BYTES: usize = 1 << 20; // 1 MiB per-fuzz-input working bound

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }

    // The raw bytes must never be mutated by any codec call.
    let before: Arc<[u8]> = Arc::from(data);
    let limits = flavite_encoding::DecodeLimits::default();
    let encode_limits = flavite_encoding::EncodeLimits::default();

    // Each decode case keeps the policy it used: the re-decode invariant is
    // stated under the SAME policy. An explicit BOM-less UTF-16 descriptor
    // can decode bytes that are simultaneously valid UTF-8, so re-decoding
    // such a re-encode under Auto would legitimately resolve differently.
    let cases = [
        (
            flavite_encoding::DecodePolicy::Auto,
            flavite_encoding::decode(data, flavite_encoding::DecodePolicy::Auto, &limits, &NOT_CANCELLED),
        ),
        (
            flavite_encoding::DecodePolicy::Explicit(flavite_encoding::SourceFormat::utf16le()),
            flavite_encoding::decode(
                data,
                flavite_encoding::DecodePolicy::Explicit(flavite_encoding::SourceFormat::utf16le()),
                &limits,
                &NOT_CANCELLED,
            ),
        ),
    ];

    for (policy, decoded) in cases
        .into_iter()
        .filter_map(|(policy, result)| result.ok().map(|decoded| (policy, decoded)))
    {
        let format = decoded.format();
        // Round-trip invariant for strict successful decodes: re-encoding the
        // decoded text under the same descriptor is byte-exact with the
        // source we decoded from.
        let reencoded = flavite_encoding::encode(
            decoded.text(),
            format,
            &encode_limits,
            &NOT_CANCELLED,
        )
        .expect("a strict successful decode re-encodes under its own descriptor");
        assert_eq!(
            reencoded.as_slice(),
            data,
            "encode(decode(bytes)) must round-trip byte-exactly under {format:?}"
        );

        // Re-decoding the re-encoded bytes under the same policy yields the
        // same text.
        let redecoded = flavite_encoding::decode(&reencoded, policy, &limits, &NOT_CANCELLED)
            .expect("round-tripped bytes still decode");
        assert_eq!(redecoded.text(), decoded.text());

        // The boundary map answers position queries without panicking, at a
        // few probed offsets. EOF is always a valid paired boundary, so the
        // full source length must map to the decoded length.
        let map = decoded.map();
        assert_eq!(
            map.source_to_decoded(data.len()),
            Some(decoded.text().len()),
            "EOF must map to the decoded length"
        );
        for probe in [0usize, data.len() / 2, data.len().saturating_sub(1)] {
            let _ = map.source_to_decoded(probe);
            let _ = map.decoded_to_source(probe);
        }
        // A mapped decoded position maps back inside the source.
        if let Some(source) = map.decoded_to_source(0) {
            assert!(map.source_to_decoded(source).is_some(), "map inversion at 0");
        }
    }

    // The input is never mutated (FR-22: the source is never modified).
    assert_eq!(&*before, data, "codec calls must not mutate the input");
});
