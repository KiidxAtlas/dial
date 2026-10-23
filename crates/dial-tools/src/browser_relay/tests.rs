use super::*;

#[test]
fn the_token_is_created_once_then_reused() {
    let dir = tempfile::tempdir().unwrap();
    let first = load_or_create_token(dir.path()).unwrap();
    assert!(is_valid_token(&first), "{first}");
    assert_eq!(load_or_create_token(dir.path()).unwrap(), first);
    assert_eq!(
        std::fs::read_to_string(token_path(dir.path())).unwrap(),
        first
    );
}

#[test]
fn a_malformed_token_file_is_replaced_not_trusted() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(token_path(dir.path()).parent().unwrap()).unwrap();
    std::fs::write(token_path(dir.path()), "short\n").unwrap();
    let token = load_or_create_token(dir.path()).unwrap();
    assert!(is_valid_token(&token));
    assert_ne!(token, "short");
}

#[cfg(unix)]
#[test]
fn the_token_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    load_or_create_token(dir.path()).unwrap();
    let mode = std::fs::metadata(token_path(dir.path()))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn existing_token_and_directory_are_made_private_before_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let path = token_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let token = "ab".repeat(32);
    std::fs::write(&path, &token).unwrap();
    assert_eq!(load_or_create_token(dir.path()).unwrap(), token);
    for path in [path.as_path(), path.parent().unwrap()] {
        let file = if path.is_dir() {
            crate::platform::fs::open_dir_nofollow(path).unwrap()
        } else {
            std::fs::File::open(path).unwrap()
        };
        assert!(crate::platform::fs::is_owner_only(&file).unwrap());
    }
}

#[cfg(unix)]
#[test]
fn token_symlinks_and_hard_links_are_refused_without_touching_the_target() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("unrelated");
    let token = "cd".repeat(32);
    std::fs::write(&target, &token).unwrap();
    let path = token_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    symlink(&target, &path).unwrap();
    assert!(load_or_create_token(dir.path()).is_err());
    std::fs::remove_file(&path).unwrap();
    std::fs::hard_link(&target, &path).unwrap();
    assert!(load_or_create_token(dir.path()).is_err());
    assert_eq!(std::fs::read_to_string(target).unwrap(), token);
}

#[cfg(unix)]
#[test]
fn a_symlinked_token_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("browser-relay")).unwrap();
    assert!(load_or_create_token(dir.path()).is_err());
    assert!(!outside.path().join("token").exists());
}

#[test]
fn a_directory_in_place_of_the_token_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(token_path(dir.path())).unwrap();
    assert!(load_or_create_token(dir.path()).is_err());
    assert!(token_path(dir.path()).is_dir());
}
