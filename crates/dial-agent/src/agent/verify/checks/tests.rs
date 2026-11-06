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
fn check_log_identity_never_falls_back_to_old_or_rendered_output() {
    use crate::runtime::execution::OutputIntegrity;

    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    let store = crate::storage::artifact::ArtifactStore::new(state.path()).unwrap();
    let original = store.retain_exact_bytes(b"original log").unwrap();
    agent.workspace = agent.workspace.clone().with_artifacts(store.clone());
    let call = ToolCall {
        id: "focused-check:log".into(),
        name: "bash".into(),
        arguments: json!({"command":"printf check"}),
        malformed_arguments: false,
    };
    let output = ToolOutput::ok("filtered, unrelated presentation".into())
        .with_process(completed_process(0))
        .with_output_integrity(OutputIntegrity {
            artifact: Some(original.clone()),
            ..Default::default()
        });
    agent.note_verification(&call, &output, &mut NullUi);
    let reference = agent
        .step_checks
        .evidence
        .evidence_ref("printf check")
        .unwrap()
        .to_owned();
    let record = store.evidence_record(&reference).unwrap();
    assert_eq!(record.representation_uri, original);
    assert_eq!(
        store.evidence_bytes(&record, 4096).unwrap(),
        b"original log"
    );

    // Each is a new observation, including absent, invalid and evicted native log identities.
    // An unrelated retained output exists throughout and must never be substituted.
    store.retain_exact_bytes(b"unrelated newer log").unwrap();
    for artifact in [
        None,
        Some("artifact://invalid".into()),
        Some(format!("artifact://{}", "0".repeat(64))),
    ] {
        agent.note_verification(&call, &output, &mut NullUi);
        assert!(
            agent
                .step_checks
                .evidence
                .evidence_ref("printf check")
                .is_some()
        );
        let missing = ToolOutput::ok("original log".into())
            .with_process(completed_process(0))
            .with_output_integrity(OutputIntegrity {
                artifact,
                ..Default::default()
            });
        agent.note_verification(&call, &missing, &mut NullUi);
        assert!(
            agent
                .step_checks
                .evidence
                .evidence_ref("printf check")
                .is_none()
        );
    }
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

    let unknown_check = check("echo failed; exit 1");
    let unknown = agent.run_checks(std::slice::from_ref(&unknown_check), &mut NullUi);
    assert!(unknown.actionable.is_empty());
    assert_eq!(unknown.failures.len(), 1);
    assert!(unknown.failures[0].contains("cause is unconfirmed"));
    assert_eq!(agent.step_checks.failed_checks.len(), 3);
    assert!(agent.step_checks.failed_checks.contains(&unknown_check));
    assert!(agent.step_checks.failed);
    assert_eq!(
        agent
            .step_checks
            .evidence
            .latest_status(&unknown_check.command),
        Some(verify::verification::CheckStatus::FailedUnknownCause)
    );
}

#[test]
fn unknown_cause_reproduction_then_pass_clears_failure_without_fix_credit() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    let check = verify::verification::FocusedCheck {
        name: "repro: unknown".into(),
        command: "test -f fixed || { echo failed; exit 1; }".into(),
        timeout_secs: 5,
    };
    agent
        .step_checks
        .evidence
        .register_repro(check.clone())
        .unwrap();
    let failed = agent.run_checks(std::slice::from_ref(&check), &mut NullUi);
    assert_eq!(failed.failures.len(), 1);
    assert!(failed.actionable.is_empty());
    std::fs::write(root.path().join("fixed"), "").unwrap();
    agent.step_checks.mutations += 1;
    let passed = agent.run_checks(&[check], &mut NullUi);
    assert!(passed.failures.is_empty());
    assert!(!agent.step_checks.failed);
    assert_eq!(
        agent
            .step_checks
            .evidence
            .passing_behavioral(agent.step_checks.revision()),
        0
    );
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
fn unchanged_baseline_is_disclosed_without_repair_but_changed_diagnostic_is_actionable() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    let check = crate::agent::verify::verification::FocusedCheck {
        name: "unrelated".into(),
        command: "cat diagnostic; exit 1".into(),
        timeout_secs: 30,
    };
    std::fs::write(
        root.path().join("diagnostic"),
        "AssertionError: old_test expected 1 got 2\n",
    )
    .unwrap();
    let baseline = agent.run_checks(std::slice::from_ref(&check), &mut NullUi);
    assert_eq!(baseline.actionable.len(), 1);
    agent.step_checks.evidence.capture_baseline();
    let unchanged = agent.run_checks(std::slice::from_ref(&check), &mut NullUi);
    assert_eq!(unchanged.failures.len(), 1);
    assert!(unchanged.failures[0].contains("Unchanged baseline failure"));
    assert!(unchanged.actionable.is_empty());
    std::fs::write(
        root.path().join("diagnostic"),
        "AssertionError: new_test expected 1 got 3\n",
    )
    .unwrap();
    let changed = agent.run_checks(&[check], &mut NullUi);
    assert_eq!(changed.actionable.len(), 1);
    assert!(changed.actionable[0].contains("new_test"));
}

