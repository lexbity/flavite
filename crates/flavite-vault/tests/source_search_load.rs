//! Opt-in release measurement of the committed pattern corpus through real workers.
use flavite_buffer::revision::{BufferId, Revision};
use flavite_text_search::MatchMode;
use flavite_vault::text_edit::{FindRequest, ReplaceRequest, prepare_find, prepare_ops};
use std::time::Instant;

/// Ensure the regex helper (sibling binary, `flavite-text-search-worker`)
/// is installed next to this test executable — the exact discovery rule the
/// product uses (`current_exe().parent()`; never PATH). Candidates in
/// order: already present → profile-root build (`deps/..`) → the workspace
/// release target dir (`$CARGO_TARGET_DIR` override or `target/release`
/// next to the workspace root) → a one-time `cargo build --release -p
/// flavite-text-search-worker` in the workspace root. (RFC-0008 S9
/// consolidation: the reference-load suite measures the real worker, so the
/// helper must exist — self-installing removes the invisible environment
/// prerequisite.)
fn ensure_helper_installed() {
    let exe = std::env::current_exe().expect("current_exe");
    let dir = exe.parent().expect("test executable parent");
    let mut sibling = dir.join("flavite-text-search-worker");
    #[cfg(windows)]
    sibling.set_extension("exe");
    if sibling.is_file() {
        return;
    }
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../")
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    candidates.push(dir.join("..").join("flavite-text-search-worker"));
    if let Some(target_dir) = std::env::var_os("CARGO_TARGET_DIR") {
        candidates.push(
            std::path::PathBuf::from(target_dir)
                .join("release")
                .join("flavite-text-search-worker"),
        );
    }
    candidates.push(
        workspace_root
            .join("target")
            .join("release")
            .join("flavite-text-search-worker"),
    );
    #[cfg(windows)]
    for candidate in &mut candidates {
        candidate.set_extension("exe");
    }
    let mut built = candidates.into_iter().find(|candidate| candidate.is_file());
    if built.is_none() {
        let status = std::process::Command::new("cargo")
            .arg("build")
            .arg("--release")
            .arg("-p")
            .arg("flavite-text-search-worker")
            .arg("--locked")
            .current_dir(&workspace_root)
            .status()
            .expect("spawn cargo to build the helper");
        assert!(status.success(), "helper build failed");
        let after = dir.join("..").join("flavite-text-search-worker");
        #[cfg(windows)]
        let after = {
            let mut a = after;
            a.set_extension("exe");
            a
        };
        built = after.is_file().then_some(after);
    }
    std::fs::copy(built.as_ref().expect("helper binary buildable"), &sibling)
        .expect("install helper beside the test binary");
}

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
    ensure_helper_installed();
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
                scope_spans: vec![],
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
                scope_spans: vec![],
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
