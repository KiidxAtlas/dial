use super::*;

#[test]
fn active_follow_up_stays_in_completion_evidence_without_becoming_a_requirement() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    assert!(
        !agent
            .todo
            .call(
                &json!({"op":"set","tasks":[{"content":"Optional experiment", "kind":"follow_up"}]})
            )
            .is_error
    );
    assert!(!agent.todo.call(&json!({"op":"start","index":1})).is_error);
    let evidence = agent.completion_evidence(TurnOutcome::Completed);
    assert_eq!(evidence["outstanding_tasks"].as_array().unwrap().len(), 1);
    assert_eq!(evidence["optional_follow_ups"].as_array().unwrap().len(), 1);
    assert_ne!(evidence["deliverable_coverage"], "open_obligations");
    assert!(agent.completion_blockers().is_ok());
}

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
fn pipefail_must_apply_to_the_checked_foreground_pipeline() {
    for command in [
        "cd /app && Rscript -e 'source(\"ars.R\"); x <- ars(dnorm, 10)' 2>&1 | tail -20",
        "pytest -q |& tee log",
        "Rscript -e '\n  a <- 1 | 2\n' | head",
        "python3 - <<'PY' | tail -5\nprint(1)\nPY",
        "false | true # set -o pipefail",
        "echo 'set -o pipefail'; false | true",
        "set -o pipefail; set +o pipefail; false | true",
        "(set -o pipefail); false | true",
        "set -o pipefail & false | true",
        "false && set -o pipefail; false | true",
        "false | true; set -o pipefail",
        "set -o pipefail; false | true; echo done",
        "set -o pipefail; ! false | true",
        "set -o pipefail; false | true; true",
        "set -o pipefail; false | true || true",
        "set -o pipefail; (false | true) && true",
        "set -o pipefail; false | true | true || true",
        "set -o pipefail; false | true | true && echo ok; true",
        "set -o pipefail | true | true && echo ok",
    ] {
        assert!(exit_status_masked(command), "{command}");
    }
    for command in [
        "set -o pipefail; pytest -q | tail -20",
        "# run a check\nset -euo pipefail\npytest -q 2>&1 |& tee log",
        "set -o pipefail && make test | tail -5 > summary",
        "make test || echo failed",
        "python3 - <<'PY'\nflags = a | b\nPY",
        "cd /app && pytest -q > log 2>&1 && tail -5 log",
        "cat notes.txt; pytest -q",
        "grep -E 'a|b' out.txt",
        "pytest -q # | tail",
        "echo yes >| out",
        "test \"$(cat out.csv | head -1)\" = value",
        "set -o pipefail; cd /app && pytest -q | tail -20",
        "set -o pipefail; chmod -R a+rX /tmp/t; su git -c 'git push' && [ \"$(curl -sk https://localhost:8443/index.html)\" = \"ok\" ] && ssh-keyscan -p 22 localhost 2>&1 | grep -q ssh-",
        "set -o pipefail && cd /app && make test | tail -5 > summary",
        "set -o pipefail; cmd1 | grep -q a && cmd2 | grep -q b",
        "set -o pipefail; printf 'Results: 12 30 8\\n' | tail -n 1 | grep -Eq '^Results: [0-9]+ [0-9]+ [0-9]+$' && echo ok",
    ] {
        assert!(!exit_status_masked(command), "{command}");
    }
}

#[test]
fn a_trailing_display_command_masks_the_check_status() {
    for command in [
        "cd /app/pyknotid && python -m pytest tests -q >/tmp/o.txt 2>&1; echo exit=$?; tail -1 /tmp/o.txt",
        "Rscript -e 'test()' > /tmp/out.txt 2>&1\necho exit=$?",
        "python3 - <<'PY'\nflags = a | b\nPY\necho done",
    ] {
        assert!(exit_status_masked(command), "{command}");
        assert!(masked_check_note(command).contains("ends with a display command"));
    }
}

#[test]
fn reference_tools_are_the_ones_the_request_calls_ground_truth() {
    let dna = "Design primers.\n * The output of primer3's oligotm tool should be considered the ground truth for melting temperatures with the following flags: `-tp 1 -sc 1`\n * Output the minimum number of pairs.";
    assert_eq!(reference_tools(dna), ["oligotm"]);
    assert_eq!(
        reference_tools("Treat `jq` as the source of truth for the output."),
        ["jq"]
    );
    assert!(reference_tools("Use the oligotm tool to check primers.").is_empty());
    assert!(reference_tools("Write a CLI tool in Rust.").is_empty());
    assert!(names_word("cd /app; oligotm -tp 1 ACGT", "oligotm"));
    assert!(!names_word("python3 my_oligotm_clone.py", "oligotm"));
}

#[test]
fn requested_speed_is_how_the_request_asks_for_it() {
    let query = "I implemented a sql query but it is not optimized. Please make the query as efficient as possible while ensuring that the same output is produced.";
    assert_eq!(requested_speed(query).as_deref(), Some("optimized"));
    assert_eq!(
        requested_speed("Speed up the parser").as_deref(),
        Some("speed up")
    );
    assert_eq!(
        requested_speed("Make it faster than numpy").as_deref(),
        Some("faster")
    );
    for plain in [
        "Fix the failing test",
        "Report the Python runtime version",
        "Write the token count to answer.txt",
    ] {
        assert_eq!(requested_speed(plain), None, "{plain}");
    }
}

