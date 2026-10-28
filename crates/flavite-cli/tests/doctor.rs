use std::process::Command;

#[test]
fn doctor_uses_vault_identity_and_reports_corrupt_cache() {
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let cache_root = tempfile::tempdir().unwrap();

    let run = || {
        Command::new(env!("CARGO_BIN_EXE_flavite"))
            .args(["doctor", "--path"])
            .arg(root.path())
            .arg("--json")
            .env("XDG_DATA_HOME", data.path())
            .env("XDG_CACHE_HOME", cache_root.path())
            .output()
            .unwrap()
    };

    let healthy = run();
    assert!(
        healthy.status.success(),
        "{}",
        String::from_utf8_lossy(&healthy.stderr)
    );
    let identity = std::fs::read_to_string(root.path().join(".flavite/vault.toml")).unwrap();
    let uuid = identity
        .lines()
        .find_map(|line| line.strip_prefix("uuid = \"")?.strip_suffix('"'))
        .unwrap();
    let db = cache_root
        .path()
        .join("flavite/flavite")
        .join(uuid)
        .join("v1/index.sqlite3");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(db, b"damaged cache").unwrap();

    let damaged = run();
    assert!(!damaged.status.success());
    let report: serde_json::Value = serde_json::from_slice(&damaged.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&damaged.stdout)));
    assert_eq!(report["vault"]["uuid"], uuid);
    assert_eq!(
        report["checks"].as_array().unwrap()[0]["name"],
        "sqlite_quick_check"
    );
    assert_eq!(report["checks"][0]["status"], "Error");
}
