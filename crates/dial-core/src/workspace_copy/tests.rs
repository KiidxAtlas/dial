use super::*;
#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(windows)]
use std::os::windows::fs::symlink_file as symlink;

#[test]
fn root_build_outputs_are_excluded_but_nested_sources_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert!(!is_excluded_source_path(root, &root.join("bin/script")));
    fs::write(root.join(".gitignore"), "/bin/\n").unwrap();
    for excluded in [
        "bin",
        "bin/dial",
        "bin/dial.bak-20261015-131809",
        ".cargo-work/windows/rustc/lib/librustc_driver.dylib",
        ".tmp/dial-debug-dump-1",
    ] {
        assert!(
            is_excluded_source_path(root, &root.join(excluded)),
            "{excluded}"
        );
    }
    for kept in [
        "src/bin/tool.rs",
        "src/main.rs",
        "docs/bin/readme.md",
        "binary.txt",
        ".cargo/config.toml",
    ] {
        assert!(
            !is_excluded_source_path(root, &root.join(kept)),
            "{kept} is source and must be copied"
        );
    }
}

#[test]
fn copies_omit_credentials_but_keep_templates() {
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join(".env"), "OPENAI_API_KEY=secret").unwrap();
    fs::write(
        source.path().join(".env.example"),
        "OPENAI_API_KEY=replace-me",
    )
    .unwrap();
    fs::write(source.path().join("private.pem"), "private key").unwrap();
    symlink(source.path().join(".env"), source.path().join("env_alias")).unwrap();
    fs::write(source.path().join("main.rs"), "fn main() {}").unwrap();
    let copied = clone_project(source.path()).unwrap();
    assert!(!copied.path().join(".env").exists());
    assert!(!copied.path().join("env_alias").exists());
    assert!(!copied.path().join("private.pem").exists());
    assert!(copied.path().join(".env.example").is_file());
    assert!(copied.path().join("main.rs").is_file());
}

#[test]
fn plain_copy_applies_the_same_exclusions_as_clone() {
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("main.rs"), "fn main() {}").unwrap();
    for excluded in SECRET_DIRS.iter().chain(DEPENDENCY_DIRS) {
        fs::create_dir(source.path().join(excluded)).unwrap();
        fs::write(source.path().join(excluded).join("x"), "x").unwrap();
    }
    fs::write(source.path().join(".env"), "secret").unwrap();
    let copied = copy_project_plain(source.path()).unwrap();
    assert_eq!(
        fs::read_to_string(copied.path().join("main.rs")).unwrap(),
        "fn main() {}"
    );
    assert!(!copied.path().join(".env").exists());
    assert!(
        SECRET_DIRS
            .iter()
            .chain(DEPENDENCY_DIRS)
            .all(|d| !copied.path().join(d).exists())
    );
}

#[test]
fn paths_outside_the_root_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    assert!(is_secret_source_path(
        root.path(),
        &outside.path().join("main.rs")
    ));
}

#[cfg(unix)]
#[test]
fn directory_aliases_never_leave_the_real_directory_empty() {
    // Directory read order is unspecified, so the real directory must survive whichever of
    // its names the copier reaches first.
    let source = tempfile::tempdir().unwrap();
    fs::create_dir(source.path().join("real")).unwrap();
    fs::write(source.path().join("real/a.txt"), "a").unwrap();
    for alias in 0..8 {
        symlink(
            source.path().join("real"),
            source.path().join(format!("alias-{alias}")),
        )
        .unwrap();
    }
    for copied in [
        clone_project(source.path()).unwrap(),
        copy_project_plain(source.path()).unwrap(),
    ] {
        assert_eq!(
            fs::read_to_string(copied.path().join("real/a.txt")).unwrap(),
            "a"
        );
        for alias in 0..8 {
            assert_eq!(
                fs::read_to_string(copied.path().join(format!("alias-{alias}/a.txt"))).unwrap(),
                "a"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn directory_cycles_stop_and_external_targets_are_refused() {
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("source.txt"), "source").unwrap();
    symlink(source.path(), source.path().join("cycle")).unwrap();
    for copied in [
        clone_project(source.path()).unwrap(),
        copy_project_plain(source.path()).unwrap(),
    ] {
        assert_eq!(
            fs::read_to_string(copied.path().join("source.txt")).unwrap(),
            "source"
        );
        assert_eq!(
            fs::read_dir(copied.path().join("cycle")).unwrap().count(),
            0
        );
    }
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("private.txt"), "untouched").unwrap();
    symlink(outside.path(), source.path().join("escape")).unwrap();
    assert!(clone_project(source.path()).is_err());
    assert!(copy_project_plain(source.path()).is_err());
    assert_eq!(
        fs::read_to_string(outside.path().join("private.txt")).unwrap(),
        "untouched"
    );
}

#[cfg(unix)]
#[test]
fn copies_preserve_file_modes_and_do_not_share_writes() {
    use std::os::unix::fs::PermissionsExt;

    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("script.sh");
    fs::write(&file, "original").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o751)).unwrap();
    for copied in [
        clone_project(source.path()).unwrap(),
        copy_project_plain(source.path()).unwrap(),
    ] {
        let copy = copied.path().join("script.sh");
        assert_eq!(
            fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
            0o751
        );
        assert_eq!(fs::read_to_string(&copy).unwrap(), "original");
        fs::write(copy, "changed").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "original");
    }
}

#[cfg(unix)]
#[test]
fn file_symlinks_cannot_escape_and_broken_links_fail_closed() {
    let source = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let private = outside.path().join("private.txt");
    fs::write(&private, "private").unwrap();
    let alias = source.path().join("alias");
    symlink(&private, &alias).unwrap();
    assert!(clone_project(source.path()).is_err());
    assert!(copy_project_plain(source.path()).is_err());
    fs::remove_file(&alias).unwrap();
    symlink("missing", &alias).unwrap();
    assert!(clone_project(source.path()).is_err());
    assert!(copy_project_plain(source.path()).is_err());
    assert_eq!(fs::read_to_string(private).unwrap(), "private");
}

#[cfg(unix)]
#[test]
fn aliases_to_excluded_directories_are_not_materialized() {
    let source = tempfile::tempdir().unwrap();
    for excluded in ["node_modules", ".ssh"] {
        fs::create_dir(source.path().join(excluded)).unwrap();
        fs::write(source.path().join(excluded).join("hidden"), "hidden").unwrap();
        symlink(excluded, source.path().join(format!("alias-{excluded}"))).unwrap();
    }
    for copied in [
        clone_project(source.path()).unwrap(),
        copy_project_plain(source.path()).unwrap(),
    ] {
        assert_eq!(fs::read_dir(copied.path()).unwrap().count(), 0);
    }
}