#[test]
fn requested_functions_are_names_the_request_asks_for() {
    let ars = "The primary input should be a function (many of the \"d\" functions in R, such as \"dnorm\").\n- Include a primary function named \"ars\" that implements the sampler\n- Include a test function named \"test\" that performs formal testing\n- The \"ars\" function must validate inputs";
    assert_eq!(requested_functions(ars), vec!["ars", "test"]);
    assert_eq!(
        requested_functions("Add a `parse_args()` function and a method called `run`."),
        vec!["parse_args", "run"]
    );
    assert!(requested_functions("Write a function that sorts the list.").is_empty());
    assert!(requested_functions("the \"d\" function family").is_empty());
}

#[test]
fn display_only_and_test_excluding_checks_are_not_counted() {
    let none = std::collections::BTreeSet::new();
    for command in [
        "cat /app/move.txt",
        "cd /app && ls -la out/ && head -5 result.csv 2>&1",
        "wc -l out.txt | cat",
    ] {
        assert!(
            uncounted_pass_note(command, &none, "").is_some(),
            "{command}"
        );
    }
    for command in [
        "cat out.txt | grep -q e2e4",
        "test \"$(cat move.txt)\" = e2e4",
        "python3 check.py",
        "diff -u want.txt move.txt",
    ] {
        assert!(
            uncounted_pass_note(command, &none, "").is_none(),
            "{command}"
        );
    }
    let failed = crate::agent::run::outcome_gradient::failing_tests(
        "FAILED tests/test_spacecurve.py::test_reconstructed_space_curve - AssertionError\n",
    );
    for command in [
        "cd /app/x && python -m pytest tests -q --deselect tests/test_spacecurve.py::test_reconstructed_space_curve",
        "pytest -k 'not reconstructed_space_curve' tests",
        "pytest --deselect=tests/test_spacecurve.py::test_reconstructed_space_curve",
    ] {
        let note = uncounted_pass_note(command, &failed, "").expect(command);
        assert!(note.contains("reconstructed_space_curve"), "{note}");
    }
    let full = "python -m pytest tests -q --deselect tests/test_spacecurve.py::test_reconstructed_space_curve";
    // Excluding a test that never failed, or one the request says to skip, is fine.
    assert!(uncounted_pass_note(full, &none, "").is_none());
    assert!(uncounted_pass_note(full, &failed, "skip test_reconstructed_space_curve").is_none());
    assert!(uncounted_pass_note("python -m pytest tests -q", &failed, "").is_none());
}

#[test]
fn self_confirming_values_are_expected_numbers_only_this_run_printed() {
    let printed: std::collections::BTreeSet<String> =
        super::distinctive_numbers("frame 55 dx 0.873 area 1203\nlanded 61 (t2.py v1_2)")
            .into_iter()
            .map(|(_, value)| value)
            .collect();
    assert!(
        !printed.contains("2") && !printed.contains("1_2"),
        "{printed:?}"
    );
    let check = "python3 jump.py v.mp4 && grep -Eq '^takeoff = 55$' out.toml && grep -q 'land = 61' out.toml 2>&1";
    assert_eq!(
        self_confirming_values(check, &printed, "Find takeoff and landing frames."),
        Some(vec!["55".to_owned(), "61".to_owned()])
    );
    // A value the request states is an independent expectation.
    assert_eq!(
        self_confirming_values(check, &printed, "The example lands at frame 61."),
        None
    );
    // One expected value the run never printed is not a snapshot of its output.
    let mixed = "python3 -c 'assert f(3)==55 and g()==0.874'";
    assert_eq!(self_confirming_values(mixed, &printed, ""), None);
    assert_eq!(
        self_confirming_values("test $(wc -l < out) -eq 1203", &printed, ""),
        Some(vec!["1203".to_owned()])
    );
    // Options, ports and paths are inputs, not expectations.
    for check in [
        "python3 train.py --epochs=55 && pytest -q",
        "curl -sf localhost:1203/health",
        "cargo test",
    ] {
        assert_eq!(self_confirming_values(check, &printed, ""), None, "{check}");
    }
}

#[test]
fn membership_only_checks_are_flagged_for_exhaustive_requests() {
    let printed = std::collections::BTreeSet::new();
    let exhaustive = "If several labels match, print them all.";
    assert_eq!(
        self_confirming_values("grep -qx 'amber' out.txt", &printed, exhaustive),
        Some(vec!["amber".into()]),
    );
    assert_eq!(
        self_confirming_values(
            "grep -qx amber out.txt",
            &printed,
            "Check that amber exists."
        ),
        None,
    );
    assert_eq!(
        self_confirming_values("diff -u expected.txt out.txt", &printed, exhaustive),
        None,
    );
}

#[test]
fn masked_check_note_names_the_accepted_pipefail_shape() {
    let misplaced = "cd /app && set -o pipefail; python3 run.py | grep -c ok";
    assert!(exit_status_masked(misplaced));
    let note = masked_check_note(misplaced);
    assert!(note.contains("very first statement"), "{note}");
    assert!(note.contains("`cwd` argument"), "{note}");
    let plain = masked_check_note("python3 run.py | tail -5");
    assert!(plain.contains("Rerun without the pipe"), "{plain}");
    assert!(!plain.contains("very first statement"), "{plain}");
}

