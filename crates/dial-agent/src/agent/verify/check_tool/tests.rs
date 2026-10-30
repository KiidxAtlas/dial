use super::*;

mod incremental;

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
fn predictions_are_offered_only_under_the_flag_and_mismatches_are_reported() {
    assert!(
        schema(false)
            .pointer("/function/parameters/properties/expect")
            .is_none()
    );
    assert!(
        schema(true)
            .pointer("/function/parameters/properties/expect")
            .is_some()
    );
    let root = tempfile::tempdir().unwrap();
    let mut config = config();
    config
        .verification
        .commands
        .retain(|check| check.name == "unit");
    // Off: the argument is inert.
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.mutations = 1;
    let off = agent.check_tool(&json!({"expect":"fail"}), &mut NullUi);
    assert!(off.data.as_ref().unwrap().get("prediction").is_none());
    assert!(!off.text.contains("Prediction mismatch"), "{}", off.text);

    let mut config = config.clone();
    config.experimental.check_predictions = true;
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.mutations = 1;
    let invalid = agent.check_tool(&json!({"expect":"maybe"}), &mut NullUi);
    assert!(invalid.is_error);
    let missed = agent.check_tool(&json!({"names":["unit"],"expect":"fail"}), &mut NullUi);
    assert!(!missed.is_error, "the check itself passed");
    assert_eq!(
        missed.data.as_ref().unwrap()["prediction"],
        json!({"expected":"fail","observed":"pass","matched":false})
    );
    assert!(
        missed
            .text
            .contains("Prediction mismatch: expected fail, observed pass")
    );
    let matched = agent.check_tool(&json!({"names":["unit"],"expect":"pass"}), &mut NullUi);
    assert_eq!(
        matched.data.as_ref().unwrap()["prediction"]["matched"],
        true
    );
    assert!(!matched.text.contains("Prediction mismatch"));
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

const REPRO: &str = "test -f fixed || { echo 'assertion failed: bug present'; exit 1; }";

fn repro_args(name: &str, command: &str) -> Value {
    json!({"repro":{"name":name,"command":command}})
}

#[test]
fn a_registered_reproduction_earns_behavioral_credit_only_after_fail_then_fix() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    // Registration runs it at once, even before any change: this is the pre-fix observation.
    let first = agent.check_tool(&repro_args("bug", REPRO), &mut NullUi);
    assert!(first.is_error, "{}", first.text);
    assert!(
        first.text.contains("code_failed: repro: bug"),
        "{}",
        first.text
    );
    assert!(first.text.contains("reproduces the bug"), "{}", first.text);
    assert_eq!(
        first.data.as_ref().unwrap()["results"][0]["reproduction"],
        "not_passing"
    );
    // The completion gate now selects it like any other check, and retains its failure.
    assert!(
        agent
            .current_focused_checks()
            .iter()
            .any(|check| check.name == "repro: bug" && check.command == REPRO)
    );
    assert!(agent.step_checks.failed);

    // Rerunning with no change is not a fix, even if it now passes.
    std::fs::write(root.path().join("fixed"), "").unwrap();
    let unchanged = agent.check_tool(&json!({"names":["repro: bug"]}), &mut NullUi);
    assert!(!unchanged.is_error, "{}", unchanged.text);
    assert!(
        unchanged.text.contains("no behavioral credit"),
        "{}",
        unchanged.text
    );
    let revision = agent.step_checks.revision();
    assert_eq!(agent.step_checks.evidence.passing_behavioral(revision), 0);

    // A change after the failure, then a pass at the final revision, earns credit.
    std::fs::remove_file(root.path().join("fixed")).unwrap();
    agent.check_tool(
        &json!({"failed_only":false,"names":["repro: bug"]}),
        &mut NullUi,
    );
    std::fs::write(root.path().join("fixed"), "").unwrap();
    agent.step_checks.mutations += 1;
    let fixed = agent.check_tool(&json!({"failed_only":true}), &mut NullUi);
    assert!(!fixed.is_error, "{}", fixed.text);
    assert!(fixed.text.contains("passes now"), "{}", fixed.text);
    let revision = agent.step_checks.revision();
    assert_eq!(agent.step_checks.evidence.passing_behavioral(revision), 1);
    let evidence = agent.completion_evidence(TurnOutcome::Completed);
    assert_eq!(evidence["checks"]["passing_behavioral_checks"], 1);
    assert_eq!(evidence["checks"]["reproductions"][0]["state"], "fixed");
    assert!(!agent.step_checks.failed);

    // A later edit makes the pass stale and withholds the credit until a fresh run.
    agent.step_checks.mutations += 1;
    let revision = agent.step_checks.revision();
    assert_eq!(agent.step_checks.evidence.passing_behavioral(revision), 0);
    assert_eq!(
        agent.completion_evidence(TurnOutcome::Completed)["checks"]["reproductions"][0]["state"],
        "stale"
    );
}

