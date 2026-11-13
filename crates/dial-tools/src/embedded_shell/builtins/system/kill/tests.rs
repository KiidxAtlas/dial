//! Windows `kill`, run through the embedded shell on a real child process. A `ping` child is
//! started and killed by PID, so the test needs no console or job-control state.

use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::embedded_shell::builtins::tests::run_script;
use dial_core::platform::process::process_exists;
use dial_core::platform::processes;
use tempfile::tempdir;

#[test]
fn kill_checks_and_terminates_a_process_by_pid() {
    let mut child = Command::new("ping")
        .args(["-n", "60", "127.0.0.1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let dir = tempdir().unwrap();
    let dir = dir.path();
    assert_eq!(
        run_script(dir, &format!("kill -0 {pid}")),
        0,
        "running process"
    );
    assert_eq!(
        run_script(dir, &format!("kill -s TERM {pid}")),
        0,
        "terminate"
    );
    let status = child.wait().unwrap();
    assert!(
        !status.success(),
        "killed child reports failure: {status:?}"
    );
    assert_eq!(
        run_script(dir, &format!("kill -0 {pid}")),
        1,
        "exited process"
    );
    assert_eq!(run_script(dir, &format!("kill {pid}")), 1, "already gone");
}

#[test]
fn kill_usage_and_listing_follow_the_documented_forms() {
    let dir = tempdir().unwrap();
    let dir = dir.path();
    let cases: &[(&str, u8)] = &[
        ("kill", 2),
        ("kill -s", 2),
        ("kill -99 1", 1),
        ("kill abc", 2),
        ("kill %9", 1),
        ("kill 4000000000", 1),
        ("kill -- -0", 2),
        ("kill -- -x", 2),
        ("kill -- -99999999", 1),
        ("kill -l 9 > out.txt", 0),
    ];
    for (script, expected) in cases {
        assert_eq!(run_script(dir, script), *expected, "script: {script}");
    }
    assert_eq!(fs::read_to_string(dir.join("out.txt")).unwrap(), "KILL\n");
}

/// Killing a `cmd.exe` (what a `.cmd` shim runs) ends the `ping` it launched too: the whole tree
/// goes, not only the named process.
#[test]
fn kill_terminates_the_whole_tree_of_a_named_process() {
    let mut shim = Command::new("cmd")
        .args(["/C", "ping -n 60 127.0.0.1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let shim_pid = shim.id();
    let deadline = Instant::now() + Duration::from_secs(20);
    let grandchild = loop {
        let found = processes::snapshot()
            .unwrap()
            .into_iter()
            .find(|entry| entry.parent == shim_pid && entry.image.eq_ignore_ascii_case("PING.EXE"));
        if let Some(entry) = found {
            break entry.pid;
        }
        assert!(Instant::now() < deadline, "cmd.exe never launched ping");
        thread::sleep(Duration::from_millis(50));
    };
    let dir = tempdir().unwrap();
    assert_eq!(
        run_script(dir.path(), &format!("kill -s TERM {shim_pid}")),
        0,
        "terminate the tree"
    );
    let _ = shim.wait();
    while process_exists(grandchild).unwrap() {
        assert!(Instant::now() < deadline, "ping outlived its cmd.exe");
        thread::sleep(Duration::from_millis(50));
    }
}
