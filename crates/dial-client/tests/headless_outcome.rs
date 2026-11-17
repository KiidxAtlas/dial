//! The `headless` example's exit status and output against a scripted engine: a settled turn
//! succeeds, a failed turn or an engine that exits first must not look like success.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Output};

/// The example binary. Cargo builds examples for `cargo test -p dial-client` and does not expose
/// their path to tests, so it is found beside the test executable's target profile.
fn headless_binary() -> PathBuf {
    let name = format!("headless{}", std::env::consts::EXE_SUFFIX);
    let test_exe = std::env::current_exe().expect("test executable path");
    test_exe
        .ancestors()
        .map(|dir| dir.join("examples").join(&name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("examples/{name} not built beside {}", test_exe.display()))
}

/// Run `headless` once against an engine that runs `script` (after its `ready` line).
fn run_headless(script: &str) -> Output {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("temp dir");
    let engine = dir.path().join("engine");
    std::fs::write(&engine, format!("#!/bin/sh\n{script}\n")).expect("write engine");
    std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    Command::new(headless_binary())
        .arg("summarize the repository")
        .env("DIAL_BIN", &engine)
        .env_remove("DIAL_RPC_ARGS")
        .output()
        .expect("run headless example")
}

const READY: &str = "printf '%s\\n' '{\"event\":\"ready\",\"protocol_version\":1,\"methods\":[\"send\",\"shutdown\"]}'";
/// Answer the example's `shutdown` call (id 2) so it exits through its normal path.
const SHUTDOWN_REPLY: &str = "IFS= read -r shutdown\nprintf '%s\\n' '{\"id\":2,\"result\":{}}'";

#[test]
fn a_settled_turn_exits_zero_and_reports_its_outcome() {
    let output = run_headless(&format!(
        "{READY}\nIFS= read -r send\nprintf '%s\\n' '{{\"event\":\"text\",\"delta\":\"done\"}}' '{{\"id\":1,\"result\":{{\"outcome\":\"completed\"}}}}'\n{SHUTDOWN_REPLY}"
    ));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("done"));
    assert!(stderr.contains("turn finished: \"completed\""), "{stderr}");
}

#[test]
fn a_failed_turn_exits_non_zero_and_names_the_error() {
    let output = run_headless(&format!(
        "{READY}\nIFS= read -r send\nprintf '%s\\n' '{{\"id\":1,\"error\":{{\"code\":\"busy\",\"message\":\"a turn is running\"}}}}'\n{SHUTDOWN_REPLY}"
    ));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "a failed turn must not exit 0");
    assert!(
        stderr.contains("turn failed: busy: a turn is running"),
        "{stderr}"
    );
}

#[test]
fn an_engine_that_exits_before_the_turn_settles_exits_non_zero() {
    let output = run_headless(&format!("{READY}\nIFS= read -r send"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "an unsettled turn must not exit 0"
    );
    assert!(stderr.contains("engine exited"), "{stderr}");
}
