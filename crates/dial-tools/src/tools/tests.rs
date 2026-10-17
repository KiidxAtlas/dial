use super::testkit::*;
use super::*;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;

#[test]
fn unknown_tool_and_bad_args_are_errors_not_panics() {
    let (_dir, ws) = workspace();
    assert!(run(&ws, "nope", json!({})).is_error);
    assert!(run(&ws, "read", json!("text")).is_error);
    let out = run(&ws, "read", json!({"path": 5}));
    assert!(out.is_error && out.text.contains("must be a string"));
    assert!(run(&ws, "read", json!({})).text.contains("missing"));
}

#[cfg(unix)]
#[test]
fn paths_cannot_escape_workspace() {
    let (dir, ws) = workspace();
    let ws = ws.with_edit_mode(EditMode::Replace);
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("canary"), "secret").unwrap();
    fs::write(dir.path().join("inside.txt"), "hello\n").unwrap();
    symlink(outside.path().join("canary"), dir.path().join("file_link")).unwrap();
    symlink(outside.path(), dir.path().join("dir_link")).unwrap();
    symlink(dir.path().join("nowhere"), dir.path().join("dangling")).unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    symlink(
        dir.path().join("inside.txt"),
        dir.path().join("sub/inner_link"),
    )
    .unwrap();

    let canary = outside.path().join("canary");
    let escapes = [
        "../x".to_string(),
        "sub/../../x".to_string(),
        canary.to_string_lossy().into_owned(),
        "file_link".to_string(),
        "dir_link/canary".to_string(),
        "dir_link/new.txt".to_string(),
        "dangling".to_string(),
    ];
    for path in &escapes {
        let read = run(&ws, "read", json!({"path": path}));
        assert!(
            read.is_error && !read.text.contains("secret"),
            "read {path}: {}",
            read.text
        );
        let write = run(&ws, "write", json!({"path": path, "content": "pwned"}));
        assert!(write.is_error, "write {path}");
        let edit = run(
            &ws,
            "edit",
            json!({"path": path, "old_string": "secret", "new_string": "x"}),
        );
        assert!(edit.is_error, "edit {path}");
    }
    assert_eq!(fs::read_to_string(&canary).unwrap(), "secret");
    assert!(!outside.path().join("new.txt").exists());
    assert!(!dir.path().join("nowhere").exists());

    // Symlinks that stay inside the workspace are fine.
    let ok = run(&ws, "read", json!({"path": "sub/inner_link"}));
    assert!(!ok.is_error && ok.text.contains("hello"), "{}", ok.text);
    // Absolute paths inside the workspace are fine too.
    let abs = dir.path().canonicalize().unwrap().join("inside.txt");
    assert!(!run(&ws, "read", json!({"path": abs})).is_error);
}

#[test]
fn home_workspace_sandbox_policies_keep_private_storage_excluded() {
    use crate::config::types::SandboxMode;
    let (dir, ws) = workspace();
    let user = dir.path().canonicalize().unwrap().join(".dial");
    let state = user.join("projects/home");
    fs::create_dir_all(&state).unwrap();
    for mode in [SandboxMode::Dev, SandboxMode::Strict] {
        let ws = apply_sandbox_config(
            ws.clone(),
            &SandboxConfig {
                mode,
                ..Default::default()
            },
            Some((&state, &user)),
        )
        .unwrap();
        assert!(ws.resolve(".dial/config.toml").is_err());
        assert!(ws.resolve("public.txt").is_ok());
        let denied = match ws.sandbox_policy() {
            sandbox::Policy::Dev { deny_read, .. } | sandbox::Policy::Strict { deny_read, .. } => {
                deny_read
            }
        };
        assert!(denied.contains(&user));
        assert!(denied.contains(&state));
    }
}

#[test]
fn protected_storage_is_excluded_even_without_sandbox_or_gitignore() {
    let (dir, ws) = workspace();
    let private = dir.path().join("storage/private");
    fs::create_dir_all(&private).unwrap();
    fs::write(private.join("secret.txt"), "private-canary").unwrap();
    fs::write(dir.path().join("public.txt"), "public-canary").unwrap();
    let ws = apply_sandbox_config(
        ws,
        &SandboxConfig {
            mode: crate::config::types::SandboxMode::Off,
            ..Default::default()
        },
        Some((&private, &private)),
    )
    .unwrap();
    for path in [
        "storage/private",
        "storage/private/secret.txt",
        "storage/private/new.txt",
        "storage/private/../escape.txt",
    ] {
        assert!(ws.resolve(path).is_err(), "{path}");
        assert!(run(&ws, "read", json!({"path":path})).is_error);
        assert!(run(&ws, "write", json!({"path":path,"content":"changed"})).is_error);
    }
    assert!(ws.observe_delete(&ws.root().join("storage")).is_err());
    assert!(ws.observe_write(&ws.root().join("storage")).is_err());
    for (tool, args) in [
        ("read", json!({"path":"."})),
        ("glob", json!({"pattern":"**/*","gitignore":false})),
        ("grep", json!({"pattern":"canary","gitignore":false})),
    ] {
        let out = run(&ws, tool, args);
        assert!(!out.is_error, "{tool}: {}", out.text);
        assert!(
            !out.text.contains("secret.txt") && !out.text.contains("private-canary"),
            "{tool}: {}",
            out.text
        );
        assert!(out.text.contains("public.txt"), "{tool}: {}", out.text);
    }
    assert_eq!(
        fs::read_to_string(private.join("secret.txt")).unwrap(),
        "private-canary"
    );
}

#[cfg(unix)]
#[test]
fn protected_storage_rejects_symlink_aliases_and_missing_tails() {
    let (dir, ws) = workspace();
    let private = dir.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::write(private.join("secret.txt"), "private-canary").unwrap();
    symlink(&private, dir.path().join("alias")).unwrap();
    symlink(private.join("secret.txt"), dir.path().join("file-alias")).unwrap();
    let ws = ws.with_protected_paths(&[dir.path().join("alias"), dir.path().join("future/state")]);
    for path in [
        "private/secret.txt",
        "alias/secret.txt",
        "alias/new.txt",
        "file-alias",
        "future/state/new.txt",
    ] {
        assert!(run(&ws, "read", json!({"path":path})).is_error, "{path}");
        assert!(
            run(&ws, "write", json!({"path":path,"content":"changed"})).is_error,
            "{path}"
        );
    }
    assert!(ws.resolve("future/public.txt").is_ok());
    assert_eq!(
        fs::read_to_string(private.join("secret.txt")).unwrap(),
        "private-canary"
    );
}