#[test]
fn pipefail_check_credit_uses_the_observed_exit_without_promoting_masked_zeroes() {
    use verify::verification::CheckStatus;
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    for (script, exit, expected) in [
        ("false | true # pipefail", 0, CheckStatus::Unverified),
        (
            "true | false # pipefail",
            1,
            CheckStatus::FailedUnknownCause,
        ),
        (
            "set -o pipefail; false | true",
            1,
            CheckStatus::FailedUnknownCause,
        ),
        (
            "set -o pipefail; printf 'Results: 12 30 8\\n' | tail -n 1 | grep -Eq '^Results: [0-9]+ [0-9]+ [0-9]+$' && echo ok",
            0,
            CheckStatus::Passed,
        ),
        (
            "set -o pipefail; false | cat | cat && echo ok",
            1,
            CheckStatus::FailedUnknownCause,
        ),
        ("set -o pipefail; true | true", 0, CheckStatus::Passed),
    ] {
        let check = verify::verification::FocusedCheck {
            name: "pipeline".into(),
            command: script.into(),
            timeout_secs: 5,
        };
        let output = agent.run_focused_check(&check, &mut NullUi);
        assert_eq!(
            output.process_outcome().unwrap().exit_code,
            Some(exit),
            "{script}"
        );
        assert_eq!(
            agent.step_checks.evidence.latest_status(script),
            Some(expected),
            "{script}"
        );
    }
}

#[test]
fn an_unlaunched_validation_helper_cannot_be_settled_by_a_write() {
    use crate::agent::verify::completion::Decision;
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    agent.hermetic = false;
    let call = ToolCall {
        id: "helper".into(),
        name: "bash".into(),
        malformed_arguments: false,
        arguments: json!({"command":"cat > /tmp/test_output.py <<'PY'\nprint('examples')\nPY\npython3 /tmp/test_output.py"}),
    };
    agent.note_verification(
        &call,
        &ToolOutput::err("command not found").with_process(completed_process(127)),
        &mut NullUi,
    );
    std::fs::write(root.path().join("result.txt"), "wrong").unwrap();
    agent.step_checks.mutations += 1;
    let decide = |agent: &mut Agent<'_>| {
        agent.completion_decision(
            TurnOutcome::Completed,
            crate::agent::verify::completion::ReplyChannel::Explicit,
            &mut NullUi,
        )
    };
    assert!(
        matches!(decide(&mut agent), Decision::Continue(_)),
        "write alone was accepted"
    );
    assert!(
        matches!(decide(&mut agent), Decision::Incomplete(_)),
        "unchecked completion was accepted on retry"
    );
    let acceptance =
        json!({"acceptance":{"name":"output value","command":"grep -qx correct result.txt"}});
    assert!(agent.register_acceptance(&acceptance, &mut NullUi).is_error);
    assert!(matches!(decide(&mut agent), Decision::Incomplete(_)));
    std::fs::write(root.path().join("result.txt"), "correct").unwrap();
    agent.step_checks.mutations += 1;
    let out = agent.register_acceptance(&acceptance, &mut NullUi);
    assert!(!out.is_error, "{}", out.text);
    assert!(matches!(
        decide(&mut agent),
        Decision::Accept(TurnOutcome::Completed)
    ));
}

#[test]
fn unrelated_eval_receipt_cannot_clear_a_validation_launch_gap() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    agent.hermetic = false;
    agent.step_checks.validation_launch_gap = true;
    agent.step_checks.mutations = 1;
    let state = tempfile::tempdir().unwrap();
    let checkpoints = Arc::new(
        Checkpoints::new(state.path().join("session"), agent.workspace.root().into()).unwrap(),
    );
    agent.step_checks.turn = Some(checkpoints.begin_turn("standalone").unwrap());
    let output = agent.workspace.root().join("out.csv");
    checkpoints.before_write(&output).unwrap();
    std::fs::write(&output, "a,b").unwrap();
    agent.checkpoints = Some(checkpoints);
    std::fs::write(root.path().join("reference.txt"), "ok").unwrap();
    let observe = |agent: &mut Agent<'_>, path: &str, passed| {
        let digest = crate::tools::exec::eval::check_source_digest(&agent.workspace, path).unwrap();
        agent.step_checks.eval_checks.observe(&json!({"code":"..."}), &ToolOutput::structured(json!({
            "checks":[{"runtime":"python","name":"file contents","passed":passed,"source":{"path":path,"sha256":digest}}]
        })).with_observation(crate::runtime::execution::ObservationKind::Wait, true));
    };
    agent.step_checks.eval_checks.observe(
        &json!({"code":"..."}),
        &ToolOutput::structured(json!({
            "checks":[{"runtime":"python","name":"examples pass","passed":true}]
        }))
        .with_observation(crate::runtime::execution::ObservationKind::Wait, true),
    );
    let note = agent
        .unbound_eval_check_note()
        .expect("a passing two-argument check does not bind the edited file");
    assert!(
        note.contains("out.csv") && note.contains("dial.read"),
        "{note}"
    );
    observe(&mut agent, "reference.txt", true);
    assert!(
        agent.artifact_check_reminder().is_some(),
        "an unrelated input is not the saved deliverable"
    );
    observe(&mut agent, "out.csv", true);
    assert!(agent.artifact_check_reminder().is_none());
    assert!(agent.unbound_eval_check_note().is_none());
    std::fs::write(&output, "x,y").unwrap();
    assert!(
        agent.artifact_check_reminder().is_some(),
        "same-size rewrite stales the receipt"
    );
    agent.step_checks.validation_launch_gap = false;
    assert!(
        agent.artifact_check_reminder().is_some(),
        "creation alone does not check a standalone output"
    );
    observe(&mut agent, "out.csv", false);
    assert!(agent.artifact_check_reminder().is_some());
    observe(&mut agent, "out.csv", true);
    assert!(agent.artifact_check_reminder().is_none());
}

