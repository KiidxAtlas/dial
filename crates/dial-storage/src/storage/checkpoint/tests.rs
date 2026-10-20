use super::*;

pub(super) struct Fixture {
    _state: tempfile::TempDir,
    project: tempfile::TempDir,
    dir: PathBuf,
}

impl Fixture {
    pub(super) fn new() -> Self {
        let state = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let dir = state.path().join("session-1");
        Self {
            _state: state,
            project,
            dir,
        }
    }
    pub(super) fn root(&self) -> PathBuf {
        fs::canonicalize(self.project.path()).unwrap()
    }
    pub(super) fn open(&self) -> Checkpoints {
        Checkpoints::new(self.dir.clone(), self.root()).unwrap()
    }
    pub(super) fn path(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }
    pub(super) fn write(&self, rel: &str, text: &str) {
        let p = self.path(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    pub(super) fn read(&self, rel: &str) -> Option<String> {
        fs::read_to_string(self.path(rel)).ok()
    }
}
#[test]
fn changed_paths_are_relative_and_scoped_to_their_turn() {
    let fixture = Fixture::new();
    let checkpoint = fixture.open();
    fixture.write("src/lib.rs", "before");
    let first = checkpoint.begin_turn("edit").unwrap();
    checkpoint
        .before_write(&fixture.path("src/lib.rs"))
        .unwrap();
    assert_eq!(checkpoint.changed_files(first), vec!["src/lib.rs"]);
    assert!(checkpoint.changed_files(first + 1).is_empty());

    let second = checkpoint.begin_turn("next").unwrap();
    fixture.write("README.md", "new");
    checkpoint.before_write(&fixture.path("README.md")).unwrap();
    // A completed turn keeps its inventory: `app.rs` lets the user review any earlier turn.
    assert_eq!(checkpoint.changed_files(first), vec!["src/lib.rs"]);
    assert_eq!(checkpoint.changed_files(second), vec!["README.md"]);
}

#[test]
fn repair_sources_are_exact_and_survive_further_edits_and_rollback() {
    let fixture = Fixture::new();
    let checkpoint = fixture.open();
    fixture.write("src/lib.rs", "original");
    let turn = checkpoint.begin_turn("repair").unwrap();
    modify(&checkpoint, &fixture, "src/lib.rs", "failing");
    let failure = checkpoint.repair_changes(turn).unwrap();
    modify(&checkpoint, &fixture, "src/lib.rs", "passing");
    let pass = checkpoint.repair_changes(turn).unwrap();
    assert_ne!(failure[0].after_sha256, pass[0].after_sha256);
    assert_eq!(failure[0].before_sha256, pass[0].before_sha256);
    checkpoint.rollback(1).unwrap();
    assert_eq!(
        fs::read_to_string(failure[0].before_ref.as_ref().unwrap()).unwrap(),
        "original"
    );
    assert_eq!(
        fs::read_to_string(failure[0].after_ref.as_ref().unwrap()).unwrap(),
        "failing"
    );
    assert_eq!(
        fs::read_to_string(pass[0].after_ref.as_ref().unwrap()).unwrap(),
        "passing"
    );
    assert!(checkpoint.list().is_empty());
}

#[test]
fn repair_sources_represent_creation_and_deletion() {
    let fixture = Fixture::new();
    let checkpoint = fixture.open();
    let turn = checkpoint.begin_turn("create").unwrap();
    modify(&checkpoint, &fixture, "new.rs", "new");
    let created = checkpoint.repair_changes(turn).unwrap();
    assert_eq!(created[0].before_sha256, None);
    assert_eq!(created[0].before_ref, None);
    assert!(created[0].after_ref.is_some());
    fs::remove_file(fixture.path("new.rs")).unwrap();
    let removed = checkpoint.repair_changes(turn).unwrap();
    assert_eq!(removed[0].after_sha256, None);
    assert_eq!(removed[0].after_ref, None);
    assert!(checkpoint.repair_changes(turn + 1).is_err());
}

/// Mimic a tool: record, then mutate.
fn modify(cp: &Checkpoints, fx: &Fixture, rel: &str, text: &str) {
    cp.before_write(&fx.path(rel)).unwrap();
    fx.write(rel, text);
}

#[test]
fn turn_versions_pair_saved_content_with_current_content() {
    let fx = Fixture::new();
    let cp = fx.open();
    fx.write("a.txt", "A0");
    fx.write("gone.txt", "G0");
    let turn = cp.begin_turn("edit").unwrap();
    modify(&cp, &fx, "a.txt", "A1");
    modify(&cp, &fx, "new.txt", "N1");
    cp.before_write(&fx.path("gone.txt")).unwrap();
    fs::remove_file(fx.path("gone.txt")).unwrap();
    let mut versions = cp.turn_versions(turn).unwrap();
    versions.sort_by(|a, b| a.path.cmp(&b.path));
    let view: Vec<_> = versions
        .iter()
        .map(|v| {
            (
                v.path.as_str(),
                v.in_project,
                v.before.as_deref(),
                v.after.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        view,
        vec![
            ("a.txt", true, Some(&b"A0"[..]), Some(&b"A1"[..])),
            ("gone.txt", true, Some(&b"G0"[..]), None),
            ("new.txt", true, None, Some(&b"N1"[..])),
        ]
    );
    assert!(cp.turn_versions(turn + 1).is_err());
}

#[test]
fn rolls_back_modified_created_and_deleted_files() {
    let fx = Fixture::new();
    fx.write("a.txt", "A0");
    fx.write("b.txt", "B0");
    let cp = fx.open();
    cp.begin_turn("turn one").unwrap();
    modify(&cp, &fx, "a.txt", "A1");
    modify(&cp, &fx, "new/deep/c.txt", "C1");
    cp.before_write(&fx.path("b.txt")).unwrap();
    fs::remove_file(fx.path("b.txt")).unwrap();

    let report = cp.rollback(1).unwrap();
    assert_eq!(fx.read("a.txt").as_deref(), Some("A0"));
    assert_eq!(fx.read("b.txt").as_deref(), Some("B0"));
    assert!(!fx.path("new/deep/c.txt").exists());
    assert!(
        !fx.path("new").exists(),
        "directories created by the turn are cleaned up"
    );
    let mut restored = report.restored.clone();
    restored.sort();
    assert_eq!(restored, ["a.txt", "b.txt"]);
    assert_eq!(report.removed, ["new/deep/c.txt"]);
    assert!(report.warnings.is_empty());
    assert!(cp.list().is_empty(), "consumed checkpoints are gone");
    assert!(cp.rollback(1).is_err(), "nothing left to roll back");
}

#[test]
fn only_the_first_write_per_turn_is_recorded() {
    let fx = Fixture::new();
    fx.write("a.txt", "A0");
    let cp = fx.open();
    cp.begin_turn("t").unwrap();
    modify(&cp, &fx, "a.txt", "A1");
    modify(&cp, &fx, "a.txt", "A2");
    modify(&cp, &fx, "a.txt", "A3");
    assert_eq!(
        cp.list(),
        [CheckpointInfo {
            turn: 1,
            label: "t".into(),
            files: 1
        }]
    );
    cp.rollback(1).unwrap();
    assert_eq!(fx.read("a.txt").as_deref(), Some("A0"));
}

#[test]
fn multiple_turns_roll_back_newest_first_and_partially() {
    let fx = Fixture::new();
    fx.write("a.txt", "A0");
    let cp = fx.open();
    cp.begin_turn("one").unwrap();
    modify(&cp, &fx, "a.txt", "A1");
    modify(&cp, &fx, "x.txt", "X1");
    cp.begin_turn("two").unwrap();
    modify(&cp, &fx, "a.txt", "A2");
    cp.begin_turn("read only").unwrap();
    cp.begin_turn("three").unwrap();
    modify(&cp, &fx, "a.txt", "A3");
    modify(&cp, &fx, "x.txt", "X3");

    let labels: Vec<_> = cp
        .list()
        .into_iter()
        .map(|c| (c.turn, c.label, c.files))
        .collect();
    assert_eq!(
        labels,
        [
            (4, "three".to_string(), 2),
            (2, "two".to_string(), 1),
            (1, "one".to_string(), 2)
        ]
    );

    cp.rollback(1).unwrap();
    assert_eq!(
        (fx.read("a.txt").as_deref(), fx.read("x.txt").as_deref()),
        (Some("A2"), Some("X1"))
    );
    // Two more turns: the oldest recorded state (A0, no x.txt) must win.
    cp.rollback(2).unwrap();
    assert_eq!(fx.read("a.txt").as_deref(), Some("A0"));
    assert!(!fx.path("x.txt").exists());
}

#[test]
fn rollback_survives_restart() {
    let fx = Fixture::new();
    fx.write("a.txt", "A0");
    {
        let cp = fx.open();
        cp.begin_turn("before restart").unwrap();
        modify(&cp, &fx, "a.txt", "A1");
        modify(&cp, &fx, "n.txt", "N1");
    }
    let cp = fx.open();
    assert_eq!(cp.list().len(), 1);
    // New turns after reopen get fresh numbers and stack on top.
    assert_eq!(cp.begin_turn("after restart").unwrap(), 2);
    modify(&cp, &fx, "a.txt", "A2");
    cp.rollback(2).unwrap();
    assert_eq!(fx.read("a.txt").as_deref(), Some("A0"));
    assert!(!fx.path("n.txt").exists());
}

#[test]
fn restores_binary_content_and_permissions() {
    let fx = Fixture::new();
    let bin: Vec<u8> = (0..=255u8).collect();
    fs::write(fx.path("blob.bin"), &bin).unwrap();
    #[cfg(unix)]
    platform_fs::set_mode(&fx.path("blob.bin"), 0o755).unwrap();
    let cp = fx.open();
    cp.begin_turn("t").unwrap();
    cp.before_write(&fx.path("blob.bin")).unwrap();
    fs::write(fx.path("blob.bin"), b"x").unwrap();
    #[cfg(unix)]
    platform_fs::set_mode(&fx.path("blob.bin"), 0o600).unwrap();
    cp.rollback(1).unwrap();
    assert_eq!(fs::read(fx.path("blob.bin")).unwrap(), bin);
    #[cfg(unix)]
    assert_eq!(
        platform_fs::mode(&fs::metadata(fx.path("blob.bin")).unwrap()) & 0o777,
        0o755
    );
}

#[test]
fn refuses_paths_outside_the_project() {
    let fx = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "s").unwrap();
    let cp = fx.open();
    cp.begin_turn("t").unwrap();
    assert!(cp.before_write(&outside.path().join("secret")).is_err());
    assert!(cp.before_write(&outside.path().join("not-yet")).is_err());
    assert!(cp.before_write(&fx.root().join("../escape.txt")).is_err());
    assert!(cp.before_write(Path::new("relative.txt")).is_err());
    assert!(
        cp.before_write(&fx.root()).is_err(),
        "the root itself is not a file"
    );
    // A symlink inside the project that points out is judged by where it leads.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path().join("secret"), fx.path("link")).unwrap();
        assert!(cp.before_write(&fx.path("link")).is_err());
        std::os::unix::fs::symlink(outside.path(), fx.path("linkdir")).unwrap();
        assert!(cp.before_write(&fx.path("linkdir/new.txt")).is_err());
    }
    assert!(cp.list().is_empty());
}

#[test]
fn tampered_manifest_cannot_restore_outside_the_root() {
    let fx = Fixture::new();
    fx.write("a.txt", "A0");
    let cp = fx.open();
    cp.begin_turn("t").unwrap();
    modify(&cp, &fx, "a.txt", "A1");
    let manifest_path = turn_dir(&cp.dir, 1).join("manifest.json");
    let evil = tempfile::tempdir().unwrap();
    let target = evil.path().join("pwned");
    let text = fs::read_to_string(&manifest_path)
        .unwrap()
        .replace("a.txt", &format!("../../..{}", target.display()));
    fs::write(&manifest_path, text).unwrap();
    let report = cp.rollback(1).unwrap();
    assert!(report.restored.is_empty() && !report.warnings.is_empty());
    assert!(!target.exists());
    assert!(
        turn_dir(&cp.dir, 1).exists(),
        "a failed restore keeps the checkpoint for another try"
    );
}

#[test]
fn oversized_files_are_skipped_with_a_warning() {
    let fx = Fixture::new();
    let big = fx.path("big.bin");
    fs::File::create(&big)
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    fx.write("a.txt", "A0");
    let cp = fx.open();
    cp.begin_turn("t").unwrap();
    cp.before_write(&big).unwrap();
    cp.before_write(&big).unwrap();
    modify(&cp, &fx, "a.txt", "A1");
    let report = cp.rollback(1).unwrap();
    assert_eq!(report.restored, ["a.txt"]);
    assert!(report.warnings.iter().any(|w| w.contains("big.bin")));
}

#[test]
fn implicit_turn_and_nested_checkpoint_storage_stay_separate() {
    let fx = Fixture::new();
    fx.write("a.txt", "A0");
    let cp = fx.open();
    modify(&cp, &fx, "a.txt", "A1");
    assert_eq!(cp.list().len(), 1);
    let nested_dir = fx.root().join(".cp");
    let nested = Checkpoints::new(nested_dir.clone(), fx.root()).unwrap();
    assert!(
        nested
            .before_write(&nested_dir.join("manifest.json"))
            .is_err()
    );
    nested.before_write(&fx.path("a.txt")).unwrap();
    fs::write(fx.path("a.txt"), "A2").unwrap();
    assert_eq!(nested.rollback(1).unwrap().restored, ["a.txt"]);
    assert_eq!(fs::read_to_string(fx.path("a.txt")).unwrap(), "A1");
    assert!(cp.rollback(0).is_err());
}

/// Files under a directory attached with `/add-dir` are recorded and rolled back like project
/// files; created files are removed together with the directories the turn made.
#[test]
fn added_directories_are_recorded_and_rolled_back() {
    let fx = Fixture::new();
    let extra = tempfile::tempdir().unwrap();
    let extra_root = fs::canonicalize(extra.path()).unwrap();
    fs::write(extra_root.join("page.vue"), "old").unwrap();
    let cp = fx.open();
    assert!(
        cp.before_write(&extra_root.join("page.vue")).is_err(),
        "not attached yet"
    );
    cp.set_added_roots(std::slice::from_ref(&extra_root));
    cp.begin_turn("t").unwrap();
    cp.before_write(&extra_root.join("page.vue")).unwrap();
    fs::write(extra_root.join("page.vue"), "new").unwrap();
    let created = extra_root.join("public/install.sh");
    cp.before_write(&created).unwrap();
    fs::create_dir_all(created.parent().unwrap()).unwrap();
    fs::write(&created, "#!/bin/sh").unwrap();

    let shown = extra_root.join("page.vue").to_string_lossy().into_owned();
    assert!(
        cp.changed_files(1).contains(&shown),
        "{:?}",
        cp.changed_files(1)
    );
    let report = cp.rollback(1).unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.restored, [shown]);
    assert_eq!(
        fs::read_to_string(extra_root.join("page.vue")).unwrap(),
        "old"
    );
    assert!(!created.exists() && !extra_root.join("public").exists());
    assert!(
        extra_root.exists(),
        "the added root itself is never removed"
    );
}

/// Detaching a directory stops restores into it; a manifest naming a root that is not
/// attached cannot write there.
#[test]
fn detached_or_forged_added_roots_are_not_restored_into() {
    let fx = Fixture::new();
    let extra = tempfile::tempdir().unwrap();
    let extra_root = fs::canonicalize(extra.path()).unwrap();
    fs::write(extra_root.join("f.txt"), "old").unwrap();
    let cp = fx.open();
    cp.set_added_roots(std::slice::from_ref(&extra_root));
    cp.begin_turn("t").unwrap();
    cp.before_write(&extra_root.join("f.txt")).unwrap();
    fs::write(extra_root.join("f.txt"), "new").unwrap();
    cp.set_added_roots(&[]);
    let report = cp.rollback(1).unwrap();
    assert!(report.restored.is_empty());
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("no longer attached"))
    );
    assert_eq!(fs::read_to_string(extra_root.join("f.txt")).unwrap(), "new");

    let evil = tempfile::tempdir().unwrap();
    let evil_root = fs::canonicalize(evil.path()).unwrap();
    let manifest_path = turn_dir(&cp.dir, 1).join("manifest.json");
    let text = fs::read_to_string(&manifest_path).unwrap().replace(
        &serde_json::to_string(&extra_root).unwrap(),
        &serde_json::to_string(&evil_root).unwrap(),
    );
    fs::write(&manifest_path, text).unwrap();
    cp.set_added_roots(std::slice::from_ref(&extra_root));
    let report = cp.rollback(1).unwrap();
    assert!(report.restored.is_empty() && !report.warnings.is_empty());
    assert!(!evil_root.join("f.txt").exists());
}