#[test]
fn a_reproduction_that_never_failed_is_reported_and_earns_no_behavioral_credit() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.mutations = 1;
    let out = agent.check_tool(&repro_args("bug", "echo fine"), &mut NullUi);
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("passed: repro: bug"), "{}", out.text);
    assert!(out.text.contains("not shown to reproduce"), "{}", out.text);
    let revision = agent.step_checks.revision();
    assert_eq!(agent.step_checks.evidence.passing_behavioral(revision), 0);
    let evidence = agent.completion_evidence(TurnOutcome::Completed);
    assert_eq!(
        evidence["checks"]["reproductions"][0]["state"],
        "not_shown_to_reproduce"
    );
    // Like any observed check it still counts as a current passing check.
    assert_eq!(evidence["checks"]["passing_checks"], 1);
}

#[test]
fn reproductions_are_capped_validated_and_cannot_relabel_selected_checks() {
    let root = tempfile::tempdir().unwrap();
    let config = config();
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.mutations = 1;
    for index in 0..MAX_REPROS {
        let out = agent.check_tool(
            &repro_args(&format!("r{index}"), &format!("echo {index}")),
            &mut NullUi,
        );
        assert!(!out.is_error, "{}", out.text);
    }
    let over = agent.check_tool(&repro_args("extra", "echo extra"), &mut NullUi);
    assert!(over.is_error);
    assert!(over.text.contains("at most 4"), "{}", over.text);
    // Reusing a name replaces it within the cap.
    let replaced = agent.check_tool(&repro_args("r0", "echo replaced"), &mut NullUi);
    assert!(!replaced.is_error, "{}", replaced.text);
    assert!(agent.step_checks.evidence.is_repro("echo replaced"));
    assert!(!agent.step_checks.evidence.is_repro("echo 0"));
    assert_eq!(agent.step_checks.evidence.repros().len(), MAX_REPROS);
    // Names are prefixed `repro: `, so only the command can collide with a selected check.
    for (args, expected) in [
        (repro_args("mine", UNIT), "already a selected check"),
        (
            repro_args("dup", "echo 1"),
            "already registered as repro: r1",
        ),
        (repro_args("", "echo x"), "repro.name"),
        (repro_args("x", "  "), "repro.command"),
    ] {
        let out = agent.check_tool(&args, &mut NullUi);
        assert!(out.is_error, "{args}");
        assert!(out.text.contains(expected), "{args}: {}", out.text);
    }
    // Registration is turn state: a new turn's StepChecks starts without any.
    agent.step_checks = StepChecks::default();
    assert!(agent.step_checks.evidence.repros().is_empty());
    assert!(
        !agent
            .current_focused_checks()
            .iter()
            .any(|check| check.name.starts_with("repro: "))
    );
}

#[test]
fn reproductions_take_the_shell_approval_path_not_the_generated_check_boundary() {
    use crate::agent::safety::approval::{Effect, PermissionRule};
    use crate::config::types::Approval;
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    agent.hermetic = false;
    agent.step_checks.mutations = 1;
    let marker = root.path().join("ran");
    // A generated check runs unprompted under Ask; a reproduction asks, and NullUi refuses.
    agent.approval = Approval::Ask;
    let asked = agent.check_tool(&repro_args("bug", "touch ran"), &mut NullUi);
    assert!(asked.is_error);
    assert!(
        asked.text.contains("permission_denied: repro: bug"),
        "{}",
        asked.text
    );
    assert!(!marker.exists());
    // Write mode: like bash, a non-destructive workspace command runs without a prompt.
    agent.approval = Approval::Write;
    let ran = agent.check_tool(&json!({"names":["repro: bug"]}), &mut NullUi);
    assert!(!ran.is_error, "{}", ran.text);
    assert!(marker.exists());
    // Its run is an opaque, disclosed shell effect and stales earlier evidence.
    assert_eq!(agent.step_checks.uncheckpointed_shell_commands, 1);
    assert!(!agent.step_checks.uncheckpointed_pending());
    assert_eq!(agent.step_checks.revision(), [1, 1, 0]);
    // Deny rules and read-only mode hold.
    std::fs::remove_file(&marker).unwrap();
    agent.always.push(PermissionRule {
        effect: Effect::Deny,
        tool: "bash".into(),
        command_prefix: vec!["touch".into()],
        paths: vec![],
    });
    let denied = agent.check_tool(&json!({"names":["repro: bug"]}), &mut NullUi);
    assert!(denied.text.contains("permission_denied"), "{}", denied.text);
    agent.always.clear();
    agent.approval = Approval::Readonly;
    let readonly = agent.check_tool(&json!({"names":["repro: bug"]}), &mut NullUi);
    assert!(
        readonly.text.contains("permission_denied"),
        "{}",
        readonly.text
    );
    assert!(!marker.exists());
    // Permission denials are not code failures, so the reproduction is still not shown.
    let revision = agent.step_checks.revision();
    assert_eq!(
        agent
            .step_checks
            .evidence
            .repro_state("touch ran", revision),
        crate::agent::verify::evidence::ReproState::NotShownToReproduce
    );
}