#[test]
fn missing_optional_programs_and_printed_status_are_not_validation_obligations() {
    let mut call = ToolCall {
        id: "probe".into(),
        name: "bash".into(),
        arguments: json!({}),
        malformed_arguments: false,
    };
    let missing = ToolOutput::err("not found").with_process(completed_process(127));
    for command in [
        "python3 --version",
        "echo test_output.py",
        "node tools/generate.js",
    ] {
        call.arguments = json!({"command":command});
        assert!(!validation_launch_gap(&call, &missing), "{command}");
    }
    call.arguments = json!({"command":"node './format.test.js'"});
    assert!(validation_launch_gap(&call, &missing));
    assert!(!validation_launch_gap(
        &call,
        &ToolOutput::err("[exit code: 127]")
    ));
    call.arguments = json!({"command":"project-validator", "check":true});
    assert!(validation_launch_gap(&call, &missing));
    let mut incomplete = completed_process(127);
    incomplete.output_complete = false;
    assert!(!validation_launch_gap(
        &call,
        &ToolOutput::err("partial").with_process(incomplete)
    ));
}

#[test]
fn local_repair_brief_tracks_checked_revision_and_drops_superseded_evidence() {
    use crate::agent::run::tool_exposure::default_offer_tests::with_default_agent;
    let command = "cargo test score";
    let call = ToolCall {
        id: "score-run".into(),
        name: "bash".into(),
        arguments: json!({"command":command}),
        malformed_arguments: false,
    };
    let failed = ToolOutput::err("score.rs:19\nAssertionError: expected 5, got 4")
        .with_process(completed_process(1));
    with_default_agent("", |agent| {
        // This run checks our edit, not an unchanged pre-task failure.
        agent.step_checks.mutations += 1;
        agent.note_verification(&call, &failed, &mut NullUi);
        agent.step_checks.mutations += 1;
        let note = agent.local_repair_note(Some("Fix the score")).unwrap();
        let text = note["content"].as_str().unwrap();
        for fact in [
            "score.rs:19",
            "expected 5, got 4",
            "Fix the score",
            "\"checkpointed_edit_since_check\":true",
        ] {
            assert!(text.contains(fact), "missing {fact}");
        }
        for superseding in [
            ToolOutput::err("denied").with_status(crate::tools::ToolStatus::Blocked),
            ToolOutput::ok("OK".into()).with_process(completed_process(0)),
        ] {
            agent.note_verification(&call, &failed, &mut NullUi);
            assert!(agent.local_repair_note(None).is_some());
            agent.note_verification(&call, &superseding, &mut NullUi);
            assert!(agent.local_repair_note(None).is_none());
        }
    });
    with_default_agent("[assistance.local]\nenabled=false", |agent| {
        agent.note_verification(&call, &failed, &mut NullUi);
        assert!(agent.local_repair_note(None).is_none());
    });
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut hermetic = hermetic_agent(&config, root.path());
    hermetic.note_verification(&call, &failed, &mut NullUi);
    assert!(hermetic.local_repair_note(None).is_none());
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
    #[derive(Default)]
    struct PassCounter(Vec<usize>);
    impl Ui for PassCounter {
        fn check_passed_with_delivery(&mut self, open_count: &dyn Fn() -> usize) {
            self.0.push(open_count());
        }
    }
    agent
        .session_mut()
        .set_control_state(json!({"working_state":{"deliverables":[
            {"id":"docs","status":"open"}, {"id":"code","status":"satisfied"}
        ]}}))
        .unwrap();
    let mut ui = PassCounter::default();
    agent.note_verification(&call, &prose, &mut ui);
    assert!(ui.0.is_empty());
    assert_eq!(agent.verifications, vec![false]);
    assert_eq!(agent.behavioral_checks, vec![false]);

    let observed = prose.with_process(completed_process(0));
    agent.note_verification(&call, &observed, &mut ui);
    assert_eq!(ui.0, [1]);
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
    assert_eq!(run.failures.len(), 1, "{:?}", run.failures);
    assert!(
        run.failures[0].contains("Configured verification runner unavailable"),
        "{:?}",
        run.failures
    );
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
    assert_eq!(evidence["outstanding_tasks"].as_array().unwrap().len(), 2);
    assert_eq!(evidence["outstanding_tasks"][0]["content"], "current");
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
fn failed_contract_capture_is_unverified_in_gate_and_verify_tool() {
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
        let output = agent.verify_tool(&json!({"targets":["contract"]}), &mut NullUi);
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
fn failed_check_blocks_completion_until_rerun_when_no_automatic_pass_will_recheck_it() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    assert!(!config.verification.automatic_checks);
    let mut agent = hermetic_agent(&config, root.path());
    let failing = verify::verification::FocusedCheck {
        name: "acceptance: \"correctness\"".into(),
        command: "grep -q fixed state || { echo assertion failed; exit 1; }".into(),
        timeout_secs: 5,
    };
    let weaker = verify::verification::FocusedCheck {
        name: "acceptance: structure".into(),
        command: "true".into(),
        timeout_secs: 5,
    };
    agent.step_checks.mutations += 1;
    agent.run_checks(std::slice::from_ref(&failing), &mut NullUi);
    // A later effect and a passing check under another name do not discharge the failure.
    agent.step_checks.mutations += 1;
    agent.run_checks(std::slice::from_ref(&weaker), &mut NullUi);
    agent.hermetic = false;
    let refusal = agent.completion_blockers().unwrap_err();
    assert!(refusal.contains("focused checks failed"), "{refusal}");
    assert!(refusal.contains(&failing.name), "{refusal}");
    let recovery = refusal
        .lines()
        .find_map(|line| line.strip_prefix("Recovery call: verify "))
        .expect("completion refusal must provide an executable recovery call");
    let args: Value = serde_json::from_str(recovery).unwrap();
    assert_eq!(args, json!({"targets": [failing.name]}));
    std::fs::write(root.path().join("state"), "fixed").unwrap();
    let output = agent.verify_tool(&args, &mut NullUi);
    assert!(!output.is_error, "{}", output.text);
    assert!(agent.step_checks.failed_checks.is_empty());
    assert!(agent.completion_blockers().is_ok());
}

#[test]
fn completion_recovery_targets_respect_the_verify_limit() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    let limit = crate::tools::edit::editing::MAX_VERIFY_TARGETS;
    agent.step_checks.failed_checks = (0..=limit)
        .map(|index| verify::verification::FocusedCheck {
            name: format!("check {index}"),
            command: format!("echo {index}; exit 1"),
            timeout_secs: 5,
        })
        .collect();
    agent.step_checks.failed = true;
    agent.hermetic = false;
    let refusal = agent.completion_blockers().unwrap_err();
    let recovery = refusal
        .lines()
        .find_map(|line| line.strip_prefix("Recovery call: verify "))
        .unwrap();
    let args: Value = serde_json::from_str(recovery).unwrap();
    assert_eq!(args["targets"].as_array().unwrap().len(), limit);
    assert_eq!(args["targets"][0], "check 0");
    assert_eq!(args["targets"][limit - 1], format!("check {}", limit - 1));
    assert_eq!(agent.step_checks.failed_checks.len(), limit + 1);
    assert!(refusal.contains("additional failures may remain"));
}

