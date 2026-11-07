//! Opt-in release measurement of strict decode/encode/map throughput
//! (NFR-18 envelope: ≥50 MiB/s raw, 1 MiB decode+map preparation p95
//! ≤100 ms). Emits measurements; never runs in ordinary CI.
#![allow(clippy::cast_precision_loss)] // size/throughput reporting only

use flavite_encoding::{
    DecodeLimits, DecodePolicy, EncodeLimits, FileEncoding, SourceFormat, decode, encode,
};
use std::fmt::Write as _;
use std::time::Instant;

fn percentile(samples: &mut [f64], percent: usize) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[(samples.len() * percent).div_ceil(100).saturating_sub(1)]
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn sample<F: FnMut()>(repetitions: usize, mut run: F) -> (f64, f64, f64) {
    let mut samples = Vec::with_capacity(repetitions);
    for _ in 0..repetitions {
        let start = Instant::now();
        run();
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let p50 = percentile(&mut samples, 50);
    (
        p50,
        percentile(&mut samples, 95),
        percentile(&mut samples, 99),
    )
}

fn build_utf16(text: &str, le: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + text.len() * 2);
    out.extend_from_slice(&[0xFF, 0xFE]);
    for unit in text.encode_utf16() {
        let bytes = if le {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        };
        out.extend_from_slice(&bytes);
    }
    out
}

#[test]
#[ignore = "reference release load; emits measurements, never runs in ordinary CI"]
#[allow(clippy::assertions_on_constants)] // Runtime guard keeps ordinary debug --no-run builds available.
fn codec_reference() {
    assert!(!cfg!(debug_assertions), "run with --release");

    // Mixed-content note: mostly ASCII with CJK/supplementary lines so both
    // the BMP and surrogate paths are exercised.
    let mut block = String::from("# Flavite\n\nAn idea about café and Straße.\n");
    for i in 0..64 {
        let _ = writeln!(block, "- [ ] 考察 {i}: αβ γδ — 😀");
    }
    let repetitions = 200;

    for size in [1024 * 1024usize, 32 * 1024 * 1024] {
        let mut source = block.repeat(size / block.len() + 1);
        let mut cut = size.min(source.len());
        while !source.is_char_boundary(cut) {
            cut -= 1;
        }
        source.truncate(cut);
        let raw_bytes = source.len();

        // One-shot decode, UTF-8 (dominant family).
        let (p50, p95, p99) = sample(repetitions, || {
            let decoded = decode(
                source.as_bytes(),
                DecodePolicy::Auto,
                &DecodeLimits::default(),
                &flavite_encoding::NOT_CANCELLED,
            )
            .expect("decodes");
            assert_eq!(decoded.text().len(), source.len());
        });
        let size_mib = mib(raw_bytes);
        let utf8_throughput = mib(raw_bytes) / (p50 / 1000.0);
        println!(
            "decode utf8  {size_mib:.0} MiB: p50 {p50:>7.2} ms  p95 {p95:>7.2} ms  p99 {p99:>7.2} ms  ({utf8_throughput:>6.1} MiB/s raw)"
        );

        // One-shot decode + map preparation, UTF-16LE with BOM. The UTF-16
        // encoding doubles the byte count, so the raw budget scales with it.
        let le_bytes = build_utf16(&source, true);
        let utf16_limits = DecodeLimits {
            max_raw_bytes: (le_bytes.len() as u64).saturating_mul(2),
            ..DecodeLimits::default()
        };
        let (p50, p95, p99) = sample(repetitions.max(50).min(repetitions), || {
            let decoded = decode(
                &le_bytes,
                DecodePolicy::Auto,
                &utf16_limits,
                &flavite_encoding::NOT_CANCELLED,
            )
            .expect("decodes");
            assert_eq!(decoded.text(), source);
        });
        let utf16_throughput = mib(raw_bytes) / (p50 / 1000.0);
        println!(
            "decode utf16le {size_mib:.0} MiB: p50 {p50:>7.2} ms  p95 {p95:>7.2} ms  p99 {p99:>7.2} ms  ({utf16_throughput:>6.1} MiB/s raw)"
        );

        // Strict encode back to UTF-16LE (validate + serialize). The UTF-16
        // output can exceed the default serialized budget, so it scales too.
        let format = SourceFormat::new(FileEncoding::Utf16Le, true).unwrap();
        let encode_limits = EncodeLimits {
            max_output_bytes: (le_bytes.len() as u64).saturating_mul(2),
            ..EncodeLimits::default()
        };
        let (p50, p95, p99) = sample(repetitions.min(50), || {
            let out = encode(
                &source,
                format,
                &encode_limits,
                &flavite_encoding::NOT_CANCELLED,
            )
            .expect("encodes");
            assert_eq!(out.len(), le_bytes.len());
        });
        let encode_throughput = mib(raw_bytes) / (p50 / 1000.0);
        println!(
            "encode utf16le {size_mib:.0} MiB: p50 {p50:>7.2} ms  p95 {p95:>7.2} ms  p99 {p99:>7.2} ms  ({encode_throughput:>6.1} MiB/s input)"
        );

        if size == 1024 * 1024 {
            // NFR-18: 1 MiB decode + map preparation p95 ≤ 100 ms; also map
            // query throughput across every scalar boundary.
            let decoded = decode(
                &le_bytes,
                DecodePolicy::Auto,
                &DecodeLimits::default(),
                &flavite_encoding::NOT_CANCELLED,
            )
            .expect("decodes");
            let map = decoded.map();
            let scalars = decoded.text().chars().count();
            let (p50, p95, _p99) = sample(20, || {
                let mut checksum = 0usize;
                for dec in (0..decoded.text().len()).step_by(3) {
                    if let Some(src) = map.decoded_to_source(dec) {
                        checksum += src;
                    }
                }
                let _ = checksum;
            });
            println!("map queries {scalars:>6} scalars: p50 {p50:>7.2} ms  p95 {p95:>7.2} ms");
            let source_positions = le_bytes.len() / 2;
            let (p50, _p95, _p99) = sample(20, || {
                for src in (2..le_bytes.len()).step_by(2) {
                    let _ = map.source_to_decoded(src);
                }
            });
            println!("map source queries {source_positions} positions: p50 {p50:>7.2} ms");
        }
    }
}
