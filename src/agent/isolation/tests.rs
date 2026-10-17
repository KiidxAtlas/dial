use super::*;

fn project() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "a1").unwrap();
    fs::write(dir.path().join("b.txt"), "b1").unwrap();
    fs::write(dir.path().join("gone.txt"), "x").unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, ws)
}

#[test]
fn development_copy_has_private_git_and_dependencies_and_obeys_source_limits() {
    let (dir, ws) = project();
    crate::tools::git::initialize_private_snapshot(dir.path()).unwrap();
    fs::create_dir_all(dir.path().join("node_modules/pkg/node_modules/nested")).unwrap();
    fs::write(
        dir.path()
            .join("node_modules/pkg/node_modules/nested/index.js"),
        "module.exports=7;",
    )
    .unwrap();
    let iso = Isolated::create(&ws).unwrap();
    assert!(iso.root().join(".git/HEAD").exists());
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(iso.root())
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        status.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );
    let dep = "node_modules/pkg/node_modules/nested/index.js";
    fs::write(iso.root().join(dep), "changed").unwrap();
    let merge = iso.merge(&ws, &AtomicBool::new(false));
    assert!(merge.applied.is_empty());
    assert_eq!(
        fs::read_to_string(dir.path().join(dep)).unwrap(),
        "module.exports=7;"
    );
    let limits = super::super::agent_defs::TaskConfig {
        isolation_source_files: 1,
        ..Default::default()
    };
    assert!(Isolated::with_config(&ws, &limits, Preference::Auto).is_err());
}

#[test]
fn changes_merge_back_and_the_copy_is_discarded() {
    let (dir, ws) = project();
    let iso = Isolated::create(&ws).unwrap();
    assert_ne!(iso.root(), ws.root());
    fs::write(iso.root().join("a.txt"), "a2").unwrap();
    fs::write(iso.root().join("new.txt"), "n").unwrap();
    fs::remove_file(iso.root().join("gone.txt")).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "a1",
        "project untouched while the worker runs"
    );
    let root = iso.root().to_path_buf();
    let merge = iso.merge(&ws, &AtomicBool::new(false));
    assert_eq!(merge.applied.len(), 3);
    assert!(merge.conflicts.is_empty() && merge.kept.is_none());
    assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "a2");
    assert_eq!(fs::read_to_string(dir.path().join("new.txt")).unwrap(), "n");
    assert!(!dir.path().join("gone.txt").exists());
    assert!(!root.exists(), "copy removed");
}

/// An overlay upper layer records a deletion as a whiteout under the deleted name; merging must
/// treat it as the deletion, and anything else in that place as a reappeared file.
#[cfg(target_os = "linux")]
#[test]
fn overlay_whiteouts_materialize_as_deletions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gone.txt");
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: `name` is a valid NUL-terminated path for the duration of the call.
    if unsafe { libc::mknod(name.as_ptr(), libc::S_IFCHR | 0o600, 0) } != 0 {
        // Unprivileged whiteouts need Linux 5.8+; the overlay backend is unavailable without them.
        return;
    }
    assert!(materialize(&path, None, true).unwrap().is_none());
    assert!(materialize(&path, None, false).is_err());
}

#[test]
fn concurrent_project_edits_are_never_overwritten() {
    let (dir, ws) = project();
    let iso = Isolated::create(&ws).unwrap();
    fs::write(iso.root().join("a.txt"), "worker").unwrap();
    fs::write(iso.root().join("b.txt"), "worker").unwrap();
    fs::write(dir.path().join("a.txt"), "user").unwrap();
    let merge = iso.merge(&ws, &AtomicBool::new(false));
    assert_eq!(merge.applied, vec![PathBuf::from("b.txt")]);
    assert_eq!(merge.conflicts, vec![PathBuf::from("a.txt")]);
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "user"
    );
    let kept = merge.kept.clone().expect("copy kept for manual merge");
    assert_eq!(fs::read_to_string(kept.join("a.txt")).unwrap(), "worker");
    assert!(merge.describe().contains("CONFLICT"));
    fs::remove_dir_all(kept).unwrap();
}