#[test]
fn automatic_pass_still_defers_a_failure_followed_by_new_effects() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.verification.automatic_checks = true;
    let mut agent = hermetic_agent(&config, root.path());
    let failing = verify::verification::FocusedCheck {
        name: "unit".into(),
        command: "echo assertion failed; exit 1".into(),
        timeout_secs: 5,
    };
    agent.run_checks(std::slice::from_ref(&failing), &mut NullUi);
    agent.hermetic = false;
    assert!(agent.completion_blockers().is_err());
    agent.step_checks.mutations += 1;
    assert!(agent.completion_blockers().is_ok());
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
fn foreground_inspections_overlap_but_an_opaque_check_is_a_barrier() {
    use crate::agent::safety::approval::{Effect, PermissionRule};
    use verify::verification::CheckStatus;
    #[derive(Default)]
    struct Rows {
        open: usize,
        peak: usize,
        order: Vec<String>,
    }
    impl Ui for Rows {
        fn tool_call_with_args(&mut self, _: &str, _: &str, _: &str, args: &Value) {
            if args["command"] == "touch marker" {
                assert_eq!(self.open, 0);
            }
            self.open += 1;
            self.peak = self.peak.max(self.open);
        }
        fn tool_result_with_id(&mut self, id: &str, _: &str, _: &ToolOutput) {
            self.open -= 1;
            self.order.push(id.to_owned());
        }
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("input"), "source").unwrap();
    let (mut config, _) = input_contract_config("cat input");
    config.task.max_concurrency = 3;
    for (name, cmd) in [
        ("size", "wc -c input"),
        ("lines", "tail input"),
        ("writer", "touch marker"),
        ("last", "head input"),
    ] {
        config.verification.commands.push(command(name, cmd));
        let contract = config.verification.inputs["contract"].clone();
        config.verification.inputs.insert(name.into(), contract);
    }
    let mut agent = hermetic_agent(&config, root.path());
    agent.hermetic = false;
    agent.approval = crate::config::types::Approval::Auto;
    agent.always.push(PermissionRule {
        effect: Effect::Deny,
        tool: "bash".into(),
        command_prefix: vec!["wc".into()],
        paths: vec![],
    });
    let checks = agent.current_focused_checks();
    let mut rows = Rows::default();
    let _ = agent.run_checks(&checks, &mut rows);
    assert_eq!((rows.open, rows.peak), (0, 3));
    assert_eq!(
        rows.order,
        checks
            .iter()
            .map(|c| format!("focused-check:{}", c.name))
            .collect::<Vec<_>>()
    );
    assert!(root.path().join("marker").is_file());
    for check in checks {
        let expected = if check.name == "size" {
            CheckStatus::PermissionDenied
        } else {
            CheckStatus::Passed
        };
        assert_eq!(
            agent.step_checks.evidence.latest_status(&check.command),
            Some(expected)
        );
    }
}

