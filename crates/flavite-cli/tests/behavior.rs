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
    let counters = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "runtime_counters")
        .unwrap();
    assert!(
        counters["details"]
            .as_str()
            .unwrap()
            .contains("autosave.failures_total=")
    );
    assert!(
        counters["details"]
            .as_str()
            .unwrap()
            .contains("rename.items_skipped_total=")
    );
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["name"] == "watcher_health")
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

#[test]
fn rename_via_cli_rewrites_links_and_reports_strategy() {
    let env = CliEnv::new();
    let vault = tempfile::tempdir().unwrap();
    let init = env.run(&["init", "--path", vault.path().to_str().unwrap()]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    std::fs::write(vault.path().join("A.md"), "# A\n").unwrap();
    std::fs::write(vault.path().join("Links.md"), "See [[A]]\n").unwrap();

    let output = env.run(&[
        "rename",
        "--vault",
        vault.path().to_str().unwrap(),
        "A.md",
        "B.md",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("rewrite"));
    assert!(!vault.path().join("A.md").exists());
    assert_eq!(std::fs::read(vault.path().join("B.md")).unwrap(), b"# A\n");
    assert_eq!(
        std::fs::read(vault.path().join("Links.md")).unwrap(),
        b"See [[B]]\n"
    );
}

#[test]
fn alias_strategy_and_typed_search_are_reachable_from_cli() {
    let env = CliEnv::new();
    let vault = tempfile::tempdir().unwrap();
    let init = env.run(&["init", "--path", vault.path().to_str().unwrap()]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    std::fs::write(vault.path().join("A.md"), "---\ntags: [x]\n---\n# A\n").unwrap();
    std::fs::write(vault.path().join("Links.md"), "See [[A]]\n").unwrap();

    let alias = env.run(&[
        "rename",
        "--vault",
        vault.path().to_str().unwrap(),
        "A.md",
        "B.md",
        "--strategy",
        "alias",
    ]);
    assert!(
        alias.status.success(),
        "{}",
        String::from_utf8_lossy(&alias.stderr)
    );
    let renamed = std::fs::read_to_string(vault.path().join("B.md")).unwrap();
    assert!(renamed.contains("aliases: \"A\""), "{renamed:?}");
    assert_eq!(
        std::fs::read(vault.path().join("Links.md")).unwrap(),
        b"See [[A]]\n"
    );

    let search = env.run(&["search", "--vault", vault.path().to_str().unwrap(), "tag:x"]);
    assert!(
        search.status.success(),
        "{}",
        String::from_utf8_lossy(&search.stderr)
    );
    assert!(String::from_utf8_lossy(&search.stdout).contains("B.md"));
    let invalid = env.run(&[
        "search",
        "--vault",
        vault.path().to_str().unwrap(),
        "bogus:x",
    ]);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("unsupported query syntax"));
}

#[test]
fn json_stdout_is_one_value_and_json_events_use_stderr() {
    let env = CliEnv::new();
    let vault = tempfile::tempdir().unwrap();
    let init = env.run(&["init", "--path", vault.path().to_str().unwrap()]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    std::fs::write(vault.path().join("A.md"), "# A\n").unwrap();
    std::fs::write(vault.path().join("Links.md"), "See [[A]]\n").unwrap();

    let output = env.run(&[
        "--json",
        "rename",
        "--vault",
        vault.path().to_str().unwrap(),
        "A.md",
        "B.md",
        "--strategy",
        "none",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["strategy"], "none");
    let event_lines = String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(
        event_lines
            .iter()
            .any(|event| event["type"] == "links_broken")
    );
}
