use super::*;

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
fn acceptance_helpers_rerun_once_after_mutation_and_keep_their_origin() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = agent(&config, root.path());
    let output = agent.verify_tool(
        &json!({"acceptance":{"name":"file exists","command":"test -f ready"}}),
        &mut NullUi,
    );
    assert!(output.is_error, "{}", output.text);
    assert_eq!(output.data.as_ref().unwrap()["origin"], "model_registered");
    std::fs::write(root.path().join("ready"), "ready").unwrap();
    agent.step_checks.mutations += 1;
    let output = agent
        .auto_registered_checks(&mut NullUi)
        .expect("changed revision triggers one pass");
    assert!(!output.is_error, "{}", output.text);
    assert!(
        agent.auto_registered_checks(&mut NullUi).is_none(),
        "unchanged state must not rerun"
    );
    assert_eq!(
        agent
            .current_focused_checks()
            .iter()
            .filter(|check| check.name.starts_with("acceptance:"))
            .count(),
        1
    );
    assert!(
        agent.config.verification.commands.is_empty(),
        "model helpers cannot become configured task checks"
    );
}

#[test]
fn acceptance_registration_cannot_bypass_readonly_approval_or_replace_configured_checks() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = agent(&config, root.path());
    agent.approval = crate::config::types::Approval::Readonly;
    agent.plan_mode = true;
    let output = agent.verify_tool(
        &json!({"acceptance":{"name":"untrusted","command":"touch forbidden"}}),
        &mut NullUi,
    );
    assert!(output.is_error, "{}", output.text);
    assert_eq!(output.data.as_ref().unwrap()["results"][0]["ran"], false);
    assert!(!root.path().join("forbidden").exists());
    let count = agent.step_checks.registered_acceptance.len();
    let invalid = agent.verify_tool(
        &json!({"targets":[],"acceptance":{"name":"bad","command":"true"}}),
        &mut NullUi,
    );
    assert!(invalid.is_error);
    assert_eq!(agent.step_checks.registered_acceptance.len(), count);
}
