//! Extended CLI surface coverage: structural `query` success, human-readable
//! `info`/`doctor` output, typed open/theme/encoding errors, literal source
//! find with continuation, and the manual replace preview/select/apply/
//! status/cancel/resume paths (RFC-0006 S07/S08).

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

fn init_vault(env: &CliEnv, vault: &std::path::Path) {
    let init = env.run(&["init", "--path", vault.to_str().unwrap()]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
}

fn vault_arg(vault: &std::path::Path) -> String {
    vault.to_str().unwrap().to_owned()
}

#[test]
fn query_structural_succeeds_human_and_json() {
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    init_vault(&env, &vault);
    std::fs::write(
        vault.join("tagged.md"),
        "---\ntags: [idea]\n---\n# Tagged\n",
    )
    .unwrap();
    std::fs::write(vault.join("untagged.md"), "# Plain\n").unwrap();
    let path = vault_arg(&vault);

    let human = env.run(&["query", "--path", &path, "--no-watch", "tag:idea"]);
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(stdout.contains("tagged.md"), "{stdout}");
    assert!(!stdout.contains("untagged.md"), "{stdout}");

    let json = env.run(&["query", "--path", &path, "--no-watch", "--json", "tag:idea"]);
    assert!(json.status.success());
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert!(value.as_array().is_some());
    assert!(value.to_string().contains("tagged.md"));
}

#[test]
fn info_human_and_doctor_human_report_healthy_vault() {
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    init_vault(&env, &vault);
    std::fs::write(vault.join("a.md"), "# A\n").unwrap();
    let path = vault_arg(&vault);

    let info = env.run(&["info", "--path", &path, "--no-watch"]);
    assert!(
        info.status.success(),
        "{}",
        String::from_utf8_lossy(&info.stderr)
    );
    let stdout = String::from_utf8_lossy(&info.stdout);
    assert!(stdout.contains("Vault:"), "{stdout}");
    assert!(stdout.contains("UUID:"), "{stdout}");
    assert!(stdout.contains("Notes: 1"), "{stdout}");
    assert!(stdout.contains("API Version:"), "{stdout}");

    let doctor = env.run(&["doctor", "--path", &path]);
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(stdout.contains("Vault Doctor Report"), "{stdout}");
    assert!(stdout.contains("HEALTHY"), "{stdout}");
    assert!(stdout.contains('✓'), "{stdout}");
}

#[test]
fn open_missing_vault_is_a_typed_refusal() {
    let env = CliEnv::new();
    // A path that exists but is not a directory reaches the explicit
    // "Vault does not exist" guard.
    let root = tempfile::tempdir().unwrap();
    let not_vault = root.path().join("a-file");
    std::fs::write(&not_vault, b"x").unwrap();
    let out = env.run(&["open", "--path", not_vault.to_str().unwrap(), "--no-watch"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Vault does not exist"), "{stderr}");
}

#[test]
fn api_test_runs_and_reports_versions() {
    let env = CliEnv::new();
    let out = env.run(&["api-test"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("API version:"), "{stdout}");
    assert!(stdout.contains("Event API version:"), "{stdout}");
    assert!(stdout.contains("Created ApiVaultRef:"), "{stdout}");
}

#[test]
fn theme_list_select_and_uninstall_flows() {
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    init_vault(&env, &vault);
    let path = vault_arg(&vault);

    let list = env.run(&["theme", "--vault", &path, "--no-watch", "list"]);
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert!(String::from_utf8_lossy(&list.stdout).contains("No themes installed."));

    let list_json = env.run(&["theme", "--vault", &path, "--no-watch", "--json", "list"]);
    assert!(list_json.status.success());
    let value: serde_json::Value = serde_json::from_slice(&list_json.stdout).unwrap();
    assert_eq!(value["themes"].as_array().map(Vec::len), Some(0));

    // Selecting `none` clears the selection without requiring an installed
    // theme.
    let select_none = env.run(&["theme", "--vault", &path, "--no-watch", "select", "none"]);
    assert!(
        select_none.status.success(),
        "{}",
        String::from_utf8_lossy(&select_none.stderr)
    );
    assert!(String::from_utf8_lossy(&select_none.stdout).contains("Theme selection saved"));

    // Selecting an uninstalled theme is refused.
    let select_missing = env.run(&["theme", "--vault", &path, "--no-watch", "select", "nope"]);
    assert!(!select_missing.status.success());
    assert!(
        String::from_utf8_lossy(&select_missing.stderr).contains("not installed"),
        "{}",
        String::from_utf8_lossy(&select_missing.stderr)
    );

    // Uninstalling an unknown theme is refused.
    let uninstall = env.run(&["theme", "--vault", &path, "--no-watch", "uninstall", "nope"]);
    assert!(!uninstall.status.success());
    assert!(!String::from_utf8_lossy(&uninstall.stderr).is_empty());
}

#[test]
fn theme_convert_rejects_missing_input() {
    let env = CliEnv::new();
    let missing = tempfile::tempdir().unwrap().path().join("missing.xpi");
    let out = env.run(&["theme", "convert", missing.to_str().unwrap()]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("reading"), "{stderr}");
}

#[test]
fn theme_install_select_list_uninstall_lifecycle() {
    // A real minimal browser-theme package (manifest only) drives the full
    // store lifecycle through the CLI: install, human list, select (the
    // `select_theme` installed-check + settings_write arms), JSON list,
    // then uninstall.
    use std::io::{Cursor, Write};
    const MANIFEST: &str = r##"{
        "manifest_version": 2,
        "name": "CLI Fixture",
        "browser_specific_settings": { "gecko": { "id": "cli@host" } },
        "theme": { "colors": { "frame": "#101010", "tab_background_text": "#eeeeee" } }
    }"##;
    let mut buffer = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("manifest.json", options).expect("entry");
        zip.write_all(MANIFEST.as_bytes()).expect("bytes");
        zip.finish().expect("finish");
    }
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    init_vault(&env, &vault);
    let package = root.path().join("fixture.xpi");
    std::fs::write(&package, buffer.into_inner()).unwrap();
    let path = vault_arg(&vault);

    let install = env.run(&[
        "theme",
        "--vault",
        &path,
        "--no-watch",
        "install",
        package.to_str().unwrap(),
    ]);
    assert!(
        install.status.success(),
        "{}",
        String::from_utf8_lossy(&install.stderr)
    );
    let stdout = String::from_utf8_lossy(&install.stdout);
    assert!(stdout.contains("Installed: cli@host"), "{stdout}");

    let list = env.run(&["theme", "--vault", &path, "--no-watch", "list"]);
    assert!(list.status.success());
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("cli@host — CLI Fixture"), "{stdout}");

    let select = env.run(&[
        "theme",
        "--vault",
        &path,
        "--no-watch",
        "select",
        "cli@host",
    ]);
    assert!(
        select.status.success(),
        "{}",
        String::from_utf8_lossy(&select.stderr)
    );
    assert!(String::from_utf8_lossy(&select.stdout).contains("Theme selection saved"));

    let uninstall = env.run(&[
        "theme",
        "--vault",
        &path,
        "--no-watch",
        "uninstall",
        "cli@host",
    ]);
    assert!(
        uninstall.status.success(),
        "{}",
        String::from_utf8_lossy(&uninstall.stderr)
    );
    assert!(String::from_utf8_lossy(&uninstall.stdout).contains("theme.uninstall: ok"));

    let list = env.run(&["theme", "--vault", &path, "--no-watch", "list"]);
    assert!(list.status.success());
    assert!(String::from_utf8_lossy(&list.stdout).contains("No themes installed."));
}

#[test]
fn encoding_convert_success_rewrites_bytes_and_reports_descriptor() {
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    init_vault(&env, &vault);
    let note = vault.join("plain.md");
    std::fs::write(&note, b"plain text\n").unwrap();
    let out = env.run(&[
        "--vault",
        &vault_arg(&vault),
        "--no-watch",
        "encoding",
        note.to_str().unwrap(),
        "convert",
        "--encoding",
        "utf16-le",
        "--bom",
        "present",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["encoding"], "utf16le");
    assert_eq!(report["bom"], "present");
    assert_eq!(report["converted"], true);
    assert_eq!(report["saved"], true);
    // The bytes on disk are now UTF-16LE with a BOM.
    let bytes = std::fs::read(&note).unwrap();
    assert_eq!(&bytes[..2], &[0xFF, 0xFE], "BOM present");
    let text = String::from_utf16_lossy(
        &bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect::<Vec<_>>(),
    );
    assert_eq!(text, "plain text\n");
}

#[test]
fn doctor_human_lists_durable_operations_after_a_job() {
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    init_vault(&env, &vault);
    std::fs::write(vault.join("a.md"), "alpha one\n").unwrap();
    let path = vault_arg(&vault);

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
        &"6".repeat(32),
    ]);
    assert_eq!(applied["status"]["phase"], "completed");

    let doctor = env.run(&["doctor", "--path", &path]);
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        stdout.contains("Durable replacement operations:"),
        "{stdout}"
    );
    assert!(stdout.contains("state="), "{stdout}");
    assert!(stdout.contains("manifest=true"), "{stdout}");
    assert!(stdout.contains("records="), "{stdout}");
    assert!(stdout.contains("HEALTHY"), "{stdout}");
}

