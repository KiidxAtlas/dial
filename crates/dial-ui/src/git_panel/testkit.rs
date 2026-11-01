//! Shared Git fixtures for repository services and their UI consumers.
use std::path::Path;

pub(crate) fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    if args.first() == Some(&"init") {
        // Keep operator-global executable filters out of ordinary fixtures, without
        // changing the process environment or weakening the production Git guard.
        let filters = std::process::Command::new("git")
            .current_dir(root)
            .args([
                "config",
                "--null",
                "--name-only",
                "--get-regexp",
                r"^filter\..*\.(clean|smudge|process)$",
            ])
            .output()
            .expect("query fixture filters");
        assert!(filters.status.success() || filters.status.code() == Some(1));
        let keys = String::from_utf8(filters.stdout).expect("UTF-8 filter names");
        for key in keys.split('\0').filter(|key| !key.is_empty()) {
            git(root, &["config", "--local", key, ""]);
        }
    }
    String::from_utf8_lossy(&output.stdout).into_owned()
}