#[test]
fn configured_missing_program_uses_shared_environment_verdict_without_repair() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    let program = "dial_test_nonexistent_verification_runner_42";
    config
        .verification
        .commands
        .push(crate::config::types::VerificationCommand {
            name: "custom runner".into(),
            command: program.into(),
            file_extensions: vec![],
            timeout_secs: 30,
        });
    config.verification.runners.insert(
        "custom runner".into(),
        crate::config::types::VerificationRunner {
            program: program.into(),
            module: None,
        },
    );
    let mut agent = hermetic_agent(&config, root.path());
    let check = crate::agent::verify::verification::FocusedCheck {
        name: "custom runner".into(),
        command: program.into(),
        timeout_secs: 30,
    };
    let run = agent.run_checks(&[check], &mut NullUi);
    assert_eq!(run.failures.len(), 1);
    assert!(run.failures[0].contains("Configured verification runner unavailable"));
    assert!(run.actionable.is_empty());
    assert_eq!(
        agent.step_checks.evidence.latest_status(program),
        Some(crate::agent::verify::verification::CheckStatus::EnvironmentUnavailable)
    );
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
            ..Default::default()
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

#[test]
fn strict_completion_requires_observed_current_evidence_and_respects_no_auto_mode() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.verification.strict_requirements = true;
    config.verification.automatic_checks = false;
    config
        .verification
        .commands
        .push(crate::config::types::VerificationCommand {
            name: "contract".into(),
            command: "never-auto-launch".into(),
            file_extensions: vec![],
            timeout_secs: 5,
        });
    config.verification.requirements.insert(
        "boundary".into(),
        crate::config::types::VerificationRequirement {
            checks: vec!["contract".into()],
            visual: false,
            ..Default::default()
        },
    );
    let mut agent = hermetic_agent(&config, root.path());
    assert!(
        agent
            .validate_completion(&mut NullUi)
            .unwrap_err()
            .contains("boundary")
    );
    assert!(
        agent
            .step_checks
            .evidence
            .latest_status("never-auto-launch")
            .is_none()
    );
    let revision = agent.step_checks.revision();
    agent.step_checks.evidence.observe(
        "never-auto-launch",
        "observed",
        crate::agent::verify::verification::CheckStatus::Passed,
        revision,
        true,
    );
    assert!(agent.validate_completion(&mut NullUi).is_ok());
    agent.step_checks.mutations += 1;
    assert!(agent.validate_completion(&mut NullUi).is_err());
}

#[test]
fn strict_automatic_completion_runs_required_checks_once_and_refuses_permission_denial() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.verification.strict_requirements = true;
    config.verification.automatic_checks = true;
    config
        .verification
        .commands
        .push(crate::config::types::VerificationCommand {
            name: "contract".into(),
            command: "printf checked".into(),
            file_extensions: vec!["unused".into()],
            timeout_secs: 5,
        });
    config.verification.requirements.insert(
        "boundary".into(),
        crate::config::types::VerificationRequirement {
            checks: vec!["contract".into()],
            visual: false,
            ..Default::default()
        },
    );
    let mut agent = hermetic_agent(&config, root.path());
    agent.hermetic = false;
    assert!(agent.validate_completion(&mut NullUi).is_ok());
    let observed = agent.verifications.len();
    assert_eq!(observed, 1);
    assert!(agent.validate_completion(&mut NullUi).is_ok());
    assert_eq!(agent.verifications.len(), observed);
    agent.step_checks.mutations += 1;
    agent
        .always
        .push(crate::agent::safety::approval::PermissionRule {
            effect: crate::agent::safety::approval::Effect::Deny,
            tool: "bash".into(),
            command_prefix: vec!["printf".into()],
            paths: vec![],
        });
    assert!(agent.validate_completion(&mut NullUi).is_err());
    assert_eq!(
        agent.step_checks.evidence.latest_status("printf checked"),
        Some(crate::agent::verify::verification::CheckStatus::PermissionDenied)
    );
}