#[test]
fn info_uses_current_directory_when_no_vault_path_is_given() {
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    init_vault(&env, &vault);
    std::fs::write(vault.join("a.md"), "# A\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_flavite-cli"))
        .args(["info", "--no-watch"])
        .current_dir(&vault)
        .env("XDG_DATA_HOME", env._data.path())
        .env("XDG_CACHE_HOME", env._cache.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Vault:"), "{stdout}");
    assert!(stdout.contains("Notes: 1"), "{stdout}");
}

#[test]
fn search_session_failure_emits_the_machine_readable_error_object() {
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    init_vault(&env, &vault);
    let path = vault_arg(&vault);

    // A whitespace-only term reaches the session endpoint and fails as a
    // typed invalid query; the CLI prints the deterministic error object on
    // stderr and exits non-zero.
    let out = env.run(&["search", "--path", &path, "--no-watch", "--json", "   "]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let parsed: serde_json::Value = stderr
        .lines()
        .filter(|line| line.trim_start().starts_with('{'))
        .find_map(|line| serde_json::from_str(line).ok())
        .expect("machine-readable error object");
    assert_eq!(parsed["error"]["kind"], "invalid");
    assert!(!parsed["error"]["message"].as_str().unwrap_or("").is_empty());
}

#[test]
fn theme_convert_success_writes_the_flavite_xpi_human_and_json() {
    // Build a real minimal xpi and convert it (offline tool, no vault).
    use std::io::{Cursor, Write};
    const MANIFEST: &str = r##"{
        "manifest_version": 2,
        "name": "Convert Fixture",
        "browser_specific_settings": { "gecko": { "id": "convert@host" } },
        "theme": { "colors": { "frame": "#101010", "tab_background_text": "#eeeeee" } }
    }"##;
    let mut buffer = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("manifest.json", options).expect("entry");
        zip.write_all(MANIFEST.as_bytes()).expect("bytes");
        zip.finish().expect("finish");
    }
    let env = CliEnv::new();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("fixture.xpi");
    std::fs::write(&input, buffer.into_inner()).unwrap();

    // Human output.
    let human = env.run(&["theme", "convert", input.to_str().unwrap()]);
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(stdout.contains("Theme id: convert@host"), "{stdout}");
    assert!(stdout.contains("Output: "), "{stdout}");
    let output = dir.path().join("fixture.xpi.flavite.xpi");
    assert!(output.exists(), "converted package written");

    // JSON output.
    let json = env.run(&["theme", "convert", "--json", input.to_str().unwrap()]);
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["report"]["theme_id"], "convert@host");
    assert!(value["output"].as_str().unwrap().contains("flavite.xpi"));
}