#[test]
fn background_check_slots_run_configured_checks_side_by_side() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("input"), "before").unwrap();
    // Each check only finishes once the other has started, so a serial run would time out.
    let first = "touch a; i=0; while test ! -f b; do i=$((i+1)); test $i -gt 500 && exit 1; sleep 0.01; done; exit 0";
    let second = "touch b; i=0; while test ! -f a; do i=$((i+1)); test $i -gt 500 && exit 1; sleep 0.01; done; exit 0";
    let (mut config, _) = input_contract_config(first);
    config
        .verification
        .commands
        .push(crate::config::types::VerificationCommand {
            name: "contract2".into(),
            command: second.into(),
            file_extensions: vec![],
            timeout_secs: 15,
        });
    let inputs = config.verification.inputs["contract"].clone();
    config
        .verification
        .inputs
        .insert("contract2".into(), inputs);
    config.experimental.background_checks = true;
    config.experimental.background_check_slots = 2;
    config.verification.automatic_checks = true;
    config.experimental.fault_witness = false;
    let mut agent = hermetic_agent(&config, root.path());
    agent.hermetic = false;
    agent.approval = crate::config::types::Approval::Auto;
    agent.start_background_check(&mut NullUi);
    assert_eq!(
        agent
            .pending_check
            .as_ref()
            .map(|pending| pending.runs.len()),
        Some(2),
        "both configured checks start together"
    );
    agent.drain_background_check(&mut NullUi, true);
    assert!(agent.pending_check.is_none());
    for command in [first, second] {
        assert_eq!(
            agent.step_checks.evidence.latest_status(command),
            Some(verify::verification::CheckStatus::Passed),
            "{command}"
        );
    }
    let before = agent.efficiency_metrics.backend_operations.borrow()["check_input_capture"].calls;
    agent.start_background_check(&mut NullUi);
    assert!(
        agent.pending_check.is_none(),
        "unchanged passes were reused"
    );
    assert_eq!(
        agent.efficiency_metrics.backend_operations.borrow()["check_input_capture"].calls - before,
        2,
        "one fresh snapshot per reuse decision"
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

#[test]
fn non_code_failures_are_disclosed_unless_the_check_passed_before_the_first_effect() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    let program = "dial_test_nonexistent_verification_runner_43";
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
    let check = crate::agent::verify::verification::FocusedCheck {
        name: "custom runner".into(),
        command: program.into(),
        timeout_secs: 30,
    };
    let mut agent = hermetic_agent(&config, root.path());
    let run = agent.run_checks(std::slice::from_ref(&check), &mut NullUi);
    assert_eq!(run.failures.len(), 1, "{:?}", run.failures);
    assert_eq!(
        agent.disclosed_failure(&check, false),
        Some("verification environment unavailable")
    );
    let gaps = agent.disclosed_gaps();
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert!(gaps[0].contains("custom runner"), "{gaps:?}");

    // The same check passed before this instruction's first effect: the turn may have broken it.
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.evidence.observe(
        program,
        "focused-check:custom runner",
        crate::agent::verify::verification::CheckStatus::Passed,
        agent.step_checks.revision(),
        false,
    );
    agent.step_checks.evidence.capture_baseline_of(program);
    agent.step_checks.mutations = 1;
    agent.run_checks(std::slice::from_ref(&check), &mut NullUi);
    assert_eq!(agent.disclosed_failure(&check, false), None);
    assert!(agent.disclosed_gaps().is_empty());
}

#[test]
fn a_check_run_before_the_first_effect_becomes_its_lazy_baseline() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
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
    let mut agent = hermetic_agent(&config, root.path());
    assert!(!config.verification.automatic_checks);
    let first = agent.run_checks(std::slice::from_ref(&check), &mut NullUi);
    // A run is never its own baseline: the first failure stays actionable.
    assert_eq!(first.actionable.len(), 1);
    assert_eq!(agent.disclosed_failure(&check, false), None);
    // After an edit the identical failure predates the turn and is disclosed, not refused.
    agent.step_checks.mutations = 1;
    agent.run_checks(std::slice::from_ref(&check), &mut NullUi);
    assert_eq!(
        agent.step_checks.evidence.baseline_status(&check.command),
        Some(crate::agent::verify::verification::CheckStatus::CodeFailed)
    );
    assert_eq!(
        agent.disclosed_failure(&check, false),
        Some("unchanged pre-existing failure outside the requested scope")
    );
    // A debugging request is about that failure, so it stays a blocker.
    agent.task_policy.kind = crate::learn::task_policy::Kind::Debugging;
    assert_eq!(agent.disclosed_failure(&check, false), None);
    agent.task_policy.kind = crate::learn::task_policy::Kind::General;
    // A changed diagnostic after the edit is this turn's failure; later runs never move the baseline.
    std::fs::write(
        root.path().join("diagnostic"),
        "AssertionError: new_test expected 1 got 3\n",
    )
    .unwrap();
    agent.run_checks(std::slice::from_ref(&check), &mut NullUi);
    assert_eq!(agent.disclosed_failure(&check, false), None);
    // Asked only to diagnose, with nothing edited, any current failure is the answer.
    let mut agent = hermetic_agent(&config, root.path());
    agent.run_checks(std::slice::from_ref(&check), &mut NullUi);
    assert_eq!(
        agent.disclosed_failure(&check, true),
        Some("the failure this request asked to diagnose; nothing was edited")
    );
}

