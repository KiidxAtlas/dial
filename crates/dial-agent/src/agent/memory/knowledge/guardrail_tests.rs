use super::*;
use crate::runtime::execution::ProcessOutcome;

fn setup() -> (tempfile::TempDir, Workspace, Codebase) {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("src/router.rs"),
        "pub fn route(path: &str) -> bool { path.starts_with('/') }\n",
    )
    .unwrap();
    std::fs::write(
        project.join("src/store.rs"),
        "pub fn open() -> bool { true }\n",
    )
    .unwrap();
    let layout = Layout::with_user_dir(&project, dir.path().join("user")).unwrap();
    let cache = Codebase::open(&layout).unwrap();
    let workspace = Workspace::new(&project).unwrap();
    (dir, workspace, cache)
}

#[test]
fn read_attaches_invariants_once_per_generation_and_resets_after_compaction() {
    let (_dir, workspace, cache) = setup();
    cache
        .remember_invariant(
            "src/router.rs",
            "All routes must begin with a leading slash before dispatch.",
            "s1",
        )
        .unwrap();

    let mut state = GuardrailState::default();
    let call = ToolCall {
        id: "c1".into(),
        name: "read".into(),
        arguments: json!({"path": "src/router.rs:1-10"}),
        malformed_arguments: false,
    };

    let mut first =
        ToolOutput::ok("1: pub fn route(path: &str) -> bool { path.starts_with('/') }".into());
    let diags = attach_guardrails_with(
        &cache,
        &workspace,
        &mut state,
        0,
        &call,
        &mut first,
        GuardrailEvidence {
            check_failed: false,
            attached_verify_failed: None,
            changed_files: &[],
            baselines: &BTreeMap::new(),
        },
    );
    assert!(diags.is_empty());
    assert!(
        first
            .text
            .contains("[Invariants (advisory, source-verified)]")
    );
    assert!(
        first
            .text
            .contains("All routes must begin with a leading slash before dispatch.")
    );

    // Second read in the same history generation attaches zero extra tokens.
    let mut second =
        ToolOutput::ok("1: pub fn route(path: &str) -> bool { path.starts_with('/') }".into());
    attach_guardrails_with(
        &cache,
        &workspace,
        &mut state,
        0,
        &call,
        &mut second,
        GuardrailEvidence {
            check_failed: false,
            attached_verify_failed: None,
            changed_files: &[],
            baselines: &BTreeMap::new(),
        },
    );
    assert!(!second.text.contains("[Invariants"));

    // Advancing history_generation (compaction/rewind/reset) allows it to attach again.
    let mut after_compact =
        ToolOutput::ok("1: pub fn route(path: &str) -> bool { path.starts_with('/') }".into());
    attach_guardrails_with(
        &cache,
        &workspace,
        &mut state,
        1,
        &call,
        &mut after_compact,
        GuardrailEvidence {
            check_failed: false,
            attached_verify_failed: None,
            changed_files: &[],
            baselines: &BTreeMap::new(),
        },
    );
    assert!(
        after_compact
            .text
            .contains("All routes must begin with a leading slash before dispatch.")
    );
}

#[test]
fn dead_ends_attach_only_on_failure_and_stay_silent_on_passing_runs() {
    let (dir, workspace, cache) = setup();
    let saved = cache
        .remember_dead_end(
            "src/store.rs",
            "test_wal_locking",
            "Opening a second write connection inside the lock deadlocks SQLite; open outside lock.",
            "s1",
        )
        .unwrap();
    let baseline_digest = saved["digest"].as_str().unwrap().to_owned();

    let mut state = GuardrailState::default();
    let bash_call = ToolCall {
        id: "c2".into(),
        name: "bash".into(),
        arguments: json!({"command": "cargo test test_wal_locking"}),
        malformed_arguments: false,
    };

    // Passing bash check produces zero dead-end tokens.
    let mut passing = ToolOutput::ok("test store::test_wal_locking ... ok".into());
    passing.execution.process = Some(ProcessOutcome {
        started: true,
        exit_code: Some(0),
        signal: None,
        timed_out: false,
        cancelled: false,
        output_complete: true,
    });
    attach_guardrails_with(
        &cache,
        &workspace,
        &mut state,
        0,
        &bash_call,
        &mut passing,
        GuardrailEvidence {
            check_failed: false,
            attached_verify_failed: None,
            changed_files: &["src/store.rs".into()],
            baselines: &BTreeMap::from([("src/store.rs".into(), baseline_digest.clone())]),
        },
    );
    assert!(!passing.text.contains("DEAD_END"));

    // Simulate editing src/store.rs in the current turn and hitting the failing test.
    std::fs::write(
        dir.path().join("project/src/store.rs"),
        "pub fn open() -> bool { false }\n",
    )
    .unwrap();
    let mut failing =
        ToolOutput::ok("test store::test_wal_locking ... FAILED\ndatabase is locked".into());
    failing.execution.process = Some(ProcessOutcome {
        started: true,
        exit_code: Some(101),
        signal: None,
        timed_out: false,
        cancelled: false,
        output_complete: true,
    });
    attach_guardrails_with(
        &cache,
        &workspace,
        &mut state,
        0,
        &bash_call,
        &mut failing,
        GuardrailEvidence {
            check_failed: true,
            attached_verify_failed: None,
            changed_files: &["src/store.rs".into()],
            baselines: &BTreeMap::from([("src/store.rs".into(), baseline_digest)]),
        },
    );
    assert!(
        failing
            .text
            .contains("[Dead ends (advisory, source-verified — avoid repeating)]")
    );
    assert!(
        failing
            .text
            .contains("Opening a second write connection inside the lock deadlocks SQLite")
    );
}