#[test]
fn two_workers_on_disjoint_files_both_land_and_identical_edits_are_not_conflicts() {
    let (dir, ws) = project();
    let (one, two) = (
        Isolated::create(&ws).unwrap(),
        Isolated::create(&ws).unwrap(),
    );
    fs::write(one.root().join("a.txt"), "one").unwrap();
    fs::write(two.root().join("b.txt"), "two").unwrap();
    fs::write(two.root().join("a.txt"), "one").unwrap();
    assert!(one.merge(&ws, &AtomicBool::new(false)).conflicts.is_empty());
    let merge = two.merge(&ws, &AtomicBool::new(false));
    assert!(merge.conflicts.is_empty(), "{merge:?}");
    assert_eq!(fs::read_to_string(dir.path().join("b.txt")).unwrap(), "two");
}

#[test]
fn build_output_and_secrets_in_the_copy_never_merge() {
    let (dir, ws) = project();
    fs::write(dir.path().join(".env"), "SECRET").unwrap();
    let iso = Isolated::create(&ws).unwrap();
    assert!(!iso.root().join(".env").exists(), "secrets are not copied");
    fs::create_dir_all(iso.root().join("target")).unwrap();
    fs::write(iso.root().join("target/out.bin"), "junk").unwrap();
    fs::write(iso.root().join(".env"), "PLANTED").unwrap();
    let merge = iso.merge(&ws, &AtomicBool::new(false));
    assert!(
        merge.applied.is_empty() && merge.conflicts.is_empty(),
        "{merge:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join(".env")).unwrap(),
        "SECRET"
    );
}

fn git_project() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "a1").unwrap();
    fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=dial",
        "-c",
        "user.email=dial@example.invalid",
        "commit",
        "-q",
        "-m",
        "init",
    ]);
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, ws)
}

fn round_trip(preference: Preference, expected_backend: &str) {
    let (dir, ws) = git_project();
    let iso = Isolated::with_config(
        &ws,
        &super::super::agent_defs::TaskConfig::default(),
        preference,
    )
    .unwrap();
    let root = iso.root().to_path_buf();
    assert_ne!(root, ws.root());
    fs::write(root.join("a.txt"), "a2").unwrap();
    fs::write(root.join("new.txt"), "n").unwrap();
    let merge = iso.merge(&ws, &AtomicBool::new(false));
    assert!(
        merge.conflicts.is_empty() && merge.errors.is_empty(),
        "{merge:?}"
    );
    assert!(
        merge.backend.starts_with(expected_backend),
        "{}",
        merge.backend
    );
    assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "a2");
    assert_eq!(fs::read_to_string(dir.path().join("new.txt")).unwrap(), "n");
    assert!(!root.exists(), "copy removed");
    let list = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&list.stdout);
    assert_eq!(listed.matches("worktree ").count(), 1, "{listed}");
}

#[test]
fn worktree_backend_merges_back_and_removes_its_metadata() {
    round_trip(Preference::Worktree, "worktree");
}

#[test]
fn copy_backend_merges_back_and_is_removed() {
    round_trip(Preference::Copy, "copy");
}

/// Sparse files keep the oversized fixtures cheap: only the logical length is set.
#[test]
fn oversized_project_file_is_reported_by_path_before_any_copy() {
    let (dir, ws) = project();
    fs::File::create(dir.path().join("model.bin"))
        .unwrap()
        .set_len(MAX_BYTES as u64 + 1)
        .unwrap();
    let error = format!(
        "{:#}",
        Isolated::create(&ws)
            .err()
            .expect("an oversized file is refused")
    );
    for expected in [
        "preflight",
        "no files were copied",
        "model.bin",
        &(MAX_BYTES + 1).to_string(),
        &MAX_BYTES.to_string(),
    ] {
        assert!(error.contains(expected), "{error}");
    }
    assert_eq!(
        fs::metadata(dir.path().join("model.bin")).unwrap().len(),
        MAX_BYTES as u64 + 1,
        "the project file is untouched"
    );
}