#[test]
fn requirement_mapped_baseline_failure_stays_actionable_without_behavioral_credit() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    let command = "echo 'assertion failed boundary'; exit 1";
    config
        .verification
        .commands
        .push(crate::config::types::VerificationCommand {
            name: "boundary-check".into(),
            command: command.into(),
            file_extensions: vec![],
            timeout_secs: 5,
        });
    config.verification.requirements.insert(
        "boundary".into(),
        crate::config::types::VerificationRequirement {
            checks: vec!["boundary-check".into()],
            visual: false,
            ..Default::default()
        },
    );
    let mut agent = hermetic_agent(&config, root.path());
    let check = crate::agent::verify::verification::FocusedCheck {
        name: "boundary-check".into(),
        command: command.into(),
        timeout_secs: 5,
    };
    agent.run_checks(std::slice::from_ref(&check), &mut NullUi);
    agent.step_checks.evidence.capture_baseline();
    let run = agent.run_checks(&[check], &mut NullUi);
    assert_eq!(run.failures.len(), 1);
    assert_eq!(run.actionable.len(), 1);
    assert!(!run.actionable[0].contains("outside declared behavioral scope"));
    assert_eq!(
        agent
            .step_checks
            .evidence
            .passing_behavioral(agent.step_checks.revision()),
        0
    );
}

fn input_contract_config(command: &str) -> (Config, verify::verification::FocusedCheck) {
    let mut config = Config::default();
    config
        .verification
        .commands
        .push(crate::config::types::VerificationCommand {
            name: "contract".into(),
            command: command.into(),
            file_extensions: vec![],
            timeout_secs: 15,
        });
    config.verification.inputs.insert(
        "contract".into(),
        crate::config::types::VerificationInputs {
            paths: vec!["input".into()],
            environment: vec![],
        },
    );
    config
        .verification
        .behavioral_checks
        .push("contract".into());
    let check = verify::verification::FocusedCheck {
        name: "contract".into(),
        command: command.into(),
        timeout_secs: 15,
    };
    (config, check)
}

#[test]
fn concurrent_input_changes_withhold_pass_and_failure_credit_but_preserve_process_facts() {
    for (remove, exit) in [(false, 0), (false, 1), (true, 0), (true, 1)] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("input"), "before").unwrap();
        // A handshake, not a timing guess: the peer writes only while the real shell is running.
        let command = format!(
            "touch started; while test ! -f changed; do sleep 0.01; done; echo 'assertion diagnostic'; exit {exit}"
        );
        let (config, check) = input_contract_config(&command);
        let mut agent = hermetic_agent(&config, root.path());
        let peer_root = root.path().to_owned();
        let peer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !peer_root.join("started").exists() {
                assert!(Instant::now() < deadline, "focused process never started");
                std::thread::sleep(Duration::from_millis(5));
            }
            if remove {
                std::fs::remove_file(peer_root.join("input")).unwrap();
            } else {
                std::fs::write(peer_root.join("input"), "after").unwrap();
            }
            std::fs::write(peer_root.join("changed"), "ready").unwrap();
        });
        let output = agent.run_focused_check(&check, &mut NullUi);
        peer.join().unwrap();
        assert_eq!(output.process_outcome().unwrap().exit_code, Some(exit));
        assert!(output.process_outcome().unwrap().output_complete);
        assert!(output.is_error);
        assert!(output.text.contains("Verification unverified"));
        assert_eq!(
            agent.step_checks.evidence.latest_status(&command),
            Some(verify::verification::CheckStatus::Unverified)
        );
        assert_eq!(agent.verifications, vec![false]);
        assert_eq!(agent.behavioral_checks, vec![false]);
        assert!(!agent.reuse_check(&check));
    }
}

