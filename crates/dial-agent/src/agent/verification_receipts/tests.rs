use super::*;

fn outcome(status: CheckStatus, ran: bool) -> Outcome {
    Outcome {
        name: "syntax".into(),
        command: "python -m py_compile a.py".into(),
        status,
        ran,
    }
}

#[test]
fn identity_binds_content_and_absence_and_ignores_order() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
    let forward = source_identity(dir.path(), &["a.py".into(), "b.py".into()]);
    let reversed = source_identity(dir.path(), &["b.py".into(), "a.py".into()]);
    assert_eq!(forward, reversed);
    assert!(forward.complete);
    assert!(!forward.digest.contains("x = 1"));

    std::fs::write(dir.path().join("b.py"), "y = 2\n").unwrap();
    let with_b = source_identity(dir.path(), &["a.py".into(), "b.py".into()]);
    assert_ne!(
        forward.digest, with_b.digest,
        "created file must change identity"
    );

    std::fs::write(dir.path().join("a.py"), "x = 3\n").unwrap();
    let edited = source_identity(dir.path(), &["a.py".into(), "b.py".into()]);
    assert_ne!(
        with_b.digest, edited.digest,
        "content edit must change identity"
    );
}

#[test]
fn escaping_paths_mark_identity_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let identity = source_identity(dir.path(), &["../outside.py".into()]);
    assert!(!identity.complete);
}
#[cfg(unix)]
#[test]
fn symlinked_parent_paths_are_never_hashed_as_workspace_files() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "private contents").unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).unwrap();
    let identity = source_identity(dir.path(), &["linked/secret.txt".into()]);
    assert!(!identity.complete);
    assert!(!identity.digest.contains("private contents"));
}

#[test]
fn receipt_labels_status_and_never_carries_output() {
    let identity = source_identity(tempfile::tempdir().unwrap().path(), &[]);
    let passed = receipt(&identity, &identity, &[outcome(CheckStatus::Passed, true)]);
    assert_eq!(passed["overall"], "passed");
    assert_eq!(passed["evidence"], "execution_status_only");
    assert!(passed.to_string().contains("Not proof of correctness"));
    assert!(passed.get("output").is_none() && passed["checks"][0].get("output").is_none());

    let cancelled = receipt(
        &identity,
        &identity,
        &[
            outcome(CheckStatus::Passed, true),
            outcome(CheckStatus::Cancelled, false),
        ],
    );
    assert_eq!(cancelled["overall"], "cancelled");
    assert_eq!(cancelled["checks"][1]["ran"], false);

    let changed = source_identity(tempfile::tempdir().unwrap().path(), &["gone.py".into()]);
    let drifted = receipt(&identity, &changed, &[outcome(CheckStatus::Passed, true)]);
    assert_eq!(drifted["overall"], "source_changed");
    assert_eq!(drifted["source"]["stable"], false);
}

#[test]
fn history_is_bounded_to_newest_receipts() {
    let mut state = json!({"verification":"kept"});
    for seq in 0..(MAX_RECEIPTS as u64 + 5) {
        append(&mut state, json!({"seq": seq}));
    }
    let history = state["verification_receipts"].as_array().unwrap();
    assert_eq!(history.len(), MAX_RECEIPTS);
    assert_eq!(history[0]["seq"], 5);
    assert_eq!(state["verification"], "kept");
}
