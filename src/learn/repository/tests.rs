use super::*;
#[test]
fn snapshots_are_immutable_and_cannot_cross_splits() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    fs::write(root.path().join("source.py"), "answer=41").unwrap();
    fs::write(
        root.path().join("test.py"),
        "from source import answer; assert answer==42",
    )
    .unwrap();
    let manifest = root.path().join("task.json");
    let mut spec = Specification {
        id: "repair".into(),
        prompt: "Fix the answer".into(),
        split: "train".into(),
        files: vec!["source.py".into()],
        verification_files: vec!["test.py".into()],
        command: "python3 test.py".into(),
    };
    fs::write(&manifest, serde_json::to_vec(&spec).unwrap()).unwrap();
    let config = Config {
        project_root: root.path().into(),
        state_dir: state.path().into(),
        ..Config::default()
    };
    register(&config, &manifest).unwrap();
    spec.id = "leak".into();
    spec.split = "evaluation".into();
    fs::write(&manifest, serde_json::to_vec(&spec).unwrap()).unwrap();
    assert!(register(&config, &manifest).is_err());
    fs::write(root.path().join("source.py"), "answer=42").unwrap();
    assert_eq!(load(&config).unwrap()[0].files["source.py"], "answer=41");
    spec.files = vec!["../escape".into()];
    fs::write(&manifest, serde_json::to_vec(&spec).unwrap()).unwrap();
    assert!(register(&config, &manifest).is_err());
}
#[test]
fn retained_deletion_replays_before_and_after_independent_checks() {
    if !crate::tools::sandbox::require_for_test() {
        return;
    }
    use crate::agent::repair_evidence::{CheckKind, CheckRun, FileChange, Tracker};
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config = Config {
        project_root: project.path().canonicalize().unwrap(),
        user_dir: home.path().into(),
        state_dir: home.path().join("model"),
        ..Config::default()
    };
    let layout =
        crate::storage::paths::Layout::with_user_dir(&config.project_root, config.user_dir.clone())
            .unwrap();
    let retained = layout.checkpoints_dir().join("repair-evidence");
    fs::create_dir_all(&retained).unwrap();
    let content = "obsolete";
    let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
    let reference = retained.join(&digest);
    fs::write(&reference, content).unwrap();
    fs::write(project.path().join("keep.txt"), "keep").unwrap();
    fs::write(
        project.path().join("check.sh"),
        "test ! -e obsolete.txt && test -f keep.txt",
    )
    .unwrap();
    let spec = Specification {
        id: "deletion".into(),
        prompt: "Remove obsolete.txt and preserve keep.txt".into(),
        split: "train".into(),
        files: vec!["keep.txt".into(), "obsolete.txt".into()],
        verification_files: vec!["check.sh".into()],
        command: "sh check.sh".into(),
    };
    let manifest = project.path().join("exercise.json");
    fs::write(&manifest, serde_json::to_vec(&spec).unwrap()).unwrap();
    let store =
        crate::storage::store::Store::open(&config.state_dir, "model", "case", &config.harness)
            .unwrap();
    let version = store.active().unwrap().id;
    let trajectory = store
        .begin_trajectory(version, "coding", "repair", "remove obsolete file")
        .unwrap();
    let run = |sequence, exit_code| CheckRun {
        command: "sh check.sh".into(),
        cwd: config.project_root.to_string_lossy().into(),
        objective: "obsolete file absent".into(),
        kind: CheckKind::Behavioral,
        tool_call_id: format!("check-{sequence}"),
        evidence_ref: format!("artifact://{sequence}"),
        exit_code: Some(exit_code),
        sequence,
    };
    let mut tracker = Tracker::default();
    tracker.observe(run(1, 1), vec![]);
    let record = tracker
        .observe(
            run(2, 0),
            vec![FileChange {
                path: "obsolete.txt".into(),
                before_sha256: Some(digest),
                before_ref: Some(reference.to_string_lossy().into()),
                after_sha256: None,
                after_ref: None,
            }],
        )
        .unwrap();
    store
        .executable_repair(trajectory, version, &serde_json::to_value(record).unwrap())
        .unwrap();
    let snapshot = register_repair(&config, &store, &manifest, trajectory).unwrap();
    assert_eq!(snapshot.files["obsolete.txt"], "obsolete");
    assert!(!project.path().join("obsolete.txt").exists());
    assert!(store.verified_successes(100.0, 100).unwrap().is_empty());
}
#[cfg(unix)]
#[test]
fn solver_cannot_replace_sealed_tests_or_follow_symlinks() {
    if !crate::tools::sandbox::require_for_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let checks = BTreeMap::from([(
        "test.py".into(),
        "from source import answer; assert answer==42".into(),
    )]);
    fs::write(root.path().join("source.py"), "answer=41").unwrap();
    fs::write(root.path().join("test.py"), "pass").unwrap();
    assert!(!verify(root.path(), &checks, "python3 -B test.py"));
    fs::write(root.path().join("source.py"), "answer=42").unwrap();
    assert!(verify(root.path(), &checks, "python3 -B test.py"));
    fs::remove_file(root.path().join("test.py")).unwrap();
    std::os::unix::fs::symlink("source.py", root.path().join("test.py")).unwrap();
    assert!(!verify(root.path(), &checks, "python3 -B test.py"));
}
