use super::*;

#[test]
fn windows_private_files_keep_owner_only_acl_and_handle_relative_replacement() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let directory = PrivateDirectory::open(&layout, "test").unwrap();

    directory.write("state", b"before").unwrap();
    directory.write("state", b"after").unwrap();
    assert_eq!(directory.read_head("state", 32).unwrap().0, b"after");
    assert!(platform_fs::is_owner_only(&directory.handle).unwrap());
    assert!(
        platform_fs::is_owner_only(&File::open(directory.path.join("state")).unwrap()).unwrap()
    );

    let nested = directory.child("nested").unwrap();
    nested.write("item", b"private").unwrap();
    directory.remove_tree("nested").unwrap();
    assert!(!directory.path.join("nested").exists());
}
