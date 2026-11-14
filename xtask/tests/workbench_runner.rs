//! Supervisor fault fixtures. These do not certify product UX: they exercise
//! the real runner's exit/status/provenance handling without an X server.
#![cfg(unix)]
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};
fn executable(root: &Path, name: &str, source: &str) {
    let path = root.join(name);
    fs::write(&path, source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
fn run(case: &str) -> std::process::Output {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("scripts")).unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    fs::copy(
        repo.join("scripts/workbench-visual-linux.sh"),
        root.join("scripts/workbench-visual-linux.sh"),
    )
    .unwrap();
    fs::create_dir_all(root.join("scripts/lib")).unwrap();
    fs::copy(
        repo.join("scripts/lib/source_fingerprint.py"),
        root.join("scripts/lib/source_fingerprint.py"),
    )
    .unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(root)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["add", "scripts"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=Runner Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture"
            ])
            .status()
            .unwrap()
            .success()
    );
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    executable(&bin, "cargo", "#!/bin/bash\nexit 0\n");
    for tool in ["xdotool", "import", "identify", "tesseract"] {
        executable(&bin, tool, "#!/bin/bash\nexit 0\n");
    }
    executable(
        &bin,
        "xvfb-run",
        r#"#!/bin/bash
while [[ "$1" != bash ]]; do shift; done
shift
script="$1"
shift
scenario="$2"
status="$3"
if [[ "$RUNNER_TEST_CASE" == boot_failure ]]; then exec bash "$script" "$@"; fi
if [[ "$RUNNER_TEST_CASE" == missing_status ]]; then exit 0; fi
run="$CARGO_TARGET_DIR/fixture-evidence"
mkdir -p "$run"
printf '0\n%s\n' "$run" > "$status"
if [[ "$RUNNER_TEST_CASE" == nonzero_worker ]]; then exit 9; fi
if [[ "$RUNNER_TEST_CASE" == missing_provenance ]]; then exit 0; fi
printf '{"scenario":"%s"}\n' "$scenario" > "$run/expected-fixture.json"
if [[ "$RUNNER_TEST_CASE" == positive || "$RUNNER_TEST_CASE" == source_changed ]]; then
    python3 - "$run/provenance.json" "$scenario" <<'PY_FIXTURE'
import json,os,pathlib,sys
pathlib.Path(sys.argv[1]).write_text(json.dumps({"scenario":sys.argv[2],"render_backend":"canvas","input_evidence":"rendered_os_input","worktree_sha256":os.environ["FLAVITE_NATIVE_SOURCE_DIGEST"],"build":os.environ["FLAVITE_NATIVE_BUILD_HEAD"]}))
PY_FIXTURE
    if [[ "$RUNNER_TEST_CASE" == source_changed ]]; then printf 'changed' > scripts/changed-build-input.rs; fi
    exit 0
fi
printf '{"scenario":"wrong","render_backend":"canvas","input_evidence":"rendered_os_input","worktree_sha256":"%064d"}\n' 0 > "$run/provenance.json"
"#,
    );
    let target = root.join("target");
    fs::create_dir_all(target.join("debug/examples")).unwrap();
    executable(
        &target.join("debug/examples"),
        "app-demo",
        "#!/bin/bash\necho 'seeded boot failure' >&2\nexit 78\n",
    );
    // Old global files must never be accepted for this run.
    fs::create_dir_all(target.join("workbench-evidence")).unwrap();
    fs::write(target.join("workbench-evidence/context-mode.status"), "0\n").unwrap();
    Command::new("bash")
        .arg(root.join("scripts/workbench-visual-linux.sh"))
        .args([
            "--scenario",
            "context-mode",
            "--backend",
            "canvas",
            "--jobs",
            "1",
        ])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("CARGO_TARGET_DIR", target)
        .env("RUNNER_TEST_CASE", case)
        .output()
        .unwrap()
}
#[test]
fn workers_cannot_turn_early_failures_or_stale_evidence_into_a_pass() {
    for case in [
        "missing_status",
        "nonzero_worker",
        "missing_provenance",
        "inconsistent_provenance",
        "boot_failure",
        "source_changed",
    ] {
        let output = run(case);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{case}: {stdout}\n{stderr}");
        assert!(
            !stdout.contains("all 1 scenario(s) passed"),
            "{case}: false green"
        );
        assert!(
            stderr.contains("[summary] FAIL"),
            "{case}: {stdout}\n{stderr}"
        );
        if case == "boot_failure" {
            assert!(
                stderr.contains("application exited before creating its window"),
                "{stderr}"
            );
        }
    }
}

#[test]
fn supervisor_accepts_a_correlated_unchanged_run() {
    let output = run("positive");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("all 1 scenario(s) passed"));
}