#[test]
fn worker_file_growing_past_the_limit_is_reported_by_path() {
    let (_dir, ws) = project();
    let iso = Isolated::with_config(
        &ws,
        &super::super::agent_defs::TaskConfig::default(),
        Preference::Copy,
    )
    .unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(iso.root().join("b.txt"))
        .unwrap()
        .set_len(MAX_BYTES as u64 + 1)
        .unwrap();
    let error = format!(
        "{:#}",
        iso.current().expect_err("growth past the limit is refused")
    );
    assert!(error.contains("b.txt"), "{error}");
    assert!(error.contains(&(MAX_BYTES + 1).to_string()), "{error}");
}

/// Build outputs and scratch directories are never copied, so a large `bin/dial` cannot fail a
/// copy worker. A real oversized source file is still refused, and the error names its path.
#[test]
fn generated_build_outputs_do_not_block_a_copy_worker_but_oversized_source_is_reported() {
    let (dir, ws) = project();
    fs::write(dir.path().join(".gitignore"), "/bin/\n").unwrap();
    let generated = [
        "bin/dial",
        "bin/dial.bak-20261015-131809",
        ".cargo-work/cache/librustc.rlib",
        ".tmp/dial-debug-dump-1",
    ];
    for relative in generated {
        let path = dir.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_BYTES as u64 + 1)
            .unwrap();
    }
    let config = super::super::agent_defs::TaskConfig::default();
    let iso = Isolated::with_config(&ws, &config, Preference::Copy)
        .expect("generated build outputs must not block the worker copy");
    assert_eq!(fs::read_to_string(iso.root().join("a.txt")).unwrap(), "a1");
    for relative in generated {
        assert!(
            !iso.root().join(relative).exists(),
            "{relative} is a build output and is not copied"
        );
        assert_eq!(
            fs::metadata(dir.path().join(relative)).unwrap().len(),
            MAX_BYTES as u64 + 1,
            "the project copy of {relative} is untouched"
        );
    }

    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::File::create(dir.path().join("src/model.bin"))
        .unwrap()
        .set_len(MAX_BYTES as u64 + 1)
        .unwrap();
    let error = format!(
        "{:#}",
        Isolated::with_config(&ws, &config, Preference::Copy)
            .err()
            .expect("oversized source is refused")
    );
    for expected in [
        "preflight",
        "src/model.bin",
        &(MAX_BYTES + 1).to_string(),
        "not configurable",
    ] {
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn tracked_bin_scripts_survive_copy_and_merge_without_private_git() {
    let (dir, ws) = project();
    fs::create_dir_all(dir.path().join("bin")).unwrap();
    fs::write(dir.path().join("bin/script"), "original").unwrap();
    crate::tools::git::initialize_private_snapshot(dir.path()).unwrap();
    fs::write(dir.path().join(".gitignore"), "/bin/\n").unwrap();
    fs::File::create(dir.path().join("bin/dial"))
        .unwrap()
        .set_len(MAX_BYTES as u64 + 1)
        .unwrap();
    let config = super::super::agent_defs::TaskConfig {
        isolation_git: false,
        ..Default::default()
    };
    let iso = Isolated::with_config(&ws, &config, Preference::Copy).unwrap();
    assert_eq!(
        fs::read_to_string(iso.root().join("bin/script")).unwrap(),
        "original"
    );
    assert!(!iso.root().join("bin/dial").exists());
    fs::write(iso.root().join("bin/script"), "updated").unwrap();
    let merged = iso.merge(&ws, &AtomicBool::new(false));
    assert!(
        merged.errors.is_empty() && merged.conflicts.is_empty(),
        "{merged:?}"
    );
    assert!(
        merged.applied.contains(&PathBuf::from("bin/script")),
        "{merged:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("bin/script")).unwrap(),
        "updated"
    );
}
