use super::*;

fn version(path: &str, before: Option<&str>, after: Option<&str>) -> TurnVersion {
    TurnVersion {
        path: path.into(),
        in_project: true,
        before: before.map(|text| text.as_bytes().to_vec()),
        after: after.map(|text| text.as_bytes().to_vec()),
    }
}

fn agent<'a>(config: &'a Config, root: &Path) -> Agent<'a> {
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
fn changed_since_uses_the_snapshot_or_the_turn_start_content() {
    let versions = [
        version("a.rs", Some("a0"), Some("a2")),
        version("b.rs", Some("b0"), Some("b1")),
        version("new.rs", None, Some("n")),
    ];
    let passing = Overrides::from([
        ("a.rs".to_owned(), Some(b"a1".to_vec())),
        ("b.rs".to_owned(), Some(b"b1".to_vec())),
    ]);
    // b.rs is unchanged since the pass; new.rs did not exist at the pass (not in the snapshot).
    assert_eq!(
        changed_since(&versions, &passing),
        Overrides::from([
            ("a.rs".to_owned(), Some(b"a1".to_vec())),
            ("new.rs".to_owned(), None),
        ])
    );
    let halves: Vec<String> = ["a", "b", "c"].map(str::to_owned).to_vec();
    assert_eq!(next_half(&halves), Some(&halves[..1]));
    assert_eq!(next_half(&halves[..1]), None);
}

#[test]
fn install_writes_and_removes_files_but_refuses_escapes() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("gone.rs"), "x").unwrap();
    install(
        root.path(),
        &Overrides::from([
            ("src/kept.rs".to_owned(), Some(b"k".to_vec())),
            ("gone.rs".to_owned(), None),
            ("never.rs".to_owned(), None),
        ]),
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("src/kept.rs")).unwrap(),
        "k"
    );
    assert!(!root.path().join("gone.rs").exists());
    assert!(
        install(
            root.path(),
            &Overrides::from([("../escape".to_owned(), None)])
        )
        .is_err()
    );
}

#[test]
fn only_explicitly_declared_behavioral_commands_are_witnessed() {
    let mut config = Config::default();
    config.experimental.fault_witness = true;
    config
        .verification
        .commands
        .push(crate::config::types::VerificationCommand {
            name: "regression".into(),
            command: "make regression".into(),
            file_extensions: vec![],
            timeout_secs: 30,
        });
    let check = |name: &str, command: &str| FocusedCheck {
        name: name.into(),
        command: command.into(),
        timeout_secs: 30,
    };
    let selected = check("regression", "make regression");
    assert!(
        !behavioral(&config, &selected),
        "configuration alone is not selection"
    );
    config
        .verification
        .behavioral_checks
        .push("regression".into());
    assert!(behavioral(&config, &selected));
    for unselected in [
        check("related Rust tests: x", "cargo test x"),
        check("different name", "make regression"),
        check("regression", "make test"),
        check("python AST: a.py", "python -c ..."),
        check("cargo check: .", "cargo check"),
    ] {
        assert!(!behavioral(&config, &unselected), "{}", unselected.name);
    }
}

#[test]
fn witness_and_bisection_run_in_copies_and_leave_the_workspace_alone() {
    let root = tempfile::tempdir().unwrap();
    // The check passes only when `fixed` says yes and `other` is untouched.
    std::fs::write(root.path().join("fixed"), "yes").unwrap();
    std::fs::write(root.path().join("other"), "ok").unwrap();
    let mut config = Config::default();
    config.experimental.fault_witness = true;
    config.experimental.edit_bisection_runs = 4;
    let check = FocusedCheck {
        name: "unit".into(),
        command: "(grep -qx yes fixed && grep -qx ok other) || { echo 'AssertionError: expected fixed=yes and other=ok'; exit 1; }".into(),
        timeout_secs: 30,
    };
    config
        .verification
        .behavioral_checks
        .push(check.name.clone());
    config
        .verification
        .commands
        .push(crate::config::types::VerificationCommand {
            name: check.name.clone(),
            command: check.command.clone(),
            file_extensions: vec![],
            timeout_secs: check.timeout_secs,
        });
    let mut agent = agent(&config, root.path());
    let versions = [version("fixed", Some("no"), Some("yes"))];
    let line = agent.fault_witness(&check, &versions, &mut NullUi).unwrap();
    assert!(
        line.contains("fails with this turn's edits reverted"),
        "{line}"
    );
    assert_eq!(
        agent.step_checks.counterfactual.witnesses["unit"]["verdict"],
        "fails_without_turn_changes"
    );
    assert!(
        agent
            .fault_witness(&check, &versions, &mut NullUi)
            .is_none(),
        "once per turn, not once per revision"
    );
    agent.step_checks.mutations += 1;
    assert!(
        agent
            .fault_witness(&check, &versions, &mut NullUi)
            .is_none(),
        "later edits must not reset the witness budget"
    );

    // Regression: `other` broke it, `noise` did not; bisection names `other`.
    std::fs::write(root.path().join("other"), "broken").unwrap();
    std::fs::write(root.path().join("noise"), "changed").unwrap();
    let changed = Overrides::from([
        ("noise".to_owned(), None),
        ("other".to_owned(), Some(b"ok".to_vec())),
    ]);
    let line = agent.bisect(&check, changed, 4, &mut NullUi).unwrap();
    assert!(line.contains("reverting only other"), "{line}");
    let report = &agent.step_checks.counterfactual.bisections["unit"];
    assert_eq!(report["culprit"], "other");
    assert_eq!(report["runs"], 2);
    assert_eq!(
        std::fs::read_to_string(root.path().join("other")).unwrap(),
        "broken"
    );
    assert!(root.path().join("noise").exists());
    assert!(!agent.step_checks.counterfactual.evidence().is_null());
}

