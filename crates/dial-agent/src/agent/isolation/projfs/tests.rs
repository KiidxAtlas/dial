use super::*;
use std::path::Path;

fn limits(files: usize) -> TaskConfig {
    TaskConfig {
        isolation_source_files: files,
        ..TaskConfig::default()
    }
}

/// Stamps the named files under `dir`, as the source snapshot `before`.
fn source_snapshot(dir: &Path, names: &[&str]) -> Snapshot {
    names
        .iter()
        .map(|name| {
            (
                PathBuf::from(name),
                stamp(&dir.join(name)).unwrap().unwrap(),
            )
        })
        .collect()
}

fn paths(names: &[&str]) -> BTreeSet<PathBuf> {
    names.iter().map(PathBuf::from).collect()
}

#[test]
fn removing_a_path_hides_everything_below_it() {
    let journal = Journal::default();
    journal.remove(PathBuf::from("dir"));
    assert!(journal.is_removed(Path::new("dir/a/b.txt")));
    assert!(journal.is_removed(Path::new("dir")));
    assert!(!journal.is_removed(Path::new("directory.txt")));
    assert!(!journal.is_removed(Path::new("other.txt")));
}

#[test]
fn recreating_a_path_clears_its_tombstone_and_marks_it_touched() {
    let journal = Journal::default();
    journal.remove(PathBuf::from("a.txt"));
    journal.touch(PathBuf::from("a.txt"));
    assert!(!journal.is_removed(Path::new("a.txt")));
    assert_eq!(journal.changes().touched, paths(&["a.txt"]));
}

#[test]
fn removing_a_directory_forgets_touches_below_it() {
    let journal = Journal::default();
    journal.touch(PathBuf::from("dir/new.txt"));
    journal.remove(PathBuf::from("dir"));
    assert!(journal.changes().touched.is_empty());
}

#[test]
fn untouched_placeholders_keep_their_source_stamp_without_being_reread() {
    let source = tempfile::tempdir().unwrap();
    let instance = tempfile::tempdir().unwrap();
    fs::write(source.path().join("a.txt"), "source").unwrap();
    fs::write(instance.path().join("a.txt"), "other bytes entirely").unwrap();
    let before = source_snapshot(source.path(), &["a.txt"]);
    let after = change_set(instance.path(), &Changes::default(), &before, &limits(100)).unwrap();
    assert_eq!(
        after[Path::new("a.txt")].hash,
        before[Path::new("a.txt")].hash
    );
}

#[test]
fn touched_files_are_restamped_from_the_instance_directory() {
    let source = tempfile::tempdir().unwrap();
    let instance = tempfile::tempdir().unwrap();
    fs::write(source.path().join("a.txt"), "source").unwrap();
    fs::write(instance.path().join("a.txt"), "worker edit").unwrap();
    let before = source_snapshot(source.path(), &["a.txt"]);
    let changes = Changes {
        touched: paths(&["a.txt"]),
        ..Changes::default()
    };
    let after = change_set(instance.path(), &changes, &before, &limits(100)).unwrap();
    let expected = stamp(&instance.path().join("a.txt")).unwrap().unwrap();
    assert_eq!(after[Path::new("a.txt")].hash, expected.hash);
    assert_ne!(
        after[Path::new("a.txt")].hash,
        before[Path::new("a.txt")].hash
    );
}

#[test]
fn new_files_are_added_and_excluded_paths_never_are() {
    let instance = tempfile::tempdir().unwrap();
    fs::write(instance.path().join("new.txt"), "new").unwrap();
    fs::write(instance.path().join(".env"), "SECRET=1").unwrap();
    let after = change_set(
        instance.path(),
        &Changes::default(),
        &Snapshot::new(),
        &limits(100),
    )
    .unwrap();
    assert!(after.contains_key(Path::new("new.txt")));
    assert!(!after.contains_key(Path::new(".env")));
}

#[test]
fn removed_files_and_directories_leave_the_change_set_and_others_stay() {
    let source = tempfile::tempdir().unwrap();
    let instance = tempfile::tempdir().unwrap();
    fs::write(source.path().join("a.txt"), "a").unwrap();
    fs::write(source.path().join("keep.txt"), "k").unwrap();
    fs::create_dir(source.path().join("dir")).unwrap();
    fs::write(source.path().join("dir").join("b.txt"), "b").unwrap();
    let before = source_snapshot(source.path(), &["a.txt", "keep.txt", "dir/b.txt"]);
    let changes = Changes {
        removed: paths(&["a.txt", "dir"]),
        ..Changes::default()
    };
    let after = change_set(instance.path(), &changes, &before, &limits(100)).unwrap();
    assert_eq!(
        after.keys().cloned().collect::<BTreeSet<_>>(),
        paths(&["keep.txt"])
    );
}

#[test]
fn the_change_set_respects_the_entry_budget() {
    let instance = tempfile::tempdir().unwrap();
    fs::write(instance.path().join("one.txt"), "1").unwrap();
    fs::write(instance.path().join("two.txt"), "2").unwrap();
    let error = change_set(
        instance.path(),
        &Changes::default(),
        &Snapshot::new(),
        &limits(1),
    )
    .unwrap_err();
    assert!(error.to_string().contains("more than 1 changed entries"));
}
