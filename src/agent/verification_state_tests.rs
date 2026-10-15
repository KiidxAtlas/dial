//! Provider-free regressions for parent-turn verification bookkeeping.
use super::*;

fn agent<'a>(config: &'a Config, root: &Path) -> Agent<'a> {
    // Construction only: no server or inference request is made by these tests.
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    Agent::hermetic(config, backend, &version, root, None, None).unwrap()
}

#[test]
fn empty_suites_do_not_supply_passing_behavioral_evidence() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config
        .verification
        .commands
        .push(crate::config::types::VerificationCommand {
            name: "behavior".into(),
            command: "cargo test focused".into(),
            file_extensions: vec![],
            timeout_secs: 5,
        });
    config
        .verification
        .behavioral_checks
        .push("behavior".into());
    let mut agent = agent(&config, root.path());
    let home = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("lib.rs"), "pub fn example() {}\n").unwrap();
    let layout = Layout::with_user_dir(root.path(), home.path().to_path_buf()).unwrap();
    let cache = Arc::new(Codebase::open(&layout).unwrap());
    cache
        .refresh(&agent.workspace, &AtomicBool::new(false))
        .unwrap();
    agent.codebase = Some(cache.clone());
    // Exercise the real cache-write branch too, with no session persistence or model requests.
    agent.hermetic = false;
    let cached_result = || -> Value {
        let conn = rusqlite::Connection::open(layout.state_dir.join("codebase.sqlite3")).unwrap();
        let raw: String = conn
            .query_row(
                "SELECT value FROM codebase_meta WHERE key='behavior_check'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    };
    let call = ToolCall {
        id: "focused-check:behavior".into(),
        name: "bash".into(),
        arguments: json!({"command":"cargo test focused"}),
        malformed_arguments: false,
    };
    for text in ["running 0 tests", "Ran 0 tests in 0.000s", "No tests found"] {
        agent.note_verification(
            &call,
            &ToolOutput {
                text: format!("{text}\n[exit code: 0]"),
                is_error: false,
                data: None,
                execution: Default::default(),
            }
            .with_process(crate::runtime::execution::ProcessOutcome {
                started: true,
                exit_code: Some(0),
                signal: None,
                timed_out: false,
                cancelled: false,
                output_complete: true,
            }),
            &mut NullUi,
        );
        assert_eq!(cached_result()["passed"], false);
    }
    assert_eq!(agent.behavioral_checks, vec![false; 3]);
    assert_eq!(agent.verifications, vec![false; 3]);
    agent.note_verification(
        &call,
        &ToolOutput {
            text: "running 2 tests\nrunning 0 tests\n[exit code: 0]".into(),
            is_error: false,
            data: None,
            execution: Default::default(),
        }
        .with_process(crate::runtime::execution::ProcessOutcome {
            started: true,
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            cancelled: false,
            output_complete: true,
        }),
        &mut NullUi,
    );
    assert_eq!(agent.behavioral_checks.last(), Some(&true));
    assert_eq!(cached_result()["passed"], true);
}

#[test]
fn round_settlement_preserves_verification_status_and_bounded_receipts() {
    let results = json!([{"name":"behavior","status":"unverified"}]);
    let receipts = json!([{"status":"unverified","source_digest":"abc"}]);
    let mut state = json!({
        "verification_results":results,
        "verification_receipts":receipts,
        "automatic_passing_checks":0,
        "automatic_failing_checks":1,
        "checked_commands":["behavior"]
    });
    let checks = StepChecks {
        failed: true,
        ..Default::default()
    };
    for _ in 0..3 {
        turn::settle_reliability(&mut state, &Default::default(), &None, false, &checks);
    }
    assert_eq!(state["verification_results"], results);
    assert_eq!(state["verification_receipts"], receipts);
    assert_eq!(state["automatic_failing_checks"], 1);
    assert_eq!(state["verification"], "failed");
    assert_eq!(state["checked_commands"], json!(["behavior"]));
}

#[test]
fn consultation_attempts_span_repair_state_and_reset_with_next_user_turn() {
    let settings = solve::EscalationConfig {
        enabled: true,
        auto_after_failed_rounds: 2,
        max_auto_per_turn: 1,
        ..Default::default()
    };
    let mut checks = StepChecks::default();
    checks.auto_escalation.remember("first failure");
    assert!(!checks.auto_escalation.round(true, &settings, true));
    // Final-check repair reentry keeps the same StepChecks, including the failure streak.
    assert!(checks.auto_escalation.round(true, &settings, true));
    // The attempt is charged before authorization/routing; neither denial nor a failed call refunds it.
    for _ in 0..4 {
        assert!(!checks.auto_escalation.round(true, &settings, true));
    }
    assert_eq!(
        checks.auto_escalation.question("goal")["evidence"],
        json!(["first failure"])
    );
    // The actual turn boundary replaces StepChecks with a fresh default-backed instance.
    checks = StepChecks::default();
    assert_eq!(
        checks.auto_escalation.question("goal")["evidence"],
        json!([])
    );
    assert!(!checks.auto_escalation.round(true, &settings, true));
    assert!(checks.auto_escalation.round(true, &settings, true));
}