#[derive(Default)]
struct Notices(Vec<String>);

impl Ui for Notices {
    fn notice(&mut self, text: &str) {
        self.0.push(text.into());
    }
}

#[test]
fn a_passing_counterfactual_is_diagnostic_not_a_failed_verification() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.experimental.fault_witness = true;
    let check = FocusedCheck {
        name: "regression".into(),
        command: "printf 'test result: ok. 1 passed'".into(),
        timeout_secs: 5,
    };
    config
        .verification
        .commands
        .push(crate::config::types::VerificationCommand {
            name: check.name.clone(),
            command: check.command.clone(),
            file_extensions: vec![],
            timeout_secs: check.timeout_secs,
        });
    config
        .verification
        .behavioral_checks
        .push(check.name.clone());
    let mut agent = agent(&config, root.path());
    let revision = agent.step_checks.revision();
    agent.step_checks.evidence.observe(
        &check.command,
        "focused-check:regression",
        CheckStatus::Passed,
        revision,
        true,
    );
    let mut ui = Notices::default();
    let line = agent
        .fault_witness(
            &check,
            &[version("fixed", Some("before"), Some("after"))],
            &mut ui,
        )
        .unwrap();
    assert!(
        ui.0.is_empty(),
        "witness progress must not emit NOTICE cards: {:?}",
        ui.0
    );
    assert!(
        !line.contains("add or run"),
        "a passing baseline must not demand more checks: {line}"
    );
    assert_eq!(
        agent
            .step_checks
            .evidence
            .current_status(&check.command, revision),
        "passed"
    );
    assert_eq!(agent.step_checks.evidence.passing_behavioral(revision), 1);
    assert_eq!(
        agent.step_checks.counterfactual.witnesses["regression"]["verdict"],
        "passes_without_turn_changes"
    );
}

#[test]
fn witness_budget_includes_other_checks_and_inconclusive_attempts() {
    for (timeout, command, verdict, deadline) in [
        (
            0,
            "printf 'test result: ok. 1 passed'",
            "passes_without_turn_changes",
            60,
        ),
        (
            600,
            "echo 'No module named pytest'; exit 1",
            "inconclusive",
            60,
        ),
        (
            5,
            "printf 'test result: ok. 1 passed'",
            "passes_without_turn_changes",
            5,
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.experimental.fault_witness = true;
        let checks = ["first", "second"].map(|name| FocusedCheck {
            name: name.into(),
            command: command.into(),
            timeout_secs: timeout,
        });
        for check in &checks {
            config
                .verification
                .behavioral_checks
                .push(check.name.clone());
            config
                .verification
                .commands
                .push(crate::config::types::VerificationCommand {
                    name: check.name.clone(),
                    command: check.command.clone(),
                    file_extensions: vec![],
                    timeout_secs: timeout,
                });
        }
        let versions = [version("fixed", Some("before"), Some("after"))];
        let mut disabled = config.clone();
        disabled.experimental.fault_witness = false;
        assert!(
            agent(&disabled, root.path())
                .fault_witness(&checks[0], &versions, &mut NullUi)
                .is_none()
        );
        let mut agent = agent(&config, root.path());
        assert!(
            agent
                .fault_witness(&checks[0], &versions, &mut NullUi)
                .is_some()
        );
        let evidence = agent.step_checks.counterfactual.evidence();
        assert_eq!(evidence["fault_witness"]["first"]["verdict"], verdict);
        assert_eq!(evidence["fault_witness"]["first"]["timeout_secs"], deadline);
        assert_eq!(
            checks[0].timeout_secs, timeout,
            "normal check deadlines must not change"
        );
        assert!(
            agent
                .fault_witness(&checks[1], &versions, &mut NullUi)
                .is_none(),
            "another eligible command must not get its own budget"
        );
        assert_eq!(agent.step_checks.counterfactual.evidence(), evidence);
        agent.step_checks = StepChecks::default();
        assert!(
            agent
                .fault_witness(&checks[1], &versions, &mut NullUi)
                .is_some(),
            "a new user turn gets a fresh budget"
        );
    }
}

#[test]
fn test_strength_reports_surviving_mutants_once_per_turn_without_touching_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let source = "limit = 5\nflag = True\n";
    std::fs::write(root.path().join("val.py"), source).unwrap();
    let mut config = Config::default();
    config.experimental.mutation_runs = 4;
    let check = FocusedCheck {
        name: "unit".into(),
        command: "grep -qx 'limit = 5' val.py || { echo 'AssertionError: limit changed'; exit 1; }"
            .into(),
        timeout_secs: 30,
    };
    let versions = [version("val.py", None, Some(source))];
    assert!(
        agent(&config, root.path())
            .test_strength(&check, &versions, &mut NullUi)
            .is_none(),
        "undeclared checks are not behavioral"
    );
    config
        .verification
        .behavioral_checks
        .push(check.name.clone());
    let mut agent = agent(&config, root.path());
    let line = agent.test_strength(&check, &versions, &mut NullUi).unwrap();
    assert!(line.contains("1 of 2 sampled mutants"), "{line}");
    assert!(
        line.contains("val.py:2  `flag = True` -> `flag = False`"),
        "{line}"
    );
    let report = &agent.step_checks.counterfactual.test_strength["unit"];
    assert_eq!(
        (report["killed"].as_u64(), report["mutants"].as_u64()),
        (Some(1), Some(2))
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("val.py")).unwrap(),
        source
    );
    assert!(
        agent
            .test_strength(&check, &versions, &mut NullUi)
            .is_none(),
        "once per turn"
    );
}
