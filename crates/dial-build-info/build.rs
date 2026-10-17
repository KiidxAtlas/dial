use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

/// Tracked inputs compiled into the executable: every workspace crate's sources, the manifests and
/// build scripts, and the files outside `src` embedded with `include_str!`/`include_bytes!` (keep
/// in sync when embedding a new file from outside `src`). Paths are relative to this crate, which
/// lives in `crates/dial-build-info`, and cover the whole workspace: `DIRTY` reports whether these
/// inputs differ from the checked-out commit. The script reruns only when they or the commit change.
///
/// Do not watch `.git/index`: Git, editors and dial itself rewrite it on every status refresh, and
/// each rerun of this script recompiles the whole crate even when no source changed.
const INPUTS: &[&str] = &[
    "../../src",
    "../../crates",
    "../../Cargo.toml",
    "../../Cargo.lock",
    "../../build.rs",
    "../../assets",
    "../../runtimes",
    "../../CHANGELOG.md",
    "../../examples/benchmarks/efficiency.toml",
    "../../python/dial_laya_server.py",
    "../../python/dial_server.py",
    "../../python/dial_steering.py",
    "../../python/dial_tool_grammar.py",
    "../../python/dial_worker.py",
    "../../python/laya-requirements.txt",
    "../../python/pyproject.toml",
];

fn git(args: &[&str]) -> Option<std::process::Output> {
    Command::new("git")
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(env::var_os("CARGO_MANIFEST_DIR")?)
        .output()
        .ok()
}

/// Watches an existing path; Cargo treats a missing path as always changed, which would rerun
/// this script and recompile the crate on every build.
fn rerun_if_exists(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn main() {
    for name in ["DIAL_BUILD_REVISION", "SOURCE_DATE_EPOCH"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    for path in INPUTS {
        println!("cargo:rerun-if-changed={path}");
    }
    // A linked worktree keeps HEAD in its own Git directory and branch refs in the common one.
    if let Some(output) = git(&["rev-parse", "--git-dir", "--git-common-dir"])
        .filter(|output| output.status.success())
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut dirs = stdout.lines().map(|line| PathBuf::from(line.trim()));
        if let (Some(git_dir), Some(common_dir)) = (dirs.next(), dirs.next()) {
            rerun_if_exists(&git_dir.join("HEAD"));
            rerun_if_exists(&common_dir.join("refs"));
            rerun_if_exists(&common_dir.join("packed-refs"));
        }
    }
    let revision = env::var("DIAL_BUILD_REVISION")
        .ok()
        .or_else(|| {
            git(&["rev-parse", "--short=12", "HEAD"])
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".into());
    assert!(
        revision == "unknown"
            || ((7..=40).contains(&revision.len())
                && revision.bytes().all(|byte| byte.is_ascii_hexdigit())),
        "DIAL_BUILD_REVISION must be a Git hash or unknown"
    );
    // A shipped binary without its commit makes bug reports and version checks useless. Dev and
    // local-profile builds keep the fallback for vendored sources without a Git checkout.
    if env::var("PROFILE").as_deref() == Ok("release") {
        assert!(
            revision != "unknown",
            "release builds need a revision: build from a Git checkout or set DIAL_BUILD_REVISION"
        );
    }
    let diff_args = [
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--quiet",
        "HEAD",
        "--",
    ];
    let dirty =
        match git(&[&diff_args[..], INPUTS].concat()).and_then(|output| output.status.code()) {
            Some(0) => "false",
            Some(1) => "true",
            _ => "unknown",
        };
    let epoch = env::var("SOURCE_DATE_EPOCH")
        .map(|value| {
            value
                .parse::<u64>()
                .expect("SOURCE_DATE_EPOCH must be an integer")
        })
        .unwrap_or_else(|_| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before Unix epoch")
                .as_secs()
        });
    for (name, value) in [
        ("REVISION", revision),
        ("DIRTY", dirty.into()),
        ("EPOCH", epoch.to_string()),
        ("TARGET", env::var("TARGET").unwrap()),
        ("PROFILE", env::var("PROFILE").unwrap()),
    ] {
        println!("cargo:rustc-env=DIAL_BUILD_{name}={value}");
    }
}
