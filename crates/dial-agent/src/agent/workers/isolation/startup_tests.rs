use super::*;
use std::cell::Cell;

thread_local! {
    static HASHED: Cell<(usize, u64)> = const { Cell::new((0, 0)) };
}

pub(super) fn record_hash(bytes: u64) {
    HASHED.with(|count| {
        let (files, total) = count.get();
        count.set((files + 1, total + bytes));
    });
}

fn take_hashes() -> (usize, u64) {
    HASHED.with(|count| count.replace((0, 0)))
}

fn config() -> crate::config::task::TaskConfig {
    crate::config::task::TaskConfig {
        isolation_git: false,
        isolation_dependencies: false,
        ..Default::default()
    }
}

#[test]
fn copy_startup_hashes_content_once_instead_of_twice() {
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("a"), vec![b'a'; 128 * 1024]).unwrap();
    fs::write(source.path().join("b"), vec![b'b'; 64 * 1024]).unwrap();
    let config = config();
    take_hashes();
    // The old pipeline hashed the source, then rehashed different copied inodes.
    let source_snapshot = snapshot(source.path(), None, &config).unwrap();
    let copied = crate::workspace_copy::copy_project_plain(source.path()).unwrap();
    snapshot(copied.path(), Some(&source_snapshot), &config).unwrap();
    assert_eq!(take_hashes(), (4, 384 * 1024));

    let project = Workspace::new(source.path()).unwrap();
    let isolated = Isolated::with_config(&project, &config, Preference::Copy).unwrap();
    assert_eq!(take_hashes(), (2, 192 * 1024));
    assert!(changed(&isolated.before, &isolated.current().unwrap()).is_empty());
    assert_eq!(take_hashes(), (0, 0), "unchanged files still reuse hashes");
}

#[test]
fn preflight_bounds_do_not_read_content() {
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("a"), "1234").unwrap();
    fs::write(source.path().join("b"), "5678").unwrap();
    for config in [
        crate::config::task::TaskConfig {
            isolation_source_files: 1,
            ..config()
        },
        crate::config::task::TaskConfig {
            isolation_source_bytes: 7,
            ..config()
        },
    ] {
        take_hashes();
        assert!(preflight(source.path(), &config).is_err());
        assert_eq!(take_hashes(), (0, 0));
    }
    fs::File::create(source.path().join("huge"))
        .unwrap()
        .set_len(MAX_BYTES as u64 + 1)
        .unwrap();
    assert!(preflight(source.path(), &config()).is_err());
    assert_eq!(take_hashes(), (0, 0));
}

#[test]
fn baseline_hashes_content_changed_after_preflight_and_rechecks_bounds() {
    let source = tempfile::tempdir().unwrap();
    let path = source.path().join("a");
    fs::write(&path, "old").unwrap();
    let config = crate::config::task::TaskConfig {
        isolation_source_bytes: 4,
        ..config()
    };
    let paths = preflight(source.path(), &config).unwrap();
    // A same-size replacement after preflight must become the actual worker baseline,
    // not inherit an obsolete source hash. This also models a mutable overlay lower layer.
    fs::remove_file(&path).unwrap();
    fs::write(&path, "new").unwrap();
    let copied = crate::workspace_copy::copy_project_plain(source.path()).unwrap();
    let before = snapshot_preserving(copied.path(), None, Some(&paths), &config).unwrap();
    assert_eq!(
        before[Path::new("a")].hash,
        <[u8; 32]>::from(Sha256::digest(b"new"))
    );
    fs::write(&path, "user").unwrap();
    assert!(!same(
        stamp(&path).unwrap().as_ref(),
        before.get(Path::new("a"))
    ));

    fs::write(copied.path().join("a"), "grown").unwrap();
    let error = snapshot_preserving(copied.path(), None, Some(&paths), &config).unwrap_err();
    assert!(format!("{error:#}").contains("limited to 4 bytes"));
    fs::write(copied.path().join("a"), "new").unwrap();
    fs::write(copied.path().join("added"), "xx").unwrap();
    assert!(snapshot_preserving(copied.path(), None, Some(&paths), &config).is_err());
}
