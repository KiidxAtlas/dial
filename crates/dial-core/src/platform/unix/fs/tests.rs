use super::*;
use std::io::Write;
use std::os::unix::fs::symlink;

#[test]
fn relative_mutations_remain_pinned_and_do_not_follow_destination_links() {
    let parent = tempfile::tempdir().unwrap();
    let original = parent.path().join("root");
    let moved = parent.path().join("moved");
    std::fs::create_dir(&original).unwrap();
    let root = open_dir_nofollow(&original).unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("keep"), b"outside").unwrap();
    std::fs::rename(&original, &moved).unwrap();
    symlink(outside.path(), &original).unwrap();

    let mut file = create_new_beneath(&root, Path::new("new"), true).unwrap();
    file.write_all(b"private").unwrap();
    assert_eq!(
        file.metadata().unwrap().mode() & 0o077,
        0,
        "owner-only creation"
    );
    assert_eq!(
        create_new_beneath(&root, Path::new("new"), false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists,
        "exclusive creation cannot reopen an existing file"
    );
    link_beneath(&root, Path::new("new"), Path::new("linked")).unwrap();
    assert_eq!(
        file.metadata().unwrap().nlink(),
        2,
        "hard link shares the inode"
    );
    assert!(link_beneath(&root, Path::new("new"), Path::new("linked")).is_err());
    symlink(outside.path().join("keep"), moved.join("target")).unwrap();
    assert!(create_new_beneath(&root, Path::new("target"), true).is_err());
    replace_beneath(&root, Path::new("new"), Path::new("target")).unwrap();
    assert_eq!(std::fs::read(moved.join("target")).unwrap(), b"private");
    assert_eq!(
        std::fs::read(outside.path().join("keep")).unwrap(),
        b"outside"
    );
    remove_beneath(&root, Path::new("linked")).unwrap();
    symlink(outside.path().join("keep"), moved.join("symlink")).unwrap();
    remove_beneath(&root, Path::new("symlink")).unwrap();
    assert!(
        outside.path().join("keep").exists(),
        "unlink removes the link, not its target"
    );
    create_dir_beneath(&root, Path::new("child")).unwrap();
    remove_dir_beneath(&root, Path::new("child")).unwrap();
    assert!(!moved.join("child").exists());
    assert!(
        !outside.path().join("new").exists(),
        "parent path replacement cannot redirect creation"
    );
}

#[test]
fn relative_mutations_reject_escaping_or_invalid_names() {
    let parent = tempfile::tempdir().unwrap();
    let root = open_dir_nofollow(parent.path()).unwrap();
    create_new_beneath(&root, Path::new("valid"), true).unwrap();
    for name in [
        "",
        ".",
        "..",
        "../escape",
        "/absolute",
        "two/names",
        "nul\0name",
    ] {
        let name = Path::new(name);
        assert!(
            create_new_beneath(&root, name, true).is_err(),
            "create {name:?}"
        );
        assert!(create_dir_beneath(&root, name).is_err(), "mkdir {name:?}");
        assert!(
            open_dir_beneath_handle(&root, name).is_err(),
            "open {name:?}"
        );
        for (from, to) in [(name, Path::new("valid")), (Path::new("valid"), name)] {
            assert!(
                link_beneath(&root, from, to).is_err(),
                "link {from:?} -> {to:?}"
            );
            assert!(
                replace_beneath(&root, from, to).is_err(),
                "rename {from:?} -> {to:?}"
            );
        }
        assert!(remove_beneath(&root, name).is_err(), "unlink {name:?}");
        assert!(remove_dir_beneath(&root, name).is_err(), "rmdir {name:?}");
    }
}

#[test]
fn exchange_swaps_two_entries_without_following_links_or_freeing_a_name() {
    let parent = tempfile::tempdir().unwrap();
    let root = open_dir_nofollow(parent.path()).unwrap();
    std::fs::write(parent.path().join("a"), b"first").unwrap();
    std::fs::write(parent.path().join("b"), b"second").unwrap();
    match exchange_beneath(&root, Path::new("a"), Path::new("b")) {
        Ok(()) => {
            assert_eq!(std::fs::read(parent.path().join("a")).unwrap(), b"second");
            assert_eq!(std::fs::read(parent.path().join("b")).unwrap(), b"first");
        }
        // A filesystem without an exchange primitive must leave both names unchanged.
        Err(error) => {
            eprintln!("exchange unsupported here: {error}");
            assert_eq!(std::fs::read(parent.path().join("a")).unwrap(), b"first");
            assert_eq!(std::fs::read(parent.path().join("b")).unwrap(), b"second");
        }
    }
    assert!(exchange_beneath(&root, Path::new("a"), Path::new("missing")).is_err());
    assert!(parent.path().join("a").exists());
    for name in ["", "..", "two/names", "nul\0name"] {
        assert!(exchange_beneath(&root, Path::new(name), Path::new("a")).is_err());
    }
}
