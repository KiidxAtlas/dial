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
fn only_configured_commands_and_related_tests_are_witnessed() {
    let mut config = Config::default();
    config
        .verification
        .commands
        .push(crate::config::types::VerificationCommand {
            name: "unit".into(),
            command: "make test".into(),
            file_extensions: vec![],
            timeout_secs: 30,
        });
    let check = |name: &str, command: &str| FocusedCheck {
        name: name.into(),
        command: command.into(),
        timeout_secs: 30,
    };
    assert!(behavioral(&config, &check("unit", "make test")));
    assert!(behavioral(
        &config,
        &check("related Rust tests: x", "cargo test x")
    ));
    assert!(!behavioral(
        &config,
        &check("python AST: a.py", "python -c ...")
    ));
    assert!(!behavioral(
        &config,
        &check("cargo check: .", "cargo check")
    ));
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
    let mut agent = agent(&config, root.path());
    let check = FocusedCheck {
        name: "unit".into(),
        command: "grep -qx yes fixed && grep -qx ok other".into(),
        timeout_secs: 30,
    };
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
        "once per revision"
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
