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
        Command::new(env!("CARGO_BIN_EXE_flavite"))
            .args(args)
            .env("XDG_DATA_HOME", self._data.path())
            .env("XDG_CACHE_HOME", self._cache.path())
            .output()
            .unwrap()
    }
}

#[test]
fn init_open_info_query_and_external_rename_are_real() {
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    let init = env.run(&["init", "--path", vault.to_str().unwrap()]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let id_file = vault.join(".flavite/vault.toml");
    let identity = std::fs::read_to_string(&id_file).unwrap();
    assert!(identity.contains("uuid = \""));

    std::fs::write(vault.join("Ideas.md"), "# Ideas\n\nneedleword\n").unwrap();
    let open = env.run(&["open", "--path", vault.to_str().unwrap(), "--no-watch"]);
    assert!(
        open.status.success(),
        "{}",
        String::from_utf8_lossy(&open.stderr)
    );
    let info = env.run(&[
        "info",
        "--path",
        vault.to_str().unwrap(),
        "--no-watch",
        "--json",
    ]);
    assert!(
        info.status.success(),
        "{}",
        String::from_utf8_lossy(&info.stderr)
    );
    let info_json: serde_json::Value = serde_json::from_slice(&info.stdout).unwrap();
    assert_eq!(info_json["note_count"], 1);
    assert!(info_json["generation"].as_u64().unwrap() > 0);
    assert_eq!(
        info_json["vault"]["uuid"],
        identity
            .lines()
            .find_map(|line| line.strip_prefix("uuid = \"")?.strip_suffix('"'))
            .unwrap()
    );

    let query = env.run(&["query", "--path", vault.to_str().unwrap(), "needleword"]);
    assert!(
        query.status.success(),
        "{}",
        String::from_utf8_lossy(&query.stderr)
    );
    assert!(String::from_utf8_lossy(&query.stdout).contains("Ideas.md"));

    std::fs::rename(vault.join("Ideas.md"), vault.join("Concepts.md")).unwrap();
    let renamed = env.run(&["open", "--path", vault.to_str().unwrap(), "--no-watch"]);
    assert!(
        renamed.status.success(),
        "{}",
        String::from_utf8_lossy(&renamed.stderr)
    );
    let after = env.run(&["query", "--path", vault.to_str().unwrap(), "needleword"]);
    assert!(
        after.status.success(),
        "{}",
        String::from_utf8_lossy(&after.stderr)
    );
    assert!(String::from_utf8_lossy(&after.stdout).contains("Concepts.md"));

    let doctor = env.run(&["doctor", "--path", vault.to_str().unwrap(), "--json"]);
    assert!(
        doctor.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["name"] == "attachments")
    );
}

#[test]
fn query_rejects_unsupported_expression_instead_of_searching_literal_text() {
    let env = CliEnv::new();
    let vault = tempfile::tempdir().unwrap();
    let output = env.run(&[
        "query",
        "--path",
        vault.path().to_str().unwrap(),
        "title:foo",
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported query syntax"));
}