#[test]
fn failed_contract_capture_is_unverified_in_gate_and_check_tool() {
    for exit in [0, 1] {
        let root = tempfile::tempdir().unwrap();
        let command = format!("echo 'assertion diagnostic'; exit {exit}");
        let (config, check) = input_contract_config(&command);
        let mut agent = hermetic_agent(&config, root.path());
        let run = agent.run_checks(std::slice::from_ref(&check), &mut NullUi);
        assert_eq!(run.failures.len(), 1);
        assert!(run.actionable.is_empty());
        assert!(run.failures[0].contains("Verification unverified"));
        assert!(
            run.failures[0].contains("contract unverified:"),
            "{}",
            run.failures[0]
        );
        assert!(
            run.failures[0]
                .contains("declared input contract could not be captured before execution"),
            "{}",
            run.failures[0]
        );
        let output = agent.check_tool(&json!({"names":["contract"]}), &mut NullUi);
        assert!(output.is_error);
        assert!(
            output.text.contains("unverified: contract"),
            "{}",
            output.text
        );
        assert!(!agent.reuse_check(&check));
    }
}

#[test]
fn external_changes_and_disappearance_invalidate_passes_without_local_counter_changes() {
    for remove in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("input"), "before").unwrap();
        let (config, check) = input_contract_config("echo checked");
        let mut agent = hermetic_agent(&config, root.path());
        agent.run_focused_check(&check, &mut NullUi);
        assert!(agent.reuse_check(&check));
        let revision = agent.step_checks.revision();
        if remove {
            std::fs::remove_file(root.path().join("input")).unwrap();
        } else {
            std::fs::write(root.path().join("input"), "after").unwrap();
        }
        // Opening progress must revalidate external changes, not present an old pass as current.
        agent.progress_view();
        assert_eq!(agent.step_checks.revision(), revision);
        assert_eq!(
            agent
                .step_checks
                .evidence
                .current_status(&check.command, revision),
            "stale"
        );
        assert_eq!(agent.step_checks.evidence.passing_behavioral(revision), 0);
        // Restoring bytes must not revive invalidated evidence without execution.
        std::fs::write(root.path().join("input"), "before").unwrap();
        assert!(!agent.reuse_check(&check));
    }
}

#[test]
fn failed_reuse_capture_is_sticky_even_if_input_is_restored() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("input"), "before").unwrap();
    let (config, check) = input_contract_config("echo checked");
    let mut agent = hermetic_agent(&config, root.path());
    agent.run_focused_check(&check, &mut NullUi);
    std::fs::remove_file(root.path().join("input")).unwrap();
    assert!(!agent.reuse_check(&check));
    std::fs::write(root.path().join("input"), "before").unwrap();
    assert!(!agent.reuse_check(&check));
    agent.run_focused_check(&check, &mut NullUi);
    assert!(agent.reuse_check(&check));
}

#[test]
fn subset_checks_preserve_unrelated_obligations_across_pass_reuse_and_cancellation() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("input"), "stable").unwrap();
    let (config, passed) = input_contract_config("echo checked");
    let mut agent = hermetic_agent(&config, root.path());
    let unrelated = verify::verification::FocusedCheck {
        name: "unrelated".into(),
        command: "echo failed; exit 1".into(),
        timeout_secs: 5,
    };
    agent.run_checks(std::slice::from_ref(&unrelated), &mut NullUi);
    for _ in 0..2 {
        let run = agent.run_checks(std::slice::from_ref(&passed), &mut NullUi);
        assert!(run.failures.is_empty());
        assert_eq!(agent.step_checks.failed_checks, vec![unrelated.clone()]);
        assert!(agent.step_checks.failed);
    }
    assert!(agent.reuse_check(&passed));
    agent.interrupt.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        agent.run_checks(std::slice::from_ref(&passed), &mut NullUi);
        assert_eq!(
            agent.step_checks.failed_checks,
            vec![unrelated.clone(), passed.clone()]
        );
    }
    agent.interrupt.store(false, Ordering::SeqCst);
    agent.run_checks(std::slice::from_ref(&passed), &mut NullUi);
    assert_eq!(agent.step_checks.failed_checks, vec![unrelated]);
    agent.run_checks(&[], &mut NullUi);
    assert!(agent.step_checks.failed);
}

