use super::*;

fn hermetic_agent<'a>(config: &'a Config, root: &Path) -> Agent<'a> {
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

const UNIT: &str = "echo 'test result: ok. 1 passed'";
const LINT: &str = "test -f fixed || { echo 'error: lint rule broken'; exit 1; }";
const SETUP: &str = "echo 'No module named pytest'; exit 1";

fn config() -> Config {
    let mut config = Config::default();
    for (name, command) in [("unit", UNIT), ("lint", LINT), ("python AST: setup", SETUP)] {
        config
            .verification
            .commands
            .push(crate::config::types::VerificationCommand {
                name: name.into(),
                command: command.into(),
                file_extensions: vec![],
                timeout_secs: 30,
            });
    }
    config.verification.behavioral_checks.push("unit".into());
    config
}

#[test]
fn reports_typed_statuses_and_records_evidence_final_verification_reads() {
    let root = tempfile::tempdir().unwrap();
    let config = config();
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.mutations = 1;
    let out = agent.check_tool(&json!({}), &mut NullUi);
    assert!(out.is_error, "a non-passing check is a failed observation");
    assert!(
        out.text.contains(&format!("passed: unit\n  $ {UNIT}")),
        "{}",
        out.text
    );
    assert!(out.text.contains("code_failed: lint"), "{}", out.text);
    assert!(out.text.contains("error: lint rule broken"), "{}", out.text);
    assert!(
        out.text
            .contains("environment_unavailable: python AST: setup"),
        "{}",
        out.text
    );
    assert!(out.text.contains("Verification environment unavailable"));

    // The ledger final verification reads holds the same typed results at this revision.
    let revision = agent.step_checks.revision();
    let ledger = &agent.step_checks.evidence;
    assert_eq!(ledger.current_status(UNIT, revision), "passed");
    assert_eq!(ledger.current_status(LINT, revision), "code_failed");
    assert_eq!(
        ledger.current_status(SETUP, revision),
        "environment_unavailable"
    );
    assert_eq!(ledger.passing_behavioral(revision), 1);
    let evidence = agent.completion_evidence(TurnOutcome::Completed);
    assert_eq!(evidence["checks"]["current_checks"], 3);
    assert_eq!(evidence["checks"]["passing_checks"], 1);
    assert_eq!(evidence["verification_coverage"], "failed");
    // The gate retains the non-passing checks for its next pass.
    assert!(agent.step_checks.failed);
    assert_eq!(agent.step_checks.failed_checks.len(), 2);

    // A later edit makes every result stale rather than reusable.
    agent.step_checks.mutations += 1;
    let revision = agent.step_checks.revision();
    assert_eq!(
        agent.step_checks.evidence.current_status(UNIT, revision),
        "stale"
    );
}

#[test]
fn failed_only_reruns_code_failures_and_a_pass_clears_the_retained_failure() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config();
    config
        .verification
        .commands
        .retain(|check| check.name != "python AST: setup");
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.mutations = 1;
    let nothing = agent.check_tool(&json!({"failed_only":true}), &mut NullUi);
    assert!(!nothing.is_error);
    assert!(nothing.text.contains("nothing run"), "{}", nothing.text);

    agent.check_tool(&json!({}), &mut NullUi);
    assert!(agent.step_checks.failed);
    std::fs::write(root.path().join("fixed"), "").unwrap();
    let rerun = agent.check_tool(&json!({"failed_only":true}), &mut NullUi);
    assert!(!rerun.is_error, "{}", rerun.text);
    assert!(rerun.text.contains("passed: lint"), "{}", rerun.text);
    assert!(
        !rerun.text.contains("unit"),
        "only the failure reran: {}",
        rerun.text
    );
    assert_eq!(
        rerun.data.as_ref().unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(agent.step_checks.failed_checks.is_empty());
    assert!(!agent.step_checks.failed);
    let revision = agent.step_checks.revision();
    assert_eq!(
        agent.step_checks.evidence.current_status(LINT, revision),
        "passed"
    );
}

#[test]
fn before_any_change_it_lists_checks_and_runs_only_a_named_baseline() {
    let root = tempfile::tempdir().unwrap();
    let config = config();
    let mut agent = hermetic_agent(&config, root.path());
    let listed = agent.check_tool(&json!({}), &mut NullUi);
    assert!(!listed.is_error);
    assert!(listed.text.contains("No changes yet"), "{}", listed.text);
    assert!(listed.text.contains("unit; lint"), "{}", listed.text);
    assert_eq!(agent.step_checks.evidence.latest_status(UNIT), None);

    let unknown = agent.check_tool(&json!({"names":["nope"]}), &mut NullUi);
    assert!(unknown.is_error);
    assert!(unknown.text.contains("Available: unit"), "{}", unknown.text);

    let baseline = agent.check_tool(&json!({"names":["unit"]}), &mut NullUi);
    assert!(!baseline.is_error, "{}", baseline.text);
    assert_eq!(
        agent.step_checks.evidence.latest_status(UNIT),
        Some(CheckStatus::Passed)
    );
    assert_eq!(agent.step_checks.evidence.latest_status(LINT), None);
}

#[test]
fn check_is_offered_by_default_and_on_the_small_tier_and_revert_edit_stays_loadable() {
    use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
    for overlay in ["", "context_window=16384"] {
        with_agent(overlay, |agent| {
            let offered = agent.offered();
            assert!(
                offered.iter().any(|name| name == NAME),
                "{overlay}: {offered:?}"
            );
            assert!(
                !offered.iter().any(|name| name == "revert_edit"),
                "{overlay}: {offered:?}"
            );
            assert!(agent.catalog().iter().any(|name| name == "revert_edit"));
            assert!(agent.schemas(&offered).is_ok());
        });
    }
    with_agent("[verification]\nenabled=false", |agent| {
        assert!(!agent.catalog().iter().any(|name| name == NAME));
    });
}
