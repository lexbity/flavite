//! RFC-0006 S00 — CLI `flavite encoding` command against real bytes.
//!
//! The CLI crate is a binary (no importable driver), so this follows the
//! existing `tests/behavior.rs` harness: it shells out to the built binary
//! with hermetic XDG data/cache directories, writes real UTF-16 files, and
//! verifies reopen reporting and refusal behavior against the actual bytes
//! on disk.
//!
//! Two known deviations from the RFC-0006 S00 CLI contract are documented in
//! the task report rather than asserted here: the flag spellings are
//! kebab-case (`utf16-le`/`utf16-be` instead of `utf16le`/`utf16be`), and
//! `encoding <note> convert` currently cannot complete (its internal
//! `note.save` call passes `{"path": …}` where the facade expects a bare
//! path string), so the convert path is exercised by the vault- and
//! API-level suites instead.

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

/// Independent std-only UTF-16 encoder (little-endian, optional BOM).
fn utf16le_bytes(text: &str, bom: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if bom {
        out.extend_from_slice(&[0xFF_u8, 0xFE]);
    }
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

fn init_vault(env: &CliEnv, vault: &std::path::Path) {
    let init = env.run(&["init", "--path", vault.to_str().unwrap()]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
}

/// Reopen under explicit and auto families reports the exact descriptor as
/// JSON and never rewrites the underlying bytes.
#[test]
fn encoding_reopen_reports_exact_descriptor_without_rewriting_bytes() {
    let env = CliEnv::new();
    let vault_root = tempfile::tempdir().unwrap();
    let vault = vault_root.path().join("vault");
    init_vault(&env, &vault);

    let text = "Cli 😀 note\r\nsecond\n";
    let note = vault.join("u16.md");
    std::fs::write(&note, utf16le_bytes(text, true)).unwrap();
    let note_arg = note.to_str().unwrap();
    let vault_arg = vault.to_str().unwrap();

    // Explicit reopen under the matching family.
    let reopen = env.run(&[
        "--vault",
        vault_arg,
        "--no-watch",
        "encoding",
        note_arg,
        "reopen",
        "--encoding",
        "utf16-le",
        "--bom",
        "auto",
        "--json",
    ]);
    assert!(
        reopen.status.success(),
        "{}",
        String::from_utf8_lossy(&reopen.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&reopen.stdout).unwrap();
    assert_eq!(report["encoding"], "utf16le");
    assert_eq!(report["bom"], true);
    // Reopen never rewrites bytes.
    assert_eq!(std::fs::read(&note).unwrap(), utf16le_bytes(text, true));

    // `auto` (no explicit family) still reports the detected descriptor.
    let auto = env.run(&[
        "--vault",
        vault_arg,
        "--no-watch",
        "encoding",
        note_arg,
        "reopen",
        "--json",
    ]);
    assert!(
        auto.status.success(),
        "{}",
        String::from_utf8_lossy(&auto.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&auto.stdout).unwrap();
    assert_eq!(report["encoding"], "utf16le");
    assert_eq!(report["bom"], true);

    // A UTF-8+BOM note reports its own descriptor (family switching across
    // notes, exercising the utf8 path of the same command).
    let bom_note = vault.join("bom.md");
    std::fs::write(&bom_note, b"\xef\xbb\xbfbom note body\n").unwrap();
    let reopened = env.run(&[
        "--vault",
        vault_arg,
        "--no-watch",
        "encoding",
        bom_note.to_str().unwrap(),
        "reopen",
        "--encoding",
        "utf8",
        "--bom",
        "auto",
        "--json",
    ]);
    assert!(
        reopened.status.success(),
        "{}",
        String::from_utf8_lossy(&reopened.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&reopened.stdout).unwrap();
    assert_eq!(report["encoding"], "utf8");
    assert_eq!(report["bom"], true);
    assert_eq!(
        std::fs::read(&bom_note).unwrap(),
        b"\xef\xbb\xbfbom note body\n"
    );
}

/// Malformed bytes under an explicit family fail the command (non-zero exit)
/// — the CLI never presents a blank editable note for undecodable input.
#[test]
fn encoding_reopen_of_malformed_bytes_fails_without_a_blank_note() {
    let env = CliEnv::new();
    let vault_root = tempfile::tempdir().unwrap();
    let vault = vault_root.path().join("vault");
    init_vault(&env, &vault);

    let bad = vault.join("broken.md");
    std::fs::write(&bad, b"plain \xff bytes\n").unwrap();

    let result = env.run(&[
        "--vault",
        vault.to_str().unwrap(),
        "--no-watch",
        "encoding",
        bad.to_str().unwrap(),
        "reopen",
        "--encoding",
        "utf8",
        "--bom",
        "auto",
        "--json",
    ]);
    assert!(
        !result.status.success(),
        "malformed bytes must fail the reopen, stdout: {}",
        String::from_utf8_lossy(&result.stdout)
    );
    // The bytes are untouched: no silent rewrite, no blank note materialized.
    assert_eq!(std::fs::read(&bad).unwrap(), b"plain \xff bytes\n");
}

/// `convert` requires an explicit family; `auto` is refused before any core
/// command runs.
#[test]
fn encoding_convert_requires_explicit_family() {
    let env = CliEnv::new();
    let vault_root = tempfile::tempdir().unwrap();
    let vault = vault_root.path().join("vault");
    init_vault(&env, &vault);

    let note = vault.join("plain.md");
    std::fs::write(&note, b"plain text\n").unwrap();

    let result = env.run(&[
        "--vault",
        vault.to_str().unwrap(),
        "--no-watch",
        "encoding",
        note.to_str().unwrap(),
        "convert",
        "--json",
    ]);
    assert!(
        !result.status.success(),
        "convert without --encoding must fail, stdout: {}",
        String::from_utf8_lossy(&result.stdout)
    );
    // The refusal precedes any file effect.
    assert_eq!(std::fs::read(&note).unwrap(), b"plain text\n");
}