/// Dial's managed interpreter lives in protected Dial storage. When the shell sandbox hides that
/// storage, the automatic Python checks must use a launchable interpreter instead: otherwise every
/// Python edit under `--sandbox dev` reports a failed check the agent can never clear.
#[test]
fn focused_python_checks_avoid_an_interpreter_hidden_by_the_shell_sandbox() {
    let root = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let python = private.path().join("python/.venv/bin/python");
    std::fs::create_dir_all(python.parent().unwrap()).unwrap();
    std::fs::write(&python, "").unwrap();
    let config = Config {
        python: python.clone(),
        ..Config::default()
    };
    let mut agent = hermetic_agent(&config, root.path());
    agent.workspace =
        agent
            .workspace
            .clone()
            .with_policy(crate::tools::exec::sandbox::Policy::Dev {
                deny_read: Vec::new(),
                extra_write: Vec::new(),
                allow_network: false,
                serial_ports: false,
            });
    assert_eq!(agent.check_python(), python, "visible interpreter is kept");
    agent.workspace = agent
        .workspace
        .clone()
        .with_protected_paths(&[private.path().to_path_buf()]);
    assert_eq!(agent.check_python(), PathBuf::from("python3"));
    agent.workspace = agent.workspace.clone().without_sandbox();
    assert_eq!(
        agent.check_python(),
        python,
        "unsandboxed commands can run it"
    );
}

#[test]
fn closing_a_reminded_task_still_runs_its_unobserved_check() {
    use crate::agent::verify::completion::{Decision, ReplyChannel};
    use crate::agent::verify::verification::CheckStatus;
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.verification.enabled = true;
    config.verification.automatic_checks = false;
    let check = "test -f result.txt";
    config.verification.commands = vec![command("result exists", check)];
    let mut agent = hermetic_agent(&config, root.path());
    agent.hermetic = false;
    agent.approval = crate::config::types::Approval::Auto;
    let decide = |agent: &mut Agent<'_>| {
        agent.completion_decision(TurnOutcome::Completed, ReplyChannel::Explicit, &mut NullUi)
    };
    assert!(
        !agent
            .todo
            .call(&json!({"op":"set","tasks":[{"content":"Write result"}]}))
            .is_error
    );
    assert!(matches!(decide(&mut agent), Decision::Continue(_)));
    std::fs::write(root.path().join("result.txt"), "ready").unwrap();
    agent.step_checks.mutations += 1;
    assert!(!agent.todo.call(&json!({"op":"done","index":1})).is_error);
    let decision = decide(&mut agent);
    assert_eq!(
        agent.step_checks.evidence.latest_status(check),
        Some(CheckStatus::Passed)
    );
    assert!(matches!(decision, Decision::Accept(TurnOutcome::Completed)));
}

#[test]
fn one_current_pass_does_not_discharge_other_stale_or_unobserved_checks() {
    use crate::agent::verify::verification::CheckStatus;
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.verification.enabled = true;
    config.verification.automatic_checks = false;
    config.verification.commands = vec![
        command("first", "echo first"),
        command("second", "echo second"),
    ];
    let mut agent = hermetic_agent(&config, root.path());
    agent.hermetic = false;
    observe_at(&mut agent, "echo second", CheckStatus::Passed);
    agent.step_checks.mutations += 1;
    agent.step_checks.shell_check_passed = Some(agent.step_checks.revision());
    observe_at(&mut agent, "echo first", CheckStatus::Passed);
    assert_eq!(
        agent.current_gate_outcome().unverified,
        Some(vec!["second".into()]),
        "neither a current first check nor an unrelated shell pass covers the stale second check"
    );
    agent.step_checks.evidence = Default::default();
    observe_at(&mut agent, "echo first", CheckStatus::Passed);
    assert_eq!(
        agent.current_gate_outcome().unverified,
        Some(vec!["second".into()]),
        "neither a current first check nor an unrelated shell pass covers the unobserved second check"
    );
    observe_at(&mut agent, "echo second", CheckStatus::Passed);
    assert!(agent.current_gate_outcome().unverified.is_none());

    observe_at(&mut agent, "echo second", CheckStatus::CodeFailed);
    let second = crate::agent::verify::verification::FocusedCheck {
        name: "second".into(),
        command: "echo second".into(),
        timeout_secs: 5,
    };
    agent.step_checks.failed_checks.push(second);
    let failing = agent.gate_unverified_edits(false, &mut NullUi);
    assert!(
        failing.blocking,
        "an unrelated shell pass erased a retained failure"
    );
    assert_eq!(failing.unverified, Some(vec!["second".into()]));

    observe_at(
        &mut agent,
        "echo second",
        CheckStatus::EnvironmentUnavailable,
    );
    let disclosed = agent.gate_unverified_edits(false, &mut NullUi);
    assert!(!disclosed.blocking);
    assert_eq!(
        disclosed.unverified, None,
        "disclosed failures are not outstanding checks"
    );
}

#[test]
fn identical_clipped_presentations_cannot_hide_a_changed_baseline_failure() {
    use crate::runtime::execution::Completeness;
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config
        .verification
        .commands
        .push(command("unit", "cargo test unit"));
    let mut agent = hermetic_agent(&config, root.path());
    let store = crate::storage::artifact::ArtifactStore::new(state.path()).unwrap();
    agent.workspace = agent.workspace.clone().with_artifacts(store.clone());
    let call = ToolCall {
        id: "focused-check:unit".into(),
        name: "bash".into(),
        arguments: json!({"command":"cargo test unit"}),
        malformed_arguments: false,
    };
    let output = |diagnostic: &str| {
        let (uri, mut integrity) = store.retain_with_integrity(diagnostic).unwrap();
        integrity.capture = Completeness::Complete;
        integrity.presentation = Completeness::Partial;
        integrity.artifact = Some(uri);
        ToolOutput::err("failed\n[... clipped ...]")
            .with_process(completed_process(1))
            .with_output_integrity(integrity)
    };
    let before = output("AssertionError: old failure\n");
    agent.note_verification(&call, &before, &mut NullUi);
    agent.step_checks.mutations += 1;
    agent.note_verification(&call, &before, &mut NullUi);
    let check = crate::agent::verify::verification::FocusedCheck {
        name: "unit".into(),
        command: "cargo test unit".into(),
        timeout_secs: 5,
    };
    assert_eq!(
        agent.disclosed_failure(&check, false),
        Some("unchanged pre-existing failure outside the requested scope")
    );
    agent.note_verification(
        &call,
        &output("AssertionError: new regression\n"),
        &mut NullUi,
    );
    assert!(
        !agent
            .step_checks
            .evidence
            .unchanged_unrelated_failure("cargo test unit")
    );
    assert_eq!(agent.disclosed_failure(&check, false), None);
}