#[test]
fn real_shell_incomplete_check_hidden_in_clipped_output_is_unverified() {
    for diagnostic in [
        "Ran 0 tests",
        "<stdin>:3: RuntimeWarning: coroutine 'check' was never awaited",
    ] {
        let root = tempfile::tempdir().unwrap();
        let artifact_root = tempfile::tempdir().unwrap();
        let config = Config::default();
        let mut agent = hermetic_agent(&config, root.path());
        agent.workspace = agent.workspace.clone().with_artifacts(
            crate::storage::artifact::ArtifactStore::new(artifact_root.path()).unwrap(),
        );
        let padding = "padding diagnostic\n".repeat(10000);
        std::fs::write(
            root.path().join("output"),
            format!("{padding}{diagnostic}\n{padding}"),
        )
        .unwrap();
        let check = verify::verification::FocusedCheck {
            name: "incomplete check".into(),
            command: "cat output".into(),
            timeout_secs: 5,
        };
        let output = agent.run_focused_check(&check, &mut NullUi);
        assert_eq!(output.process_outcome().unwrap().exit_code, Some(0));
        assert!(!output.text.contains(diagnostic));
        assert_eq!(
            agent.step_checks.evidence.latest_status(&check.command),
            Some(verify::verification::CheckStatus::Unverified)
        );
    }
}

#[test]
fn a_python_check_that_never_awaits_its_coroutine_is_unverified() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    let check = verify::verification::FocusedCheck {
        name: "async behavior".into(),
        command: "python3 - <<'PY'\nasync def regression():\n    raise AssertionError('must execute')\nregression()\nPY".into(),
        timeout_secs: 5,
    };
    let output = agent.run_focused_check(&check, &mut NullUi);
    assert_eq!(output.process_outcome().unwrap().exit_code, Some(0));
    assert!(output.text.contains("was never awaited"), "{}", output.text);
    assert_eq!(
        agent.step_checks.evidence.latest_status(&check.command),
        Some(verify::verification::CheckStatus::Unverified)
    );
}

#[test]
fn strict_required_subset_cannot_discharge_an_unrelated_failed_check() {
    use crate::config::types::{VerificationCommand, VerificationRequirement};
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.verification.strict_requirements = true;
    config.verification.automatic_checks = true;
    config.verification.commands.push(VerificationCommand {
        name: "required".into(),
        command: "printf checked".into(),
        file_extensions: vec![],
        timeout_secs: 5,
    });
    config.verification.requirements.insert(
        "boundary".into(),
        VerificationRequirement {
            checks: vec!["required".into()],
            visual: false,
            ..Default::default()
        },
    );
    let mut agent = hermetic_agent(&config, root.path());
    let unrelated = verify::verification::FocusedCheck {
        name: "unrelated".into(),
        command: "echo assertion failed; exit 1".into(),
        timeout_secs: 5,
    };
    agent.run_checks(std::slice::from_ref(&unrelated), &mut NullUi);
    agent.hermetic = false;
    let refusal = agent.validate_completion(&mut NullUi).unwrap_err();
    assert!(refusal.contains("focused checks failed"), "{refusal}");
    assert_eq!(
        agent.step_checks.evidence.latest_status("printf checked"),
        Some(verify::verification::CheckStatus::Passed)
    );
    assert_eq!(agent.step_checks.failed_checks, vec![unrelated]);
    assert!(agent.step_checks.failed);
}

