use super::super::tests::Fixture;
use super::*;

fn covered(cp: &Checkpoints) -> usize {
    match cp.snapshot_for_shell() {
        ShellCoverage::Covered { files } => files,
        ShellCoverage::Unavailable(reason) => panic!("no pre-image: {reason}"),
    }
}

#[test]
fn shell_changes_become_entries_that_rollback_restores() {
    let fx = Fixture::new();
    fx.write("keep.txt", "keep");
    fx.write("src/edit.txt", "before");
    fx.write("gone.db-wal", "frames");
    let cp = fx.open();
    let turn = cp.begin_turn("shell").unwrap();
    assert_eq!(covered(&cp), 3);

    fx.write("src/edit.txt", "after, and longer");
    fs::remove_file(fx.path("gone.db-wal")).unwrap();
    fx.write("made/new.txt", "created");
    let mut recorded = cp.record_shell_changes().unwrap();
    recorded.sort();
    let expected = ["gone.db-wal", "made/new.txt", "src/edit.txt"];
    assert_eq!(recorded, expected);
    assert_eq!(cp.changed_files(turn), expected);
    assert!(
        cp.record_shell_changes().unwrap().is_empty(),
        "a recorded path is not recorded twice"
    );

    let report = cp.rollback(1).unwrap();
    assert!(report.warnings.is_empty(), "{report:?}");
    assert_eq!(fx.read("src/edit.txt").as_deref(), Some("before"));
    assert_eq!(fx.read("gone.db-wal").as_deref(), Some("frames"));
    assert_eq!(fx.read("made/new.txt"), None);
    assert!(
        !fx.path("made").exists(),
        "the created directory is removed"
    );
    assert_eq!(fx.read("keep.txt").as_deref(), Some("keep"));
}

#[test]
fn same_length_rewrite_is_detected_and_identical_rewrite_is_not() {
    let fx = Fixture::new();
    fx.write("a.txt", "aaaa");
    fx.write("b.txt", "bbbb");
    let cp = fx.open();
    cp.begin_turn("shell").unwrap();
    covered(&cp);
    // `sed -i` style replacement with the same length, and a rewrite of identical bytes.
    fs::remove_file(fx.path("a.txt")).unwrap();
    fx.write("a.txt", "AAAA");
    fs::remove_file(fx.path("b.txt")).unwrap();
    fx.write("b.txt", "bbbb");
    assert_eq!(cp.record_shell_changes().unwrap(), ["a.txt"]);
}

#[test]
fn tool_edits_keep_their_own_pre_image() {
    let fx = Fixture::new();
    fx.write("a.txt", "original");
    let cp = fx.open();
    cp.begin_turn("mixed").unwrap();
    // The tool edit precedes the pre-image, which therefore holds the edited content.
    cp.before_write(&fx.path("a.txt")).unwrap();
    fx.write("a.txt", "tool edit");
    covered(&cp);
    fx.write("a.txt", "shell edit on top");
    cp.before_write(&fx.path("b.txt")).unwrap();
    fx.write("b.txt", "tool created");
    assert!(cp.record_shell_changes().unwrap().is_empty());
    cp.rollback(1).unwrap();
    assert_eq!(fx.read("a.txt").as_deref(), Some("original"));
    assert_eq!(fx.read("b.txt"), None);
}

#[test]
fn ignored_dependency_and_secret_paths_are_not_covered() {
    let fx = Fixture::new();
    fx.write(".gitignore", "*.log\n");
    fx.write("src/lib.rs", "code");
    fx.write("debug.log", "noise");
    fx.write("node_modules/pkg/index.js", "dep");
    fx.write(".env", "TOKEN=secret");
    fx.write(".git/HEAD", "ref");
    let cp = fx.open();
    cp.begin_turn("shell").unwrap();
    assert_eq!(covered(&cp), 2, ".gitignore and src/lib.rs");
    fx.write("debug.log", "more noise");
    fx.write("node_modules/pkg/index.js", "changed");
    fx.write(".env", "TOKEN=other");
    fx.write("target/out.o", "object");
    assert!(cp.record_shell_changes().unwrap().is_empty());
    let saved: Vec<_> = fs::read_dir(cp.dir.join(STORE))
        .unwrap()
        .map(|entry| fs::read(entry.unwrap().path()).unwrap())
        .collect();
    assert!(saved.iter().all(|bytes| bytes != b"TOKEN=secret"));
}

#[test]
fn pre_image_is_per_turn_and_absent_without_one() {
    let fx = Fixture::new();
    fx.write("a.txt", "one");
    let cp = fx.open();
    assert_eq!(
        cp.snapshot_for_shell(),
        ShellCoverage::Unavailable("no checkpoint turn is active".into())
    );
    cp.begin_turn("first").unwrap();
    covered(&cp);
    fx.write("a.txt", "two");
    // The first turn ends without a comparison; the next turn must not inherit its pre-image.
    let second = cp.begin_turn("second").unwrap();
    assert!(!cp.dir.join(STORE).exists());
    assert!(cp.record_shell_changes().unwrap().is_empty());
    covered(&cp);
    fx.write("a.txt", "three");
    assert_eq!(cp.record_shell_changes().unwrap(), ["a.txt"]);
    assert_eq!(
        cp.turn_versions(second).unwrap()[0].before.as_deref(),
        Some(&b"two"[..])
    );
}

#[test]
fn oversized_file_is_listed_as_skipped_not_restored() {
    let fx = Fixture::new();
    let big = fx.path("big.bin");
    fs::File::create(&big)
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    let cp = fx.open();
    let turn = cp.begin_turn("shell").unwrap();
    covered(&cp);
    assert!(!cp.dir.join(STORE).join("0").exists(), "not copied");
    fs::write(&big, "truncated").unwrap();
    assert_eq!(cp.record_shell_changes().unwrap(), ["big.bin"]);
    assert_eq!(cp.changed_files(turn), ["big.bin"]);
    let report = cp.rollback(1).unwrap();
    assert!(
        report.warnings.iter().any(|w| w.contains("too large")),
        "{report:?}"
    );
}

#[cfg(unix)]
#[test]
fn mode_change_is_restored_and_symlinks_are_left_alone() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new();
    fx.write("run.sh", "#!/bin/sh\n");
    fx.write("real.txt", "real");
    std::os::unix::fs::symlink(fx.path("real.txt"), fx.path("link.txt")).unwrap();
    fs::set_permissions(fx.path("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
    let cp = fx.open();
    cp.begin_turn("shell").unwrap();
    assert_eq!(covered(&cp), 2);
    fs::set_permissions(fx.path("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_file(fx.path("link.txt")).unwrap();
    assert_eq!(cp.record_shell_changes().unwrap(), ["run.sh"]);
    cp.rollback(1).unwrap();
    let mode = fs::metadata(fx.path("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o644);
}
