use super::*;

fn run(sequence: u64, code: i32, kind: CheckKind) -> CheckRun {
    CheckRun {
        command: "cargo test regression".into(),
        cwd: "/workspace".into(),
        objective: "reject duplicate keys".into(),
        kind,
        tool_call_id: format!("check-{sequence}"),
        evidence_ref: format!("artifact://check-{sequence}"),
        exit_code: Some(code),
        sequence,
    }
}
fn change(before: char, after: char) -> FileChange {
    FileChange {
        path: "src/lib.rs".into(),
        before_sha256: Some(before.to_string().repeat(64)),
        after_sha256: Some(after.to_string().repeat(64)),
        before_ref: Some("/state/before".into()),
        after_ref: Some("/state/after".into()),
    }
}

#[test]
fn records_reproducible_failure_change_pass_and_exact_applicability() {
    let mut tracker = Tracker::default();
    assert!(
        tracker
            .observe(run(1, 1, CheckKind::Behavioral), vec![])
            .is_none()
    );
    let record = tracker
        .observe(run(2, 0, CheckKind::Behavioral), vec![change('a', 'b')])
        .unwrap();
    assert!(record.valid());
    assert!(record.applies_to(
        "/workspace",
        &BTreeMap::from([("src/lib.rs".into(), Some("a".repeat(64)))])
    ));
    assert!(!record.applies_to(
        "/other",
        &BTreeMap::from([("src/lib.rs".into(), Some("a".repeat(64)))])
    ));
    assert!(!record.applies_to(
        "/workspace",
        &BTreeMap::from([("src/lib.rs".into(), Some("b".repeat(64)))])
    ));
    assert!(
        tracker
            .observe(run(3, 0, CheckKind::Behavioral), vec![change('a', 'b')])
            .is_none()
    );
}

#[test]
fn applicability_checks_current_bytes_and_refuses_exhausted_budget() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "broken").unwrap();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let mut tracker = Tracker::default();
    tracker.observe(run(1, 1, CheckKind::Behavioral), vec![]);
    let mut record = tracker
        .observe(run(2, 0, CheckKind::Behavioral), vec![change('a', 'b')])
        .unwrap();
    record.project_root = workspace.root().to_string_lossy().into_owned();
    record.before.cwd = record.project_root.clone();
    record.after.cwd = record.project_root.clone();
    record.changes[0].before_sha256 = Some(format!("{:x}", Sha256::digest(b"broken")));
    assert!(record.matches_current(&workspace, &mut 100));
    assert!(!record.matches_current(&workspace, &mut 2));
    std::fs::write(dir.path().join("src/lib.rs"), "different").unwrap();
    assert!(!record.matches_current(&workspace, &mut 100));
}

#[test]
fn syntax_proof_stays_syntax_and_changed_objective_cannot_pair() {
    let mut tracker = Tracker::default();
    tracker.observe(run(1, 1, CheckKind::SyntaxOnly), vec![]);
    assert!(
        tracker
            .observe(run(2, 0, CheckKind::Behavioral), vec![change('a', 'b')])
            .is_none()
    );
    let record = tracker
        .observe(run(3, 0, CheckKind::SyntaxOnly), vec![change('a', 'b')])
        .unwrap();
    assert_eq!(record.kind, CheckKind::SyntaxOnly);
    tracker.observe(run(4, 1, CheckKind::Behavioral), vec![]);
    let mut other = run(5, 0, CheckKind::Behavioral);
    other.command = "cargo test unrelated".into();
    assert!(tracker.observe(other, vec![change('a', 'b')]).is_none());
}

#[test]
fn rejects_flaky_pass_timeout_and_missing_evidence() {
    let mut tracker = Tracker::default();
    tracker.observe(run(1, 1, CheckKind::Behavioral), vec![change('a', 'b')]);
    assert!(
        tracker
            .observe(run(2, 0, CheckKind::Behavioral), vec![change('a', 'b')])
            .is_none()
    );
    tracker.observe(run(3, 124, CheckKind::Behavioral), vec![]);
    assert!(
        tracker
            .observe(run(4, 0, CheckKind::Behavioral), vec![change('a', 'b')])
            .is_none()
    );
    tracker.observe(run(5, 1, CheckKind::Behavioral), vec![change('a', 'b')]);
    let mut pass = run(6, 0, CheckKind::Behavioral);
    pass.evidence_ref.clear();
    assert!(tracker.observe(pass, vec![change('a', 'c')]).is_none());
}

#[test]
fn unknown_run_invalidates_an_earlier_failure_baseline() {
    let mut tracker = Tracker::default();
    tracker.observe(run(1, 1, CheckKind::Behavioral), vec![change('a', 'b')]);
    let mut unknown = run(2, 0, CheckKind::Behavioral);
    unknown.exit_code = None;
    assert!(tracker.observe(unknown, vec![change('a', 'c')]).is_none());
    assert!(
        tracker
            .observe(run(3, 0, CheckKind::Behavioral), vec![change('a', 'd')])
            .is_none()
    );
}

#[test]
fn failure_snapshot_not_turn_start_is_the_precondition() {
    let mut tracker = Tracker::default();
    tracker.observe(run(1, 1, CheckKind::Behavioral), vec![change('a', 'b')]);
    let record = tracker
        .observe(run(2, 0, CheckKind::Behavioral), vec![change('a', 'c')])
        .unwrap();
    assert_eq!(record.changes[0].before_sha256, Some("b".repeat(64)));
    assert_eq!(
        record.changes[0].before_ref.as_deref(),
        Some("/state/after")
    );
}

#[test]
fn incomplete_checkpoint_cannot_silently_drop_failure_paths() {
    let mut tracker = Tracker::default();
    tracker.observe(run(1, 1, CheckKind::Behavioral), vec![change('a', 'b')]);
    let mut unrelated = change('a', 'c');
    unrelated.path = "src/other.rs".into();
    assert!(
        tracker
            .observe(run(2, 0, CheckKind::Behavioral), vec![unrelated])
            .is_none()
    );
}

#[test]
fn retrieval_keeps_complete_provenance_within_budget_and_rejects_forged_pass() {
    let mut tracker = Tracker::default();
    tracker.observe(run(1, 1, CheckKind::SyntaxOnly), vec![]);
    let record = tracker
        .observe(run(2, 0, CheckKind::SyntaxOnly), vec![change('a', 'b')])
        .unwrap();
    let payload = serde_json::to_value(record).unwrap();
    let text = retrieval_text(&[(7, payload.clone())], 4096).unwrap();
    assert!(text.len() <= 4096);
    assert!(text.contains("syntax_only"));
    assert!(text.contains("artifact://check-1"));
    assert!(text.contains("/state/before"));
    assert!(retrieval_text(&[(7, payload.clone())], text.len() - 1).is_none());
    let mut forged = payload;
    forged["before"]["exit_code"] = serde_json::json!(0);
    assert!(retrieval_text(&[(7, forged)], 4096).is_none());
}
