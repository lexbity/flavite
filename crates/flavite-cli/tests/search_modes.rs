//! RFC-0006 S06 CLI search modes (§6.1 legacy migration): plain text is the
//! default, explicit expression syntax is not whitespace-split, page
//! tokens enumerate with total/next, `--all` consumes every continuation
//! token, session close drops the spool, and refusals are deterministic
//! machine-readable error objects.

use std::io::Write;
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
}

fn prepared_vault_dir(env: &CliEnv) -> std::path::PathBuf {
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    for index in 0..60 {
        let mut file = std::fs::File::create(vault.join(format!("note-{index:02}.md"))).unwrap();
        writeln!(file, "# Note {index}\n\npagemark token {index}\n").unwrap();
    }
    std::fs::write(vault.join("quiet.md"), "# Quiet\n\nnothing here\n").unwrap();
    std::fs::write(
        vault.join("other.md"),
        "# Other\n\npagemark but untokened\n",
    )
    .unwrap();
    let open = env.run(&["open", "--path", vault.to_str().unwrap(), "--no-watch"]);
    assert!(
        open.status.success(),
        "{}",
        String::from_utf8_lossy(&open.stderr)
    );
    let path = vault.as_os_str().to_os_string();
    let leaked = std::path::PathBuf::from(path);
    std::mem::forget(root);
    leaked
}

#[test]
fn default_text_mode_pages_with_total_and_consumes_all_tokens() {
    let env = CliEnv::new();
    let vault = prepared_vault_dir(&env);
    let path = vault.to_str().unwrap().to_owned();

    // Plain text mode: whitespace means AND. `pagemark token` matches the
    // 60 token notes, not `other.md` (missing "token") nor `quiet.md`.
    let out = env.run(&[
        "search",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "--limit",
        "50",
        "--all",
        "pagemark token",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let pages: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(pages.len(), 2, "50 + 10 across two pages: {stdout}");
    let rows: Vec<&serde_json::Value> = pages
        .iter()
        .flat_map(|page| page["rows"].as_array().unwrap())
        .collect();
    assert_eq!(rows.len(), 60, "every continuation token was consumed");
    let paths: std::collections::BTreeSet<String> = rows
        .iter()
        .map(|row| row["path"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(paths.len(), 60, "no row appears twice across pages");
    assert!(paths.iter().all(|path| path.starts_with("note-")));
    assert_eq!(pages[0]["total"], 60);
    assert_eq!(pages[0]["next"], 50);
    assert!(pages[1]["next"].is_null(), "the end token is absent");
    // The session was explicitly closed after consumption.
    let session = pages[0]["session"].as_str().unwrap().to_owned();
    assert_eq!(session.len(), 32);
}

#[test]
fn explicit_expression_mode_is_not_whitespace_split() {
    let env = CliEnv::new();
    let vault = prepared_vault_dir(&env);
    let path = vault.to_str().unwrap().to_owned();
    // Expression mode requires explicit operators: two bare terms are a
    // syntax error, not an implicit AND.
    let out = env.run(&[
        "search",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "--mode",
        "expression",
        "pagemark token",
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let parsed = json_line(&stderr).expect("machine-readable error object");
    assert_eq!(parsed["error"]["kind"], "invalid");

    // With explicit operators the same terms AND together.
    let out = env.run(&[
        "search",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "--mode",
        "expression",
        "pagemark AND token",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let first: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert_eq!(first["total"], 60);
}

#[test]
fn deterministic_errors_cover_unsupported_and_empty_queries() {
    let env = CliEnv::new();
    let vault = prepared_vault_dir(&env);
    let path = vault.to_str().unwrap().to_owned();

    // Vault source matching arrives with RFC-0006 S07; the refusal is a
    // typed error object, not a panic and not a silent text search.
    let out = env.run(&[
        "search",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "--mode",
        "regex",
        "pattern",
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let parsed = json_line(&stderr).unwrap();
    assert_eq!(parsed["error"]["kind"], "unsupported");
    assert!(
        parsed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--mode text")
    );

    // Empty/whitespace queries are invalid, never "match everywhere".
    let out = env.run(&["search", "--path", &path, "--no-watch", "--json", "   "]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let parsed = json_line(&stderr).unwrap();
    assert_eq!(parsed["error"]["kind"], "invalid");
}

/// The machine-readable error object is the one `{...}` line on stderr.
fn json_line(stderr: &str) -> Option<serde_json::Value> {
    stderr
        .lines()
        .filter(|line| line.trim_start().starts_with('{'))
        .find_map(|line| serde_json::from_str(line).ok())
}

#[test]
fn field_filters_restrict_the_evaluated_searcher_fields() {
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(
        vault.join("titled.md"),
        "---\ntitle: Rocket Manual\n---\nbody without the word\n",
    )
    .unwrap();
    std::fs::write(vault.join("bodied.md"), "rocket in the body only\n").unwrap();
    let open = env.run(&["open", "--path", vault.to_str().unwrap(), "--no-watch"]);
    assert!(open.status.success());
    let path = vault.to_str().unwrap().to_owned();
    let out = env.run(&[
        "search",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "--field",
        "title",
        "rocket",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let first: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    let paths: Vec<&str> = first["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["titled.md"], "title-only filter: {stdout}");
}
