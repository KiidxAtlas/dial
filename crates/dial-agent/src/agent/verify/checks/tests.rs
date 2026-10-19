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
    let check = |command: &str| crate::agent::verify::verification::FocusedCheck {
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
    let environment = crate::agent::verify::verification::FocusedCheck {
        name: "python AST: probe".into(),
        command: "echo 'No module named pytest'; exit 1".into(),
        timeout_secs: 30,
    };
    let code = crate::agent::verify::verification::FocusedCheck {
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

#[test]
fn completion_outcome_does_not_claim_unobserved_deliverable_or_test_coverage() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    let evidence = agent.completion_evidence(TurnOutcome::Completed);
    assert_eq!(evidence["execution_outcome"], "completed");
    assert_eq!(evidence["verification_coverage"], "not_observed");
    assert_eq!(
        evidence["deliverable_coverage"],
        "not_independently_assessed"
    );
    assert_eq!(evidence["visual_evidence"], "not_required");
    let output = agent.todo.call(&json!({"op":"set", "tasks":[
        {"content":"current"}, {"content":"still owed"}
    ]}));
    assert!(!output.is_error, "{}", output.text);
    let evidence = agent.completion_evidence(TurnOutcome::Completed);
    assert_eq!(evidence["outstanding_tasks"].as_array().unwrap().len(), 1);
    assert_eq!(evidence["outstanding_tasks"][0]["content"], "still owed");
    assert_eq!(evidence["deliverable_coverage"], "open_obligations");
    let incomplete = agent.completion_evidence(TurnOutcome::Incomplete);
    assert_eq!(incomplete["outstanding_tasks"].as_array().unwrap().len(), 2);
}

#[test]
fn completion_reports_stale_checks_and_missing_latest_visual_evidence_independently() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.evidence.observe(
        "test",
        "check",
        crate::agent::verify::verification::CheckStatus::Passed,
        [0, 0, 0],
        true,
    );
    agent.step_checks.mutations += 1;
    agent.step_checks.visual.changed(Path::new("style.css"));
    let evidence = agent.completion_evidence(TurnOutcome::Completed);
    assert_eq!(evidence["verification_coverage"], "partial_or_stale");
    assert_eq!(evidence["visual_evidence"], "missing");
    assert_eq!(evidence["checks"]["passing_behavioral_checks"], 0);
}

#[test]
fn hermetic_automatic_check_honors_denial_without_starting_a_process() {
    use crate::agent::safety::approval::{Effect, PermissionRule};
    use crate::agent::verify::verification::{CheckStatus, FocusedCheck, classify};
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    agent.always.push(PermissionRule {
        effect: Effect::Deny,
        tool: "bash".into(),
        command_prefix: vec!["touch".into()],
        paths: vec![],
    });
    agent.always.last().unwrap().validate().unwrap();
    let check = FocusedCheck {
        name: "denied".into(),
        command: "touch must-not-exist".into(),
        timeout_secs: 5,
    };
    let output = agent.run_focused_check(&check, &mut crate::agent::ui::surface::NullUi);
    assert_eq!(
        classify(&check, &output, false),
        CheckStatus::PermissionDenied
    );
    assert!(!root.path().join("must-not-exist").exists());
}

#[test]
fn plan_mode_refuses_automatic_check_even_with_auto_approval() {
    use crate::agent::verify::verification::{CheckStatus, FocusedCheck, classify};
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    agent.plan_mode = true;
    let check = FocusedCheck {
        name: "plan-only".into(),
        command: "touch must-not-exist".into(),
        timeout_secs: 5,
    };
    let output = agent.run_focused_check(&check, &mut crate::agent::ui::surface::NullUi);
    assert_eq!(
        classify(&check, &output, false),
        CheckStatus::PermissionDenied
    );
    assert!(!root.path().join("must-not-exist").exists());
}

#[test]
fn declared_requirement_selects_its_check_and_only_current_observed_runs_cover_it() {
    use crate::config::types::{VerificationCommand, VerificationRequirement};
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.verification.commands.push(VerificationCommand {
        name: "contract".into(),
        command: "python3 -m unittest boundaries".into(),
        file_extensions: vec!["never_matches".into()],
        timeout_secs: 30,
    });
    config.verification.requirements.insert(
        "negative-input".into(),
        VerificationRequirement {
            checks: vec!["contract".into()],
            visual: false,
        },
    );
    let mut agent = hermetic_agent(&config, root.path());
    let selected = agent.focused_checks(0);
    assert!(selected.iter().any(|check| check.name == "contract"));
    assert_eq!(
        agent.completion_evidence(TurnOutcome::Completed)["deliverable_coverage"],
        "requirements_uncovered"
    );
    let call = ToolCall {
        id: "behavior".into(),
        name: "bash".into(),
        arguments: json!({"command":"python3 -m unittest boundaries"}),
        malformed_arguments: false,
    };
    let printed = ToolOutput::ok("Ran 3 tests\nOK\n[exit code: 0]".into());
    agent.note_verification(&call, &printed, &mut NullUi);
    assert_eq!(
        agent.completion_evidence(TurnOutcome::Completed)["requirements"]["observed"],
        0
    );
    agent.note_verification(
        &call,
        &printed.with_process(completed_process(0)),
        &mut NullUi,
    );
    assert_eq!(
        agent.completion_evidence(TurnOutcome::Completed)["deliverable_coverage"],
        "declared_requirements_observed"
    );
    agent.step_checks.mutations += 1;
    let evidence = agent.completion_evidence(TurnOutcome::Completed);
    assert_eq!(
        evidence["requirements"]["requirements"][0]["checks"][0]["status"],
        "stale"
    );
    assert_eq!(evidence["deliverable_coverage"], "requirements_uncovered");
}
