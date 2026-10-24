use super::*;

#[test]
fn no_replace_moves_files_and_directories_without_clobbering() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    std::fs::write(&source, "source").unwrap();
    std::fs::write(&target, "external").unwrap();
    assert!(rename_noreplace(&source, &target).is_err());
    assert_eq!(std::fs::read_to_string(&source).unwrap(), "source");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "external");
    std::fs::remove_file(&target).unwrap();
    rename_noreplace(&source, &target).unwrap();
    assert!(!source.exists());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "source");
    std::fs::remove_file(&target).unwrap();
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("child"), "contents").unwrap();
    std::fs::create_dir(&target).unwrap();
    // Even an empty directory must not be replaced (ordinary Unix rename would replace it).
    assert!(rename_noreplace(&source, &target).is_err());
    assert!(source.join("child").exists());
    std::fs::remove_dir(&target).unwrap();
    rename_noreplace(&source, &target).unwrap();
    assert_eq!(
        std::fs::read_to_string(target.join("child")).unwrap(),
        "contents"
    );
}

#[cfg(unix)]
#[test]
fn no_replace_refuses_dangling_symlink_destination() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    std::fs::write(&source, "source").unwrap();
    std::os::unix::fs::symlink("missing", &target).unwrap();
    assert!(rename_noreplace(&source, &target).is_err());
    assert!(source.exists());
    assert_eq!(std::fs::read_link(&target).unwrap(), Path::new("missing"));
}
