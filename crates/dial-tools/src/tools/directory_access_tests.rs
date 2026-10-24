use super::*;

#[test]
fn sensitive_grant_is_exact_native_only_and_revoked_on_remove() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let storage = tmp.path().join("private");
    let allowed = storage.join("sessions");
    let sibling = storage.join("credentials");
    for path in [&root, &allowed, &sibling] {
        std::fs::create_dir_all(path).unwrap();
    }
    let allowed = allowed.canonicalize().unwrap();
    let storage = storage.canonicalize().unwrap();
    let mut ws = Workspace::new(&root)
        .unwrap()
        .with_protected_paths(&[storage.clone()]);
    ws.add_root(&allowed).unwrap();
    assert!(ws.resolve(allowed.join("new").to_str().unwrap()).is_err());
    let target = allowed.join("note.txt");
    let blocked = testkit::run(
        &ws,
        "write",
        json!({"path": target, "content":"native access\n"}),
    );
    assert!(blocked.is_error);
    ws.grant_sensitive_directory(&allowed).unwrap();
    assert_eq!(
        ws.resolve(allowed.join("new").to_str().unwrap()).unwrap(),
        allowed.join("new")
    );
    assert!(ws.check_protected_mutation(&allowed.join("new")).is_ok());
    let written = testkit::run(
        &ws,
        "write",
        json!({"path": target, "content":"native access\n"}),
    );
    assert!(!written.is_error, "{}", written.text);
    let read = testkit::run(&ws, "read", json!({"path": target}));
    assert!(!read.is_error, "{}", read.text);
    assert!(read.text.contains("native access"));
    assert!(ws.check_protected_mutation(&storage).is_err());
    assert!(ws.resolve(sibling.to_str().unwrap()).is_err());
    let exec::sandbox::Policy::Strict {
        deny_read,
        extra_write,
        ..
    } = ws.sandbox_policy()
    else {
        panic!()
    };
    assert!(
        deny_read.contains(&storage),
        "native grants must not drop shell denies"
    );
    assert!(
        !extra_write.contains(&allowed),
        "native-only grants must not conflict with shell denies"
    );
    ws.remove_root(&allowed).unwrap();
    assert!(ws.resolve(allowed.join("new").to_str().unwrap()).is_err());
}

#[test]
fn sensitive_request_cannot_unlock_home_or_credentials() {
    let tmp = tempfile::tempdir().unwrap();
    let protected = tmp.path().join("dial");
    let credentials = protected.join("auth");
    std::fs::create_dir_all(&credentials).unwrap();
    let mut ws = Workspace::new(tmp.path())
        .unwrap()
        .with_protected_paths(&[protected.clone()]);
    ws.credential_paths = vec![credentials.canonicalize().unwrap()];
    assert!(ws.directory_request(tmp.path(), true).is_err());
    assert!(ws.directory_request(&protected, true).is_err());
    assert!(ws.directory_request(&credentials, true).is_err());
    assert!(ws.directory_request(&protected, false).is_ok());
}

#[test]
fn sensitive_nested_root_accepts_relative_paths_but_not_sibling_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let private = tmp.path().join("private");
    let allowed = private.join("sessions");
    let other = private.join("other");
    std::fs::create_dir_all(&allowed).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let mut ws = Workspace::new(tmp.path())
        .unwrap()
        .with_protected_paths(&[private]);
    ws.grant_sensitive_directory(&allowed.canonicalize().unwrap())
        .unwrap();
    assert!(ws.resolve("private/sessions/new").is_ok());
    assert!(ws.resolve("private/other/new").is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&other, allowed.join("escape")).unwrap();
        assert!(
            ws.resolve(allowed.join("escape/new").to_str().unwrap())
                .is_err()
        );
    }
    let rebased = ws.rebased(tmp.path()).unwrap();
    assert!(rebased.resolve("private/sessions/new").is_err());
    assert!(
        ws.reroot(tmp.path())
            .unwrap()
            .resolve("private/sessions/new")
            .is_err()
    );
}

#[test]
fn configured_storage_grants_sessions_without_granting_credential_files() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let user = tmp.path().join("dial");
    let state = user.join("projects/example");
    let sessions = state.join("sessions");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    let mut ws = apply_sandbox_config(
        Workspace::new(&project).unwrap(),
        &crate::config::types::SandboxConfig::default(),
        Some((&state, &user)),
    )
    .unwrap();
    let selected = ws.directory_request(&sessions, true).unwrap();
    ws.grant_sensitive_directory(&selected).unwrap();
    assert!(
        ws.resolve(selected.join("journal.jsonl").to_str().unwrap())
            .is_ok()
    );
    assert!(ws.directory_request(&user, true).is_err());
    for credential in &ws.credential_paths {
        assert!(!ws.sensitive_access(credential));
        assert!(ws.resolve(credential.to_str().unwrap()).is_err());
    }
}