#[test]
fn finite_approval_choices_are_exact_consumed_and_never_reusable() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    agent.approval = crate::config::types::Approval::Ask;
    let call = ToolCall {
        id: "edit-1".into(),
        name: "write".into(),
        arguments: json!({"path":"new.txt", "content":"first"}),
        malformed_arguments: false,
    };
    agent.pending_batch_approvals.push((call.clone(), true));
    let mut changed = call.clone();
    changed.arguments["content"] = json!("different");
    assert!(
        agent.authorize_call(&changed, &mut NullUi).is_err(),
        "changed arguments must prompt, not consume the ticket"
    );
    assert!(agent.authorize_call(&call, &mut NullUi).is_ok());
    assert!(
        agent.authorize_call(&call, &mut NullUi).is_err(),
        "one-shot choice cannot authorize a second execution"
    );
    assert!(
        agent.always.is_empty(),
        "batch approval must not create rules"
    );
    agent.pending_batch_approvals.push((call.clone(), false));
    agent.approval = crate::config::types::Approval::Auto;
    assert!(
        agent.authorize_call(&call, &mut NullUi).is_err(),
        "a declined exact call remains declined even if policy changes"
    );
}

#[test]
fn background_check_overlaps_reads_and_settles_stale_inputs_without_credit() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("input"), "before").unwrap();
    let command = "touch started; while test ! -f release; do sleep 0.01; done; exit 0";
    let (mut config, check) = input_contract_config(command);
    config.experimental.background_checks = true;
    config.verification.automatic_checks = true;
    config.experimental.fault_witness = false;
    let mut agent = hermetic_agent(&config, root.path());
    agent.hermetic = false;
    agent.approval = crate::config::types::Approval::Auto;
    agent.start_background_check(&mut NullUi);
    assert!(
        agent.pending_check.is_some(),
        "configured check should have a controller-owned process"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.path().join("started").exists() {
        assert!(Instant::now() < deadline, "background check never started");
        std::thread::sleep(Duration::from_millis(5));
    }
    let read = tools::execute(&agent.workspace, "read", &json!({"path":"input"}), 5);
    assert!(
        !read.is_error,
        "independent read must progress while check waits: {}",
        read.text
    );
    agent.drain_background_check(&mut NullUi, false);
    assert!(
        agent.pending_check.is_some(),
        "nonblocking settlement must not wait"
    );
    std::fs::write(root.path().join("input"), "external change").unwrap();
    std::fs::write(root.path().join("release"), "ready").unwrap();
    agent.drain_background_check(&mut NullUi, true);
    assert!(agent.pending_check.is_none());
    assert_eq!(
        agent.step_checks.evidence.latest_status(command),
        Some(verify::verification::CheckStatus::Unverified)
    );
    assert!(
        !agent.reuse_check(&check),
        "stale run must not satisfy completion"
    );
}

#[test]
fn background_check_denial_never_starts_a_process() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("input"), "before").unwrap();
    let (mut config, _) = input_contract_config("touch unauthorized");
    config.experimental.background_checks = true;
    config.verification.automatic_checks = true;
    let mut agent = hermetic_agent(&config, root.path());
    agent.hermetic = false;
    agent.approval = crate::config::types::Approval::Ask;
    agent.start_background_check(&mut NullUi);
    assert!(agent.pending_check.is_none());
    assert!(!root.path().join("unauthorized").exists());
    assert_eq!(
        agent
            .step_checks
            .evidence
            .latest_status("touch unauthorized"),
        Some(verify::verification::CheckStatus::PermissionDenied)
    );
}

fn set_deliverables(agent: &mut Agent<'_>, deliverables: Value) {
    let state = crate::agent::workflow::working_state::apply(
        &Value::Null,
        &json!({"action":"update","deliverables":deliverables}),
    )
    .unwrap();
    agent
        .session_mut()
        .set_control_state(json!({"working_state": state}))
        .unwrap();
}

fn satisfied_by(id: &str, reference: &str) -> Value {
    json!({"id":id,"description":"deliverable","status":"satisfied","evidence_refs":[reference]})
}

fn observe_at(
    agent: &mut Agent<'_>,
    command: &str,
    status: crate::agent::verify::verification::CheckStatus,
) {
    let revision = agent.step_checks.revision();
    agent
        .step_checks
        .evidence
        .observe(command, "call", status, revision, false);
}

fn command(name: &str, command: &str) -> crate::config::types::VerificationCommand {
    crate::config::types::VerificationCommand {
        name: name.into(),
        command: command.into(),
        file_extensions: vec![],
        timeout_secs: 5,
    }
}

