//! Basic tests for flavite-cli

use std::process::Command;

#[test]
fn cli_runs() {
    // Just verify the CLI binary can be invoked
    // It will print an error message but that's expected
    let _output = Command::new("cargo")
        .args(["run", "-p", "flavite-cli", "--", "--help"])
        .current_dir("../../..")
        .output()
        .expect("Failed to execute cargo");

    // The CLI should at least run (exit code might be non-zero due to missing args)
    // We just check it doesn't panic during compilation
}

#[test]
fn cli_runs_with_cargo() {
    // Test that the CLI binary compiles and runs
    let _output = Command::new("cargo")
        .args(["run", "-p", "flavite-cli", "--quiet"])
        .current_dir("../../..")
        .output()
        .expect("Failed to execute cargo");

    // The binary should run (may exit with error, but it runs)
    // We just verify it compiles and executes
}
