//! The `tui` subcommand: the headless smoke test (lab fixture kept in
//! $TMPDIR, never deleted) and the argument checks that run before the UI.

use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_office-license-removal");

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn smoke_passes() {
    let out = run(&["tui", "--smoke"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("SMOKE OK"),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn ui_needs_a_terminal() {
    let out = run(&["tui"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs a terminal"));
}

#[test]
fn lab_mode_needs_both_paths() {
    let out = run(&["tui", "--home", "/tmp"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs both --home and --system-root"));
}

#[test]
fn half_a_lab_environment_is_an_error_not_a_real_run() {
    let out = Command::new(BIN)
        .arg("tui")
        .env("OLR_LAB_HOME", "/tmp")
        .env_remove("OLR_LAB_SYSTEM_ROOT")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs both"));
}