#[test]
fn unresolved_check_links_block_completion_with_exact_reason() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config
        .verification
        .commands
        .push(command("build", "echo built"));
    let mut agent = hermetic_agent(&config, root.path());
    for (reference, label) in [
        ("check:build", "not_observed"),
        ("check:nope", "unknown_check"),
    ] {
        set_deliverables(&mut agent, json!([satisfied_by("ship", reference)]));
        let expected = format!("ship -> {reference} ({label})");
        let blocker = agent.completion_blockers().unwrap_err();
        assert!(blocker.contains(&expected), "{blocker}");
        let evidence = agent.completion_evidence(TurnOutcome::Completed);
        assert_eq!(evidence["unpassed_check_links"], json!([expected]));
        assert_eq!(evidence["deliverable_coverage"], "open_obligations");
    }
}

#[test]
fn check_link_status_is_the_current_observation_not_a_declaration() {
    use crate::agent::verify::verification::CheckStatus;
    let cases = [
        (Some(CheckStatus::CodeFailed), "code_failed"),
        (Some(CheckStatus::TimedOut), "timed_out"),
        (Some(CheckStatus::Unverified), "unverified"),
        (Some(CheckStatus::PermissionDenied), "permission_denied"),
        (None, "stale"),
    ];
    for (status, label) in cases {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config
            .verification
            .commands
            .push(command("build", "echo built"));
        let mut agent = hermetic_agent(&config, root.path());
        match status {
            Some(status) => observe_at(&mut agent, "echo built", status),
            None => {
                observe_at(&mut agent, "echo built", CheckStatus::Passed);
                agent.step_checks.mutations += 1;
            }
        }
        set_deliverables(&mut agent, json!([satisfied_by("ship", "check:build")]));
        let blocker = agent.completion_blockers().unwrap_err();
        assert!(
            blocker.contains(&format!("ship -> check:build ({label})")),
            "{label}: {blocker}"
        );
    }
}

#[test]
fn current_passing_check_link_resolves_without_running_the_command() {
    use crate::agent::verify::verification::CheckStatus;
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config
        .verification
        .commands
        .push(command("build", "touch must-not-exist-build"));
    let mut agent = hermetic_agent(&config, root.path());
    agent
        .step_checks
        .evidence
        .register_repro(crate::agent::verify::verification::FocusedCheck {
            name: "repro".into(),
            command: "touch must-not-exist-repro".into(),
            timeout_secs: 5,
        })
        .unwrap();
    observe_at(
        &mut agent,
        "touch must-not-exist-build",
        CheckStatus::Passed,
    );
    observe_at(
        &mut agent,
        "touch must-not-exist-repro",
        CheckStatus::Passed,
    );
    set_deliverables(
        &mut agent,
        json!([
            satisfied_by("ship", "check:build"),
            satisfied_by("repro-case", "check:repro")
        ]),
    );
    assert!(agent.completion_blockers().is_ok());
    let evidence = agent.completion_evidence(TurnOutcome::Completed);
    assert_eq!(evidence["unpassed_check_links"], json!([]));
    assert!(!root.path().join("must-not-exist-build").exists());
    assert!(!root.path().join("must-not-exist-repro").exists());
}

#[test]
fn ambiguous_check_name_never_resolves_from_one_passing_command() {
    use crate::agent::verify::verification::CheckStatus;
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config
        .verification
        .commands
        .push(command("build", "echo a"));
    config
        .verification
        .commands
        .push(command("build", "echo b"));
    let mut agent = hermetic_agent(&config, root.path());
    observe_at(&mut agent, "echo a", CheckStatus::Passed);
    set_deliverables(&mut agent, json!([satisfied_by("ship", "check:build")]));
    let blocker = agent.completion_blockers().unwrap_err();
    assert!(
        blocker.contains("ship -> check:build (ambiguous_check)"),
        "{blocker}"
    );
}