#[test]
fn replace_human_inline_apply_and_source_find_human_continuation() {
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    init_vault(&env, &vault);
    std::fs::write(vault.join("a.md"), "alpha one\n").unwrap();
    let path = vault_arg(&vault);

    // Human inline apply prints the full result JSON on stdout (the non-
    // status else branch of run_replace).
    let applied = env.run(&[
        "replace",
        "--path",
        &path,
        "--no-watch",
        "apply",
        "--query",
        "alpha",
        "--replacement",
        "omega",
        "--key",
        &"7".repeat(32),
    ]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let applied_value: serde_json::Value =
        serde_json::from_slice(&applied.stdout).expect("human apply prints JSON");
    assert_eq!(applied_value["status"]["phase"], "completed");

    // Human source-find with --all exercises the multi-page continuation
    // print ("page N: source find continuation consumed").
    for index in 0..60 {
        std::fs::write(vault.join(format!("note-{index:02}.md")), "# n\n\nneedle\n").unwrap();
    }
    let search = env.run(&[
        "search",
        "--path",
        &path,
        "--no-watch",
        "--mode",
        "literal",
        "--all",
        "needle",
    ]);
    assert!(
        search.status.success(),
        "{}",
        String::from_utf8_lossy(&search.stderr)
    );
    let stderr = String::from_utf8_lossy(&search.stderr);
    assert!(
        stderr.contains("source find continuation consumed"),
        "{stderr}"
    );
    let stdout = String::from_utf8_lossy(&search.stdout);
    assert!(stdout.contains("note-59.md"), "{stdout}");
}

#[test]
fn doctor_human_prints_errors_and_fixes_for_a_corrupt_cache() {
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    init_vault(&env, &vault);
    std::fs::write(vault.join("a.md"), "# A\n").unwrap();
    // Open once so the identity + cache layout exist, then damage the
    // vault-local SQLite index.
    let open = env.run(&["open", "--path", vault.to_str().unwrap(), "--no-watch"]);
    assert!(open.status.success());
    let identity = std::fs::read_to_string(vault.join(".flavite/vault.toml")).unwrap();
    let uuid = identity
        .lines()
        .find_map(|line| line.strip_prefix("uuid = \"")?.strip_suffix('"'))
        .unwrap()
        .to_owned();
    let db = vault
        .join(".flavite/cache")
        .join(&uuid)
        .join("v4/index.sqlite3");
    std::fs::write(&db, b"damaged").unwrap();

    let doctor = env.run(&["doctor", "--path", vault.to_str().unwrap()]);
    assert!(
        !doctor.status.success(),
        "doctor must fail on corrupt cache"
    );
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(stdout.contains('✗'), "{stdout}");
    assert!(stdout.contains("Fix:"), "{stdout}");
    assert!(stdout.contains("UNHEALTHY"), "{stdout}");
    assert!(stdout.contains("SQLite"), "{stdout}");
}

#[test]
fn encoding_reopen_human_reports_descriptor() {
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    init_vault(&env, &vault);
    let note = vault.join("plain.md");
    std::fs::write(&note, b"plain text\n").unwrap();
    let out = env.run(&[
        "--vault",
        &vault_arg(&vault),
        "--no-watch",
        "encoding",
        note.to_str().unwrap(),
        "reopen",
        "--encoding",
        "utf8",
        "--bom",
        "absent",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("plain.md: utf8"));
}

#[test]
fn search_literal_source_find_succeeds_and_consumes_continuations() {
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    init_vault(&env, &vault);
    // 60 notes with one match each: > the default page size, so the
    // `--all` consumer walks more than one page.
    for index in 0..60 {
        std::fs::write(
            vault.join(format!("note-{index:02}.md")),
            format!("# {index}\n\nneedle {index}\n"),
        )
        .unwrap();
    }
    let path = vault_arg(&vault);

    // Human output prints the matched rows as path<TAB>count.
    let human = env.run(&[
        "search",
        "--path",
        &path,
        "--no-watch",
        "--mode",
        "literal",
        "needle",
    ]);
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(stdout.contains("note-00.md"), "{stdout}");
    assert!(stdout.contains('\t'), "rows are path<TAB>count: {stdout}");

    // JSON output consumes every continuation with --all.
    let json = env.run(&[
        "search",
        "--path",
        &path,
        "--no-watch",
        "--mode",
        "literal",
        "--json",
        "--all",
        "needle",
    ]);
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let pages: Vec<serde_json::Value> = String::from_utf8_lossy(&json.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(pages.len() > 1, "literal source find pages across notes");
    assert_eq!(pages[0]["mode"], "literal");
    let total: u64 = pages
        .iter()
        .map(|page| page["total_matches"].as_u64().unwrap_or(0))
        .sum();
    assert_eq!(total, 60, "one match per note: {pages:?}");
}

#[test]
fn replace_manual_preview_select_apply_and_status_flow() {
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    init_vault(&env, &vault);
    std::fs::write(vault.join("a.md"), "alpha one\n").unwrap();
    let path = vault_arg(&vault);

    // Inline apply in one process is the supported manual flow; the human
    // status printer then reports the completed job.
    let inline = env.run_ok(&[
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
        &"3".repeat(32),
    ]);
    assert_eq!(inline["status"]["phase"], "completed");
    assert_eq!(inline["status"]["totals"]["saved"], 1);
    assert_eq!(
        std::fs::read_to_string(vault.join("a.md")).unwrap(),
        "omega one\n"
    );
    let job_id = inline["job_id"].as_str().expect("job id").to_owned();

    let status = env.run(&[
        "replace",
        "--path",
        &path,
        "--no-watch",
        "status",
        "--job-id",
        &job_id,
    ]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(stdout.contains("phase=completed"), "{stdout}");
}

#[test]
fn replace_preview_then_cross_process_select_apply_are_typed_refusals() {
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    init_vault(&env, &vault);
    std::fs::write(vault.join("a.md"), "alpha one\n").unwrap();
    let path = vault_arg(&vault);

    // Seal a preview (works in its own process).
    let preview = env.run_ok(&[
        "replace",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "preview",
        "alpha",
        "omega",
    ]);
    let preview_id = preview["preview_id"]
        .as_str()
        .expect("preview id")
        .to_owned();

    // Select in a *fresh* process: previews are per-session, so the typed
    // refusal exercises the select arm and the ActionRequired path.
    let selected = env.run(&[
        "replace",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "select",
        "--preview-id",
        &preview_id,
        "--select",
        "all",
    ]);
    assert!(!selected.status.success());
    let stderr = String::from_utf8_lossy(&selected.stderr);
    assert!(stderr.contains("requires an action"), "{stderr}");

    // Apply with ids across processes is equally refused.
    let applied = env.run(&[
        "replace",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "apply",
        "--preview-id",
        &preview_id,
        "--selection-revision",
        "0",
        "--key",
        &"3".repeat(32),
    ]);
    assert!(!applied.status.success());
    assert!(String::from_utf8_lossy(&applied.stderr).contains("requires an action"));
}

#[test]
fn replace_select_keys_apply_mismatch_and_cancel_resume_refusals() {
    let env = CliEnv::new();
    let root = tempfile::tempdir().unwrap();
    let vault = root.path().join("vault");
    init_vault(&env, &vault);
    std::fs::write(vault.join("a.md"), "alpha one\n").unwrap();
    let path = vault_arg(&vault);

    let preview = env.run_ok(&[
        "replace",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "preview",
        "alpha",
        "omega",
    ]);
    let preview_id = preview["preview_id"]
        .as_str()
        .expect("preview id")
        .to_owned();

    // The keys= select variant maps its args before the per-session refusal.
    let selected = env.run(&[
        "replace",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "select",
        "--preview-id",
        &preview_id,
        "--select",
        "keys=00000000000000000000000000000000",
    ]);
    assert!(!selected.status.success());
    assert!(String::from_utf8_lossy(&selected.stderr).contains("requires an action"));

    // Apply with a preview id but no selection revision is a typed refusal.
    let mismatch = env.run(&[
        "replace",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "apply",
        "--preview-id",
        &preview_id,
        "--key",
        &"4".repeat(32),
    ]);
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("apply requires"));

    // Cancelling an unknown job is a typed refusal; resuming one too.
    let cancel = env.run(&[
        "replace",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "cancel",
        "--job-id",
        &"a".repeat(32),
    ]);
    assert!(!cancel.status.success());
    let resume = env.run(&[
        "replace",
        "--path",
        &path,
        "--no-watch",
        "--json",
        "resume",
        "--job-id",
        &"b".repeat(32),
        "--key",
        &"5".repeat(32),
    ]);
    assert!(!resume.status.success());
}
