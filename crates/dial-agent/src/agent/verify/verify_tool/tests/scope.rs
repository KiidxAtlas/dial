use super::*;

#[test]
fn no_execution_receipts_distinguish_empty_scope_from_unobserved_checks() {
    for configured in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let config = if configured {
            config()
        } else {
            Config::default()
        };
        let mut agent = hermetic_agent(&config, root.path());
        for args in [json!({}), json!({"targets":["failed"]})] {
            let output = agent.verify_tool(&args, &mut NullUi);
            assert!(!output.is_error, "{}", output.text);
            let data = output.data.expect("structured no-execution receipt");
            assert_eq!(data["results"], json!([]));
            assert_eq!(
                data["scope"]["applicable_checks"],
                if configured { 3 } else { 0 }
            );
            assert_eq!(
                data["scope"]["pending_count"],
                if configured { 3 } else { 0 }
            );
            assert_eq!(data["scope"]["omitted_pending_checks"], 0);
            assert_eq!(data["revision"], json!(agent.step_checks.revision()));
            assert!(
                agent.verifications.is_empty(),
                "listing scope cannot execute checks"
            );
        }
    }
}

#[test]
fn passing_subset_reports_other_checks_and_preserves_stale_evidence() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config();
    config
        .verification
        .commands
        .retain(|check| check.name != "python AST: setup");
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.mutations = 1;
    std::fs::write(root.path().join("fixed"), "ready").unwrap();
    let first = agent.verify_tool(&json!({}), &mut NullUi);
    assert!(!first.is_error, "{}", first.text);
    assert_eq!(first.data.unwrap()["scope"]["pending_count"], 0);
    assert_eq!(agent.verifications.len(), 2);

    agent.step_checks.mutations += 1;
    let subset = agent.verify_tool(&json!({"targets":["unit"]}), &mut NullUi);
    assert!(
        !subset.is_error,
        "the selected check did pass: {}",
        subset.text
    );
    let data = subset.data.unwrap();
    assert_eq!(data["results"].as_array().unwrap().len(), 1);
    assert_eq!(
        data["scope"]["pending_checks"],
        json!([{"name":"lint","status":"stale"}])
    );
    assert_eq!(data["scope"]["pending_count"], 1);
    assert!(subset.text.contains("lint (stale)"), "{}", subset.text);
    assert_eq!(
        agent.verifications.len(),
        3,
        "scope reporting must not rerun the unselected check"
    );
    assert_eq!(
        agent
            .step_checks
            .evidence
            .current_status(LINT, agent.step_checks.revision()),
        "stale"
    );
}

#[test]
fn scope_lists_failures_without_hiding_or_executing_them_in_a_passing_subset() {
    let root = tempfile::tempdir().unwrap();
    let config = config();
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.mutations = 1;
    let failed = agent.verify_tool(&json!({}), &mut NullUi);
    assert!(failed.is_error);
    assert_eq!(failed.data.unwrap()["scope"]["pending_count"], 2);
    let selected = agent.verify_tool(&json!({"targets":["unit"]}), &mut NullUi);
    assert!(!selected.is_error, "{}", selected.text);
    let data = selected.data.unwrap();
    assert_eq!(
        data["scope"]["pending_checks"],
        json!([
            {"name":"lint","status":"code_failed"},
            {"name":"python AST: setup","status":"environment_unavailable"}
        ])
    );
    assert_eq!(agent.verifications.len(), 4);
    assert!(
        agent.step_checks.failed,
        "the retained failure obligation remains open"
    );
}

#[test]
fn scope_discloses_omission_in_bounded_pending_list_without_running_anything() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    for i in 0..20 {
        config
            .verification
            .commands
            .push(crate::config::types::VerificationCommand {
                name: format!("check-{i}"),
                command: format!("echo check-{i}"),
                file_extensions: vec![],
                timeout_secs: 5,
            });
    }
    let mut agent = hermetic_agent(&config, root.path());
    let output = agent.verify_tool(&json!({"targets":["failed"]}), &mut NullUi);
    let data = output.data.unwrap();
    assert_eq!(data["scope"]["applicable_checks"], 20);
    assert_eq!(data["scope"]["pending_count"], 20);
    assert_eq!(
        data["scope"]["pending_checks"].as_array().unwrap().len(),
        16
    );
    assert_eq!(data["scope"]["omitted_pending_checks"], 4);
    assert!(output.text.contains("16 more"), "{}", output.text);
    assert!(agent.verifications.is_empty());
}