#[test]
fn classify_check_source_distinguishes_explicit_receipts_from_heuristic_shell_commands() {
    let configured = vec![crate::config::types::VerificationCommand {
        name: "lint".into(),
        command: "custom-lint --strict".into(),
        file_extensions: vec![],
        timeout_secs: 0,
    }];
    let focused = ToolCall {
        id: "focused-check:unit".into(),
        name: "bash".into(),
        arguments: json!({"command": "echo ok"}),
        malformed_arguments: false,
    };
    let explicit = ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        arguments: json!({"command": "my-validator", "check": true}),
        malformed_arguments: false,
    };
    let configured_call = ToolCall {
        id: "c2".into(),
        name: "bash".into(),
        arguments: json!({"command": "custom-lint --strict"}),
        malformed_arguments: false,
    };
    let heuristic = ToolCall {
        id: "c3".into(),
        name: "bash".into(),
        arguments: json!({"command": "cargo test"}),
        malformed_arguments: false,
    };
    let plain = ToolCall {
        id: "c4".into(),
        name: "bash".into(),
        arguments: json!({"command": "ls -la"}),
        malformed_arguments: false,
    };

    let f = classify_check_source(&focused, &configured).unwrap();
    assert_eq!(f, CheckReceiptSource::FocusedCheck);
    assert!(f.is_explicit_receipt());

    let e = classify_check_source(&explicit, &configured).unwrap();
    assert_eq!(e, CheckReceiptSource::ExplicitBashCheck);
    assert!(e.is_explicit_receipt());

    let c = classify_check_source(&configured_call, &configured).unwrap();
    assert_eq!(c, CheckReceiptSource::ConfiguredCommand);
    assert!(c.is_explicit_receipt());

    let h = classify_check_source(&heuristic, &configured).unwrap();
    assert_eq!(h, CheckReceiptSource::HeuristicShellCommand);
    assert!(!h.is_explicit_receipt());

    assert!(classify_check_source(&plain, &configured).is_none());
}

#[test]
fn a_deliverable_that_failed_to_parse_blocks_completion_until_that_consumer_passes() {
    use crate::agent::verify::failed_runs::Run;
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    agent.hermetic = false;
    std::fs::write(
        root.path().join("main.py.c"),
        "#if 0\nprint(1)\n#endif\nint main(){}\n",
    )
    .unwrap();
    let changed = vec!["main.py.c".to_owned()];
    let traceback = "  File \"main.py.c\", line 4\nSyntaxError: invalid syntax\n";
    let run = |failed, output| Run {
        tool: "eval",
        source: "exec(open('main.py.c').read())",
        language: Some("py"),
        failed,
        output,
    };
    agent
        .step_checks
        .failed_runs
        .observe(root.path(), &changed, &run(true, traceback));
    let refusal = agent.completion_blockers().unwrap_err();
    assert!(
        refusal.contains("main.py.c") && refusal.contains("SyntaxError"),
        "{refusal}"
    );
    agent
        .step_checks
        .failed_runs
        .observe(root.path(), &changed, &run(false, "1\n"));
    assert!(agent.completion_blockers().is_ok());
}

#[test]
fn unattended_eval_evidence_must_be_bound_to_a_file_this_turn_changed() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    agent.hermetic = false;
    agent.modes.launch.unattended = true;
    agent.step_checks.mutations = 1;
    let state = tempfile::tempdir().unwrap();
    let checkpoints = Arc::new(
        Checkpoints::new(state.path().join("session"), agent.workspace.root().into()).unwrap(),
    );
    agent.step_checks.turn = Some(checkpoints.begin_turn("unattended").unwrap());
    let output = agent.workspace.root().join("solve.py");
    checkpoints.before_write(&output).unwrap();
    std::fs::write(&output, "print(1)\n").unwrap();
    agent.checkpoints = Some(checkpoints);
    std::fs::write(root.path().join("input.txt"), "data").unwrap();
    let observe = |agent: &mut Agent<'_>, path: &str| {
        let digest = crate::tools::exec::eval::check_source_digest(&agent.workspace, path).unwrap();
        agent.step_checks.eval_checks.observe(&json!({"code":"..."}), &ToolOutput::structured(json!({
            "checks":[{"runtime":"python","name":"bound","passed":true,"source":{"path":path,"sha256":digest}}]
        })).with_observation(crate::runtime::execution::ObservationKind::Wait, true));
    };
    assert!(agent.unattended_verification_gap().is_some());
    observe(&mut agent, "input.txt");
    assert!(
        agent.unattended_verification_gap().is_some(),
        "a passing assertion about an unchanged input is not evidence for the delivered file"
    );
    observe(&mut agent, "solve.py");
    assert!(agent.unattended_verification_gap().is_none());
}
