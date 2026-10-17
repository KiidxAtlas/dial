use std::{fs, path::Path, process::Command};

pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Dial Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Dial Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("git must be installed for plugin tests");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Creates a git repository at `dir` whose single commit contains `dial-plugin.json` with
/// `manifest` and returns the commit SHA.
pub(crate) fn commit_plugin(dir: &Path, manifest: &str) -> String {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    fs::write(dir.join("dial-plugin.json"), manifest).unwrap();
    git(dir, &["add", "."]);
    git(
        dir,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "plugin"],
    );
    git(dir, &["rev-parse", "HEAD"])
}

pub(crate) fn write_plugin(dir: &Path, manifest: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("dial-plugin.json"), manifest).unwrap();
}
