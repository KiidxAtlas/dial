//! End-to-end `task` isolation with a scripted local provider: a worker writes a file in its
//! private copy, and the parent merges it back. Runs offline; no provider credentials needed.
use super::cli_print::{Server, call, text};
use serde_json::json;
use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

fn git(dir: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(
        dir.path(),
        &["config", "user.email", "dial@example.invalid"],
    );
    git(dir.path(), &["config", "user.name", "Dial"]);
    fs::write(dir.path().join("base.txt"), "base\n").unwrap();
    git(dir.path(), &["add", "base.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "base"]);
    dir
}

/// Run the scripted parent → worker → parent conversation with `backend` and check the merge.
fn merges_a_worker_write(backend: &str) {
    let server = Server::new(vec![
        call(
            "task",
            json!({
                "agent": "worker",
                "task": "Create worker-output.txt with one line.",
                "solution_space": "Write exactly one file.",
                "isolation": "copy"
            }),
        ),
        call(
            "write",
            json!({"path": "worker-output.txt", "content": "from worker\n"}),
        ),
        text("worker finished"),
        text("merged"),
    ]);
    let home = tempfile::tempdir().unwrap();
    fs::write(
        home.path().join("config.toml"),
        format!(
            "integrations = []\n[improve]\nenabled = false\n[isolation]\nbackend = \"{backend}\"\n"
        ),
    )
    .unwrap();
    let project = repository();

    let mut child = Command::new(env!("CARGO_BIN_EXE_dial"))
        .env("DIAL_HOME", home.path())
        .env("NO_COLOR", "1")
        .current_dir(project.path())
        .args([
            "-p",
            "--endpoint",
            &server.url,
            "--model",
            "scripted",
            "--sandbox",
            "off",
            "-y",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Create the file through a worker.")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "dial failed ({backend}):\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        fs::read_to_string(project.path().join("worker-output.txt")).unwrap(),
        "from worker\n",
        "the worker's write must merge back into the project ({backend})"
    );
    let worktrees = Command::new("git")
        .current_dir(project.path())
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&worktrees.stdout)
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count();
    assert_eq!(
        listed, 1,
        "no worktree metadata may outlive the worker ({backend})"
    );
}

#[test]
fn worktree_backend_merges_a_worker_write_and_leaves_no_worktree() {
    merges_a_worker_write("worktree");
}

#[test]
fn copy_backend_merges_a_worker_write() {
    merges_a_worker_write("copy");
}
