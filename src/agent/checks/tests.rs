use super::*;

fn hermetic_agent<'a>(config: &'a Config, root: &Path) -> Agent<'a> {
    // Construction only: hermetic agents make no provider request and write no session state.
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

fn completed_process(exit_code: i32) -> crate::runtime::execution::ProcessOutcome {
    crate::runtime::execution::ProcessOutcome {
        started: true,
        exit_code: Some(exit_code),
        signal: None,
        timed_out: false,
        cancelled: false,
        output_complete: true,
    }
}

#[test]
fn printed_exit_code_earns_no_behavior_credit_without_observed_completion() {
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
    let mut agent = hermetic_agent(&config, root.path());
    let call = ToolCall {
        id: "focused-check:behavior".into(),
        name: "bash".into(),
        arguments: json!({"command":"cargo test focused"}),
        malformed_arguments: false,
    };
    let prose = ToolOutput {
        text: "running 2 tests\ntest result: ok. 2 passed\n[exit code: 0]".into(),
        is_error: false,
        data: None,
        execution: Default::default(),
    };
    agent.note_verification(&call, &prose, &mut NullUi);
    assert_eq!(agent.verifications, vec![false]);
    assert_eq!(agent.behavioral_checks, vec![false]);

    let observed = prose.with_process(completed_process(0));
    agent.note_verification(&call, &observed, &mut NullUi);
    assert_eq!(agent.verifications, vec![false, true]);
    assert_eq!(agent.behavioral_checks, vec![false, true]);
}

#[test]
fn only_confirmed_code_failures_drive_automatic_repair() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    let check = |command: &str| verification::FocusedCheck {
        name: "probe".into(),
        command: command.into(),
        timeout_secs: 30,
    };

    let failed = agent.run_checks(&[check("echo assertion failed; exit 1")], &mut NullUi);
    assert_eq!(failed.actionable.len(), 1);
    assert_eq!(failed.failures.len(), 1);

    // Exit 0 without any executed test is unverifiable: report it, but never send it to code repair.
    let empty = agent.run_checks(&[check("echo 'Ran 0 tests'; exit 0")], &mut NullUi);
    assert!(empty.actionable.is_empty());
    assert_eq!(empty.failures.len(), 1);
}

#[test]
fn mixed_results_send_only_code_failures_to_repair() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    let environment = verification::FocusedCheck {
        name: "python AST: probe".into(),
        command: "echo 'No module named pytest'; exit 1".into(),
        timeout_secs: 30,
    };
    let code = verification::FocusedCheck {
        name: "probe".into(),
        command: "echo 'assertion failed'; exit 1".into(),
        timeout_secs: 30,
    };
    let run = agent.run_checks(&[environment, code], &mut NullUi);
    // Every final problem stays visible; only the confirmed code failure is an actionable diagnosis.
    assert_eq!(run.failures.len(), 2);
    assert!(run.failures[0].contains("Verification environment unavailable"));
    assert_eq!(run.actionable.len(), 1);
    assert!(run.actionable[0].starts_with("probe failed"));
    assert!(!run.actionable[0].contains("pytest"));
}
