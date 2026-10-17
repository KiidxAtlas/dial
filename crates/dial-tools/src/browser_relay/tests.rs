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
