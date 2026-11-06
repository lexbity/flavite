//! Opt-in release measurement of the committed pattern corpus through real workers.
use flavite_buffer::revision::{BufferId, Revision};
use flavite_text_search::MatchMode;
use flavite_vault::text_edit::{FindRequest, ReplaceRequest, prepare_find, prepare_ops};
use std::time::Instant;

#[derive(serde::Deserialize)]
struct Pattern {
    id: String,
    mode: String,
    pattern: String,
    case_sensitive: bool,
    whole_word: bool,
}
fn percentile(samples: &mut [f64], percent: usize) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[(samples.len() * percent).div_ceil(100).saturating_sub(1)]
}
#[test]
#[ignore = "reference release load; emits measurements, never runs in ordinary CI"]
#[allow(clippy::assertions_on_constants)] // Runtime guard keeps ordinary debug --no-run builds available.
fn source_search_reference() {
    assert!(!cfg!(debug_assertions), "run with --release");
    let patterns: Vec<Pattern> = serde_json::from_str(include_str!(
        "../../flavite-text-search/tests/fixtures/performance-patterns.json"
    ))
    .unwrap();
    assert_eq!(patterns.len(), 12);
    let block = format!(
        "# Flavite\n\nAn idea about café and Straße.\n- [ ] capture this task\nSee [[target|a visible link]].\nαβ\n{}\n",
        "x".repeat(4096)
    );
    let mut rows = Vec::new();
    for size in [1024 * 1024, 32 * 1024 * 1024] {
        let mut source = block.repeat(size / block.len() + 1);
        let mut end = size;
        while !source.is_char_boundary(end) {
            end -= 1;
        }
        source.truncate(end);
        source.push_str(&" ".repeat(size - end));
        let repetitions = 1000;
        for pattern in &patterns {
            let request = FindRequest {
                buffer_id: BufferId::from(1),
                revision: Revision::new(1),
                query: pattern.pattern.clone(),
                mode: if pattern.mode == "regex" {
                    MatchMode::Regex
                } else {
                    MatchMode::Literal
                },
                case_sensitive: pattern.case_sensitive,
                whole_word: pattern.whole_word,
                offset: 0,
                scope: None,
            };
            let mut samples = Vec::new();
            let mut expected = None;
            for _ in 0..repetitions {
                let start = Instant::now();
                let result = prepare_find(&source, &request).expect("complete bounded Find");
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
                if let Some(count) = expected {
                    assert_eq!(result.total_matches, count);
                }
                expected = Some(result.total_matches);
            }
            let p95 = percentile(&mut samples, 95);
            let p99 = percentile(&mut samples, 99);
            rows.push(serde_json::json!({
                "pattern":pattern.id,"bytes":size,"samples":repetitions,"p95_ms":p95,"p99_ms":p99,
                "matches":expected, "p95_target_ms":if size == 1024*1024 {100} else {1000},
                "within_target": p95 <= if size == 1024*1024 {100.0} else {1000.0},
            }));
        }
        if size == 1024 * 1024 {
            let request = ReplaceRequest {
                buffer_id: BufferId::from(1),
                revision: Revision::new(1),
                query: "idea".into(),
                replacement: "thought".into(),
                case_sensitive: false,
                whole_word: false,
                mode: MatchMode::Literal,
                scope: None,
            };
            let mut samples = Vec::new();
            for _ in 0..1000 {
                let start = Instant::now();
                let _ = prepare_ops(&source, &request).expect("bounded replacement");
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            let p95 = percentile(&mut samples, 95);
            rows.push(
                serde_json::json!({"stage":"replace_proposals","bytes":size,"samples":1000,
                "p95_ms":p95,"within_target":p95<=1000.0}),
            );
        }
    }
    let out = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/workbench-load/source-search.json");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let all_pass = rows.iter().all(|row| row["within_target"] == true);
    std::fs::write(&out, serde_json::to_vec_pretty(&serde_json::json!({
        "evidence_class":"load","seed":6006,"profile":"release",
        "stage":"source matching/proposal workers; excludes UI paint and prepared buffer installation",
        "rows":rows,"thresholds_met":all_pass,
        "host":std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default().lines()
            .find(|line|line.starts_with("model name")).unwrap_or("unknown"),
        "rss":std::fs::read_to_string("/proc/self/status").unwrap_or_default().lines()
            .filter(|line|line.starts_with("VmHWM:")||line.starts_with("VmRSS:")).collect::<Vec<_>>()
    })).unwrap()).unwrap();
    assert!(
        all_pass,
        "reference-pattern latency exceeded targets; inspect {}",
        out.display()
    );
}
