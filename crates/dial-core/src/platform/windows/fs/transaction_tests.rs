use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

#[test]
fn relative_mutations_are_exclusive_and_confined() {
    let base = std::env::temp_dir().join(format!(
        "dial-windows-fs-{}-{}",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&base).unwrap();
    let root = open_dir_nofollow_write(&base).unwrap();
    assert_eq!(
        create_new_beneath(&root, Path::new("one"), true)
            .unwrap()
            .metadata()
            .unwrap()
            .len(),
        0
    );
    let mut failures = Vec::new();
    let mut expect = |label: &str, ok: bool, result: io::Result<()>| {
        if result.is_ok() != ok {
            failures.push(format!("{label}: expected ok={ok}, got {result:?}"));
        }
    };
    expect(
        "create existing",
        false,
        create_new_beneath(&root, Path::new("one"), false).map(drop),
    );
    expect(
        "create stream name",
        false,
        create_new_beneath(&root, Path::new("name:stream"), false).map(drop),
    );
    expect(
        "link",
        true,
        link_beneath(&root, Path::new("one"), Path::new("two")),
    );
    // Same-file hard links are a no-op rename on newer Windows; collide with a distinct file instead.
    expect(
        "create other",
        true,
        create_new_beneath(&root, Path::new("other"), true).map(drop),
    );
    expect(
        "rename onto existing",
        false,
        rename_beneath(&root, Path::new("one"), Path::new("other")),
    );
    expect(
        "remove other",
        true,
        remove_beneath(&root, Path::new("other")),
    );
    expect(
        "rename",
        true,
        rename_beneath(&root, Path::new("one"), Path::new("three")),
    );
    expect(
        "remove three",
        true,
        remove_beneath(&root, Path::new("three")),
    );
    expect("remove two", true, remove_beneath(&root, Path::new("two")));
    drop(root);
    let _ = std::fs::remove_dir_all(&base);
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn private_beneath_creation_installs_acl_on_created_handle() {
    let base = std::env::temp_dir().join(format!(
        "dial-windows-private-fs-{}-{}",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&base).unwrap();
    let root = open_dir_nofollow_write(&base).unwrap();
    let file = create_new_beneath(&root, Path::new("private"), true).unwrap();
    assert!(is_owner_only(&file).unwrap());
    drop(file);
    drop(root);
    std::fs::remove_file(base.join("private")).unwrap();
    std::fs::remove_dir(base).unwrap();
}
