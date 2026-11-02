//! Shared isolation for commit-flow Git fixtures.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// Keep ordinary fixtures independent of executable drivers in operator configuration.
/// Git resolves includes and worktree configuration; only this fixture's local values change.
/// Canary tests install their executable commands after this baseline has been established.
pub(crate) fn disable_inherited_git_drivers(root: &Path) {
    let drivers = Command::new("git")
        .args(crate::tools::vcs::git::safe_git_args())
        .args([
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            r"^(filter\..*\.(clean|smudge|process)|diff\..*\.textconv)$",
        ])
        .current_dir(root)
        .output()
        .expect("query fixture Git drivers");
    assert!(
        drivers.status.success() || drivers.status.code() == Some(1),
        "query fixture Git drivers: {}",
        String::from_utf8_lossy(&drivers.stderr)
    );
    let keys = String::from_utf8(drivers.stdout).expect("UTF-8 Git driver names");
    for key in keys
        .split('\0')
        .filter(|key| !key.is_empty())
        .collect::<BTreeSet<_>>()
    {
        let result = Command::new("git")
            .args(crate::tools::vcs::git::safe_git_args())
            .args(["config", "--local", key, ""])
            .current_dir(root)
            .output()
            .expect("disable inherited fixture Git driver");
        assert!(
            result.status.success(),
            "disable fixture Git driver {key}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
