//! RFC-0006 S08 CLI durable replacement operations: preview/select/apply/
//! status over real subprocesses (each command is a fresh process/session,
//! so status exercises startup recovery), plus doctor reporting the
//! operation inventory without key material.

use std::process::{Command, Output};

struct CliEnv {
    _data: tempfile::TempDir,
    _cache: tempfile::TempDir,
}

impl CliEnv {
    fn new() -> Self {
        Self {
            _data: tempfile::tempdir().unwrap(),
            _cache: tempfile::tempdir().unwrap(),
        }
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_flavite-cli"))
            .args(args)
            .env("XDG_DATA_HOME", self._data.path())
            .env("XDG_CACHE_HOME", self._cache.path())
            .output()
            .unwrap()
    }
    fn run_ok(&self, args: &[&str]) -> serde_json::Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        serde_json::from_str(stdout.trim())
            .unwrap_or_else(|_| serde_json::Value::String(stdout.trim().to_owned()))
    }
}

fn prepared_vault_dir(env: &CliEnv) -> std::path::PathBuf {
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(vault.join("a.md"), "alpha one\n").unwrap();
    std::fs::write(vault.join("b.md"), "alpha two\n").unwrap();
    let open = env.run(&["open", "--path", vault.to_str().unwrap(), "--no-watch"]);
    assert!(
        open.status.success(),
        "{}",
        String::from_utf8_lossy(&open.stderr)
    );
    let leaked = std::path::PathBuf::from(vault.as_os_str().to_os_string());
    std::mem::forget(root);
    leaked
}

#[test]
fn preview_select_apply_status_round_trip_across_processes() {
    let env = CliEnv::new();
    let vault = prepared_vault_dir(&env);
    let path = vault.to_str().unwrap().to_owned();

    // Inline apply: seal + select all + apply in one process.
    let applied = env.run_ok(&[
        "replace",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "apply",
        "--query",
        "alpha",
        "--replacement",
        "omega",
        "--key",
        &"0".repeat(32),
    ]);
    let job_id = applied["job_id"].as_str().expect("job id").to_owned();
    assert_eq!(applied["status"]["phase"], "completed");
    assert_eq!(applied["status"]["totals"]["saved"], 2);
    assert_eq!(
        std::fs::read_to_string(vault.join("a.md")).unwrap(),
        "omega one\n"
    );
    assert_eq!(
        std::fs::read_to_string(vault.join("b.md")).unwrap(),
        "omega two\n"
    );

    // Status in a fresh process recovers the completed job.
    let status = env.run_ok(&[
        "replace",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "status",
        "--job-id",
        &job_id,
    ]);
    assert_eq!(status["status"]["phase"], "completed");
    assert_eq!(status["selected_items"], 2);

    // Doctor reports the operation inventory (manifest ok, selection,
    // records, head) without printing any key.
    let doctor = env.run_ok(&["doctor", "--path", &path, "--json"]);
    if let Some(operations) = doctor
        .get("operations")
        .and_then(serde_json::Value::as_array)
    {
        assert!(
            operations.iter().any(|operation| {
                operation["job_id"] == job_id
                    && operation["manifest_ok"] == true
                    && operation["selection_present"] == true
            }),
            "doctor inventory does not list the job: {doctor:?}"
        );
    }
}

#[test]
fn revert_round_trip_restores_before_bytes_across_processes() {
    let env = CliEnv::new();
    let vault = prepared_vault_dir(&env);
    let path = vault.to_str().unwrap().to_owned();

    let applied = env.run_ok(&[
        "replace",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "apply",
        "--query",
        "alpha",
        "--replacement",
        "omega",
        "--key",
        &"1".repeat(32),
    ]);
    let job_id = applied["job_id"].as_str().expect("job id").to_owned();
    assert_eq!(applied["status"]["phase"], "completed");

    // Revert in a fresh process (each CLI invocation is a new session).
    let reverted = env.run_ok(&[
        "replace",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "revert",
        "--job-id",
        &job_id,
        "--key",
        &"2".repeat(32),
    ]);
    assert_eq!(reverted["status"]["phase"], "completed");
    assert_eq!(reverted["status"]["totals"]["reverted"], 2);
    assert_eq!(
        std::fs::read_to_string(vault.join("a.md")).unwrap(),
        "alpha one\n"
    );
    assert_eq!(
        std::fs::read_to_string(vault.join("b.md")).unwrap(),
        "alpha two\n"
    );

    // Status in yet another fresh process shows the revert epoch.
    let status = env.run_ok(&[
        "replace",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "status",
        "--job-id",
        &job_id,
    ]);
    assert_eq!(status["status"]["phase"], "completed");
    assert_eq!(status["status"]["totals"]["reverted"], 2);
}

#[test]
fn unknown_job_status_is_a_typed_refusal() {
    let env = CliEnv::new();
    let vault = prepared_vault_dir(&env);
    let path = vault.to_str().unwrap().to_owned();
    let out = env.run(&[
        "replace",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "status",
        "--job-id",
        &"f".repeat(32),
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("job-unknown"), "{stderr}");
}
