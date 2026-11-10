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

#[test]
fn sharing_violations_are_retried_within_a_bound() {
    let mut attempts = 0;
    let mut slept = Vec::new();
    let result = retry_sharing_with(
        || {
            attempts += 1;
            if attempts < 3 {
                Err(io::Error::from_raw_os_error(ERROR_SHARING_VIOLATION))
            } else {
                Ok(attempts)
            }
        },
        |delay| slept.push(delay.as_millis()),
    );
    assert_eq!(result.unwrap(), 3);
    assert_eq!(slept, [10, 20]);

    let mut attempts = 0;
    let result: io::Result<()> = retry_sharing_with(
        || {
            attempts += 1;
            Err(io::Error::from_raw_os_error(ERROR_SHARING_VIOLATION))
        },
        |_| {},
    );
    assert_eq!(
        result.unwrap_err().raw_os_error(),
        Some(ERROR_SHARING_VIOLATION)
    );
    assert_eq!(attempts, SHARING_RETRY_DELAYS_MS.len() + 1);

    let mut attempts = 0;
    let result: io::Result<()> = retry_sharing_with(
        || {
            attempts += 1;
            Err(io::Error::from_raw_os_error(5))
        },
        |_| panic!("only sharing violations are retried"),
    );
    assert_eq!(result.unwrap_err().raw_os_error(), Some(5));
    assert_eq!(attempts, 1);
}

#[test]
fn replace_and_remove_succeed_while_another_handle_has_the_target_open() {
    use std::io::Read;
    let base = scratch_dir("posix");
    std::fs::write(base.join("target"), b"old").unwrap();
    std::fs::write(base.join("staged"), b"new").unwrap();
    // std opens with read/write/delete sharing, like a concurrent reader of a published record.
    let mut reader = File::open(base.join("target")).unwrap();
    let root = open_dir_nofollow_write(&base).unwrap();
    replace_beneath(&root, Path::new("staged"), Path::new("target")).unwrap();
    assert_eq!(std::fs::read(base.join("target")).unwrap(), b"new");
    let mut text = Vec::new();
    reader.read_to_end(&mut text).unwrap();
    assert_eq!(text, b"old", "the open handle keeps the replaced object");

    let held = File::open(base.join("target")).unwrap();
    remove_beneath(&root, Path::new("target")).unwrap();
    // POSIX deletion frees the name at once, so it can be created again while `held` is open.
    assert!(!base.join("target").exists());
    create_new_beneath(&root, Path::new("target"), false).unwrap();
    drop((held, reader, root));
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn writable_directory_opens_while_another_handle_denies_delete_sharing() {
    // A process whose current directory this is holds it open with read/write sharing only.
    let base = scratch_dir("cwd-held");
    let held = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(&base)
        .unwrap();
    let root = open_dir_nofollow_write(&base).unwrap();
    create_new_beneath(&root, Path::new("file"), false).unwrap();
    remove_beneath(&root, Path::new("file")).unwrap();
    drop((root, held));
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn short_names_support_link_rename_and_replace() {
    let base = scratch_dir("short-names");
    let root = open_dir_nofollow_write(&base).unwrap();
    for name in ["0", "9", "a", "é", "鸡", "10", "99", "100", "🐔"] {
        let target = Path::new(name);
        std::fs::write(base.join("source"), b"linked").unwrap();
        link_beneath(&root, Path::new("source"), target)
            .unwrap_or_else(|error| panic!("link to {name:?}: {error}"));
        assert_eq!(std::fs::read(base.join(target)).unwrap(), b"linked");
        remove_beneath(&root, target).unwrap();
        rename_beneath(&root, Path::new("source"), target)
            .unwrap_or_else(|error| panic!("rename to {name:?}: {error}"));
        assert!(!base.join("source").exists());
        assert_eq!(std::fs::read(base.join(target)).unwrap(), b"linked");
        std::fs::write(base.join("staged"), b"replaced").unwrap();
        replace_beneath(&root, Path::new("staged"), target)
            .unwrap_or_else(|error| panic!("replace {name:?}: {error}"));
        assert_eq!(std::fs::read(base.join(target)).unwrap(), b"replaced");
        assert!(!base.join("staged").exists());
        remove_beneath(&root, target).unwrap();
    }
    drop(root);
    assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
    std::fs::remove_dir_all(&base).unwrap();
}