/// A failed newer manifest must stop the rollback: older manifests stay untouched, so a
/// retry after reattaching cannot replay the newer saved content over the older restore.
#[test]
fn failed_newer_manifest_stops_older_rollbacks_across_retries() {
    let fx = Fixture::new();
    fx.write("a.txt", "A0");
    let extra = tempfile::tempdir().unwrap();
    let extra_root = fs::canonicalize(extra.path()).unwrap();
    fs::write(extra_root.join("b.txt"), "B0").unwrap();
    let cp = fx.open();
    cp.set_added_roots(std::slice::from_ref(&extra_root));
    cp.begin_turn("one").unwrap();
    modify(&cp, &fx, "a.txt", "A1");
    cp.begin_turn("two").unwrap();
    modify(&cp, &fx, "a.txt", "A2");
    cp.before_write(&extra_root.join("b.txt")).unwrap();
    fs::write(extra_root.join("b.txt"), "B1").unwrap();

    cp.set_added_roots(&[]);
    let report = cp.rollback(2).unwrap();
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("no longer attached"))
    );
    assert_eq!(fx.read("a.txt").as_deref(), Some("A1"));
    assert_eq!(
        cp.list().len(),
        2,
        "both checkpoints are retained for a retry"
    );

    cp.set_added_roots(std::slice::from_ref(&extra_root));
    let retry = cp.rollback(2).unwrap();
    assert!(retry.warnings.is_empty(), "{:?}", retry.warnings);
    assert_eq!(fx.read("a.txt").as_deref(), Some("A0"));
    assert_eq!(fs::read_to_string(extra_root.join("b.txt")).unwrap(), "B0");
    assert!(cp.list().is_empty());
}

/// A checkpoint whose directory cannot be discarded after a full restore is reported and
/// stops the rollback instead of being silently replayed later.
#[cfg(unix)]
#[test]
fn undeletable_checkpoint_directory_stops_the_rollback() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new();
    fx.write("a.txt", "A0");
    let cp = fx.open();
    cp.begin_turn("one").unwrap();
    modify(&cp, &fx, "a.txt", "A1");
    let files = turn_dir(&cp.dir, 1).join("files");
    fs::set_permissions(&files, fs::Permissions::from_mode(0o500)).unwrap();
    let report = cp.rollback(1);
    fs::set_permissions(&files, fs::Permissions::from_mode(0o700)).unwrap();
    let report = report.unwrap();
    assert_eq!(fx.read("a.txt").as_deref(), Some("A0"));
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("could not be discarded")),
        "{:?}",
        report.warnings
    );
    assert_eq!(cp.list().len(), 1, "the checkpoint is kept for a retry");
}
