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

fn scratch_dir(label: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "dial-windows-{label}-{}-{}",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&base).unwrap();
    base
}

#[test]
fn private_rewrite_accepts_token_default_owned_target_and_rejects_links() {
    // Elevated tokens create objects owned by BUILTIN\Administrators; the rewrite must still accept them.
    let base = scratch_dir("rewrite");
    let path = base.join("record");
    crate::platform::atomic::write_private(&path, b"old").unwrap();
    crate::platform::atomic::write_private(&path, b"new").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
    {
        let file = File::open(&path).unwrap();
        assert!(owned_by_current_user(&file).unwrap());
        assert!(is_owner_only(&file).unwrap());
    }
    // A second name for the object must still be refused by the private rewrite.
    std::fs::hard_link(&path, base.join("alias")).unwrap();
    assert!(crate::platform::atomic::write_private(&path, b"forbidden").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn owner_only_open_installs_acl_on_std_created_file_without_write_dac() {
    // std creates the file with the token's default owner and no WRITE_DAC on the returned handle.
    let base = scratch_dir("std-owned");
    let path = base.join("state");
    File::create(&path).unwrap();
    let file = open_owner_only(OpenOptions::new().read(true).write(true), &path).unwrap();
    assert!(owned_by_current_user(&file).unwrap());
    assert!(is_owner_only(&file).unwrap());
    drop(file);
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn append_only_log_handle_is_created_private_under_private_dir() {
    use std::io::{Read, Write};
    // Mirrors process log creation: a private directory, then an append-only create_new handle.
    let base = scratch_dir("append");
    set_owner_only(&base).unwrap();
    let path = base.join("process.log");
    let mut log = open_owner_only(OpenOptions::new().append(true).create_new(true), &path).unwrap();
    assert!(owned_by_current_user(&log).unwrap());
    assert!(is_owner_only(&log).unwrap());
    log.write_all(b"one\n").unwrap();
    let mut again = open_owner_only(OpenOptions::new().append(true), &path).unwrap();
    again.write_all(b"two\n").unwrap();
    drop((log, again));
    let mut text = String::new();
    File::open(&path)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "one\ntwo\n");
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_dir(&base).unwrap();
}
