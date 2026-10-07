use dial::{
    config::types::{Config, GateConfig, Harness, Proposal, SuiteConfig, ToolConfig, Version},
    learn::improve::validate_proposal,
    storage::paths::Layout,
};

struct Fixture {
    _home: tempfile::TempDir,
    project: tempfile::TempDir,
    layout: Layout,
    config: Config,
    active: Version,
}

fn configured() -> Fixture {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let mut config = Config::load(&layout, None).unwrap();
    config.model = "operator/model".into();
    let active = Version {
        id: 1,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    Fixture {
        _home: home,
        project,
        layout,
        config,
        active,
    }
}

#[test]
fn feedback_cannot_enable_unapproved_tools_or_structural_training() {
    let f = configured();
    let mut harness = f.active.harness.clone();
    harness.tools.push("shell".into());
    let mut proposal = Proposal {
        diagnosis: "Use a tool to verify results".into(),
        operation: "harness".into(),
        harness: Some(harness),
    };
    assert!(validate_proposal(&f.config, &f.active, &proposal).is_err());
    proposal.operation = "expert_growth".into();
    proposal.harness = None;
    assert!(validate_proposal(&f.config, &f.active, &proposal).is_err());
}

#[test]
fn weight_changes_require_operator_permission_and_cannot_bundle_harness_edits() {
    let mut f = configured();
    f.config.training.enabled = false;
    let mut proposal = Proposal {
        diagnosis: "Verified capability failures".into(),
        operation: "lora".into(),
        harness: None,
    };
    assert!(validate_proposal(&f.config, &f.active, &proposal).is_err());
    f.config.training.enabled = true;
    assert!(validate_proposal(&f.config, &f.active, &proposal).is_ok());
    proposal.harness = Some(f.active.harness.clone());
    assert!(validate_proposal(&f.config, &f.active, &proposal).is_err());
}

#[test]
fn invalid_generation_budgets_do_not_reach_model_execution() {
    let f = configured();
    let mut harness = f.active.harness.clone();
    harness.temperature = f64::NAN;
    assert!(f.config.validate_harness(&harness).is_err());
    harness.temperature = 0.0;
    harness.max_steps = 0;
    assert!(f.config.validate_harness(&harness).is_ok());
    harness.max_steps = 65;
    assert!(f.config.validate_harness(&harness).is_err());
    harness.max_steps = 4;
    harness.max_tokens = 9000;
    assert!(f.config.validate_harness(&harness).is_err());
}

#[test]
fn protected_data_cannot_live_inside_the_project() {
    let mut f = configured();
    let suites = |dir: &std::path::Path| GateConfig {
        suites: vec![
            SuiteConfig {
                name: "a".into(),
                path: dir.join("a.jsonl"),
                primary: true,
            },
            SuiteConfig {
                name: "b".into(),
                path: dir.join("b.jsonl"),
                primary: false,
            },
        ],
        max_regression: 1.0,
        min_improvement: 1.0,
    };
    f.config.gate = Some(suites(&f.layout.state_dir.join("private")));
    assert!(f.config.validate().is_ok());
    // Suites an agent confined to the project could read are rejected.
    f.config.gate = Some(suites(f.project.path()));
    assert!(f.config.validate().is_err());
    f.config.gate = Some(suites(&f.layout.state_dir.join("private")));
    f.config.training.train_data = f.project.path().join("train.jsonl");
    assert!(f.config.validate().is_err());
}

#[test]
fn builtin_and_extra_tools_are_selectable_but_cannot_be_shadowed() {
    let mut f = configured();
    let harness = Harness {
        tools: vec!["read".into(), "edit".into(), "todo".into()],
        ..f.active.harness.clone()
    };
    assert!(f.config.validate_harness(&harness).is_ok());
    f.config.tools.insert(
        "bash".into(),
        ToolConfig {
            program: "/bin/echo".into(),
            args: vec![],
            timeout_secs: 5,
        },
    );
    assert!(f.config.validate().is_err());
    f.config.tools.clear();
    f.config.tools.insert(
        "retain".into(),
        ToolConfig {
            program: "/bin/echo".into(),
            args: vec![],
            timeout_secs: 5,
        },
    );
    assert!(f.config.validate().is_err());
}

#[test]
fn layered_config_cannot_be_overridden_from_inside_the_project() {
    let f = configured();
    // Config files are read only from the user and state directories, never from the project.
    std::fs::write(
        f.project.path().join("dial.toml"),
        "approval = \"auto\"\n[sandbox]\nmode = \"off\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(f.project.path().join(".dial")).unwrap();
    std::fs::write(
        f.project.path().join(".dial/config.toml"),
        "approval = \"auto\"\n",
    )
    .unwrap();
    let loaded = Config::load(&f.layout, None).unwrap();
    assert_eq!(loaded.approval, dial::config::types::Approval::Ask);
    assert_eq!(loaded.sandbox.mode, dial::config::types::SandboxMode::Dev);
}