#[test]
fn file_and_artifact_refs_stay_unverified_and_evidence_separates_declared_from_outstanding() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    // Historical cancellations lack a runtime scope receipt and must remain outstanding.
    agent.session_mut().set_control_state(json!({"working_state":{"deliverables":[
        {"id":"ship","description":"d","status":"satisfied","evidence_refs":["artifact://log","src/lib.rs"]},
        {"id":"old","description":"d","status":"cancelled"},
        {"id":"next","description":"d","status":"open"}
    ]}})).unwrap();
    assert!(agent.completion_blockers().is_err());
    let evidence = agent.completion_evidence(TurnOutcome::Completed);
    assert_eq!(evidence["unpassed_check_links"], json!([]));
    assert_eq!(
        evidence["declared_deliverables"].as_array().unwrap().len(),
        3
    );
    assert_eq!(
        evidence["outstanding_deliverables"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(evidence["outstanding_deliverables"][0]["id"], "old");
    assert_eq!(evidence["outstanding_deliverables"][1]["id"], "next");
    assert_eq!(
        evidence["deliverable_claim_policy"],
        "model_reported_not_independent_verification"
    );
}

#[test]
fn malformed_stored_deliverables_block_completion_without_panicking() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    agent
        .session_mut()
        .set_control_state(json!({"working_state": {"deliverables": "oops"}}))
        .unwrap();
    let blocker = agent.completion_blockers().unwrap_err();
    assert!(blocker.contains("invalid"), "{blocker}");
    let _ = agent.completion_evidence(TurnOutcome::Completed);
}

#[test]
fn reconciliation_distinguishes_claims_current_checks_and_stale_evidence() {
    use crate::agent::verify::verification::CheckStatus;
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config
        .verification
        .commands
        .push(command("build", "echo built"));
    let mut agent = hermetic_agent(&config, root.path());
    set_deliverables(
        &mut agent,
        json!([
            satisfied_by("tested", "check:build"),
            satisfied_by("claimed", "artifact://unverified"),
            {"id":"remaining","description":"second outcome","status":"open"}
        ]),
    );
    observe_at(&mut agent, "echo built", CheckStatus::Passed);
    let evidence = agent.completion_evidence(TurnOutcome::Completed);
    let reconciliation = &evidence["reconciliation"];
    assert_eq!(
        reconciliation["deliverables"][0]["evidence_status"],
        "linked_checks_passed"
    );
    assert_eq!(
        reconciliation["deliverables"][1]["evidence_status"],
        "claim_only"
    );
    assert_eq!(reconciliation["deliverables"][2]["claimed_status"], "open");
    assert_eq!(reconciliation["undeclared_requirements"], "unassessed");
    assert!(agent.completion_blockers().is_err());
    agent.step_checks.mutations += 1;
    let stale = agent.completion_evidence(TurnOutcome::Completed);
    assert_eq!(
        stale["reconciliation"]["deliverables"][0]["evidence_status"],
        "linked_checks_unresolved"
    );
    assert_eq!(
        stale["reconciliation"]["deliverables"][0]["checks"][0]["status"],
        "stale"
    );
}

#[test]
fn deliverable_contract_gates_claims_independently_of_cited_refs_and_strict_mode() {
    use crate::agent::verify::verification::CheckStatus;
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.verification.commands = vec![
        command("required", "echo required"),
        command("unrelated", "echo unrelated"),
    ];
    config.verification.requirements.insert(
        "behavior".into(),
        crate::config::types::VerificationRequirement {
            checks: vec!["required".into()],
            deliverables: vec!["ship".into()],
            ..Default::default()
        },
    );
    assert!(!config.verification.strict_requirements);
    let mut agent = hermetic_agent(&config, root.path());
    observe_at(&mut agent, "echo unrelated", CheckStatus::Passed);
    for reference in ["src/main.rs", "artifact://claim", "check:unrelated"] {
        set_deliverables(&mut agent, json!([satisfied_by("ship", reference)]));
        assert!(agent.completion_blockers().unwrap_err().contains("ship"));
        let evidence = agent.completion_evidence(TurnOutcome::Completed);
        assert_eq!(
            evidence["reconciliation"]["deliverables"][0]["acceptance"]["status"],
            "needs_verification"
        );
    }
    observe_at(&mut agent, "echo required", CheckStatus::Passed);
    assert!(agent.completion_blockers().is_ok());
    assert_eq!(
        agent.completion_evidence(TurnOutcome::Completed)["reconciliation"]["deliverables"][0]["acceptance"]
            ["status"],
        "observed"
    );
    agent.step_checks.mutations += 1;
    assert!(agent.completion_blockers().unwrap_err().contains("ship"));
}