#[test]
fn mutating_reproduction_stales_required_checks_and_delivered_visual_evidence() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config();
    config.verification.requirements.insert(
        "behavior".into(),
        crate::config::types::VerificationRequirement {
            checks: vec!["unit".into()],
            ..Default::default()
        },
    );
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.mutations = 1;
    let baseline = agent.check_tool(&json!({"names":["unit"]}), &mut NullUi);
    assert!(!baseline.is_error);
    agent.step_checks.record_checked();
    let before = agent.step_checks.revision();
    agent.step_checks.visual.require_rendered();
    agent.step_checks.visual.captured(before, true);
    agent.step_checks.visual.delivered();
    assert_eq!(agent.step_checks.visual.status(before), "observed");

    let out = agent.check_tool(
        &repro_args("mutates", "printf changed > source.rs"),
        &mut NullUi,
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(
        std::fs::read_to_string(root.path().join("source.rs")).unwrap(),
        "changed"
    );
    let after = agent.step_checks.revision();
    assert_ne!(after, before);
    assert_eq!(out.data.as_ref().unwrap()["revision"], json!(after));
    assert_eq!(
        agent.step_checks.evidence.current_status(UNIT, after),
        "stale"
    );
    assert_eq!(agent.step_checks.visual.status(after), "missing");
    assert!(!agent.step_checks.checks_current());
    let evidence = agent.completion_evidence(TurnOutcome::Completed);
    assert_eq!(evidence["checks"]["passing_behavioral_checks"], 0);
    assert_eq!(
        evidence["checks"]["latest"]["printf changed > source.rs"]["revision"],
        json!(after)
    );
}

#[test]
fn unsafe_reproduction_rerun_cannot_earn_its_own_fix_credit() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    let command = "if test -f ran; then exit 0; else touch ran; exit 1; fi";
    let first = agent.check_tool(&repro_args("stateful", command), &mut NullUi);
    assert!(first.is_error, "{}", first.text);
    assert_eq!(
        first.data.as_ref().unwrap()["results"][0]["reproduction"],
        "not_passing"
    );
    let rerun = agent.check_tool(&json!({"names":["repro: stateful"]}), &mut NullUi);
    assert!(!rerun.is_error, "{}", rerun.text);
    assert_eq!(
        rerun.data.as_ref().unwrap()["results"][0]["reproduction"],
        "not_shown_fixed"
    );
    assert_eq!(agent.step_checks.revision(), [0, 2, 0]);
    assert_eq!(rerun.data.as_ref().unwrap()["revision"], json!([0, 2, 0]));
    assert_eq!(
        agent
            .step_checks
            .evidence
            .passing_behavioral(agent.step_checks.revision()),
        0
    );
}

#[test]
fn a_top_level_command_is_refused_with_the_repro_contract_and_runs_nothing() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = hermetic_agent(&config, root.path());
    let refused = agent.check_tool(&json!({"command": REPRO}), &mut NullUi);
    assert!(refused.is_error, "{}", refused.text);
    assert!(refused.text.contains("repro"), "{}", refused.text);
    assert!(refused.text.contains("non-zero"), "{}", refused.text);
    // No reproduction was registered, so nothing was executed or recorded for this turn.
    assert!(agent.step_checks.evidence.repros().is_empty());
}
