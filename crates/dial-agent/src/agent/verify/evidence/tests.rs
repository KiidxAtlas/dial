use super::*;

#[test]
fn failing_baseline_and_intermediate_attempts_do_not_poison_a_verified_repair() {
    let mut ledger = Ledger::default();
    ledger.observe("test", "baseline", CheckStatus::CodeFailed, [0, 0, 0], true);
    ledger.capture_baseline();
    ledger.observe("test", "attempt", CheckStatus::CodeFailed, [1, 0, 0], true);
    ledger.observe("test", "final", CheckStatus::Passed, [2, 0, 0], true);
    assert_eq!(ledger.passing_behavioral([2, 0, 0]), 1);
    let snapshot = ledger.snapshot([2, 0, 0]);
    assert_eq!(snapshot["baseline"]["test"]["status"], "code_failed");
    assert_eq!(snapshot["latest"]["test"]["status"], "passed");
}

#[test]
fn every_effect_kind_invalidates_earlier_credit_and_each_check_must_be_current() {
    let mut ledger = Ledger::default();
    ledger.observe("test a", "a", CheckStatus::Passed, [1, 0, 0], true);
    ledger.observe("test b", "b", CheckStatus::Passed, [1, 0, 0], true);
    assert_eq!(ledger.passing_behavioral([1, 0, 0]), 2);
    for revision in [[2, 0, 0], [1, 1, 0], [1, 0, 1]] {
        assert_eq!(ledger.passing_behavioral(revision), 0);
    }
    ledger.observe("test a", "a2", CheckStatus::Passed, [2, 0, 0], true);
    assert_eq!(ledger.passing_behavioral([2, 0, 0]), 0);
    assert_eq!(ledger.snapshot([2, 0, 0])["stale_checks"], 1);
    ledger.observe("test b", "b2", CheckStatus::Passed, [2, 0, 0], true);
    assert_eq!(ledger.passing_behavioral([2, 0, 0]), 2);
}

#[test]
fn unavailable_cancelled_or_unobserved_checks_cannot_earn_credit() {
    let mut ledger = Ledger::default();
    assert_eq!(ledger.passing_behavioral([0, 0, 0]), 0);
    for status in [
        CheckStatus::CodeFailed,
        CheckStatus::EnvironmentUnavailable,
        CheckStatus::PermissionDenied,
        CheckStatus::Cancelled,
        CheckStatus::TimedOut,
        CheckStatus::Unverified,
    ] {
        ledger.observe("test", "c", status, [1, 0, 0], true);
        assert_eq!(ledger.passing_behavioral([1, 0, 0]), 0, "{status:?}");
    }
    ledger.observe("build", "build", CheckStatus::Passed, [1, 0, 0], false);
    assert_eq!(ledger.passing_behavioral([1, 0, 0]), 0);
}

#[test]
fn call_settlement_covers_only_that_call_not_unrelated_later_effects() {
    let mut ledger = Ledger::default();
    ledger.observe("test", "check", CheckStatus::Passed, [0, 0, 0], true);
    ledger.settle_call("check", [0, 1, 0]);
    assert_eq!(ledger.passing_behavioral([0, 1, 0]), 1);
    ledger.settle_call("edit", [1, 1, 0]);
    assert_eq!(ledger.passing_behavioral([1, 1, 0]), 0);
}

#[test]
fn a_parse_or_compile_only_focused_check_is_not_a_behavior_pass() {
    let mut ledger = Ledger::default();
    let ast = "focused-check:python AST: filter.py";
    ledger.observe(
        "python3 -c ast.parse",
        ast,
        CheckStatus::Passed,
        [1, 0, 0],
        false,
    );
    ledger.observe(
        "cargo check",
        "focused-check:cargo check: .",
        CheckStatus::Passed,
        [1, 0, 0],
        false,
    );
    assert!(!ledger.current_focused_behavior_pass([0, 0, 0]));
    assert!(!ledger.current_focused_behavior_pass([1, 0, 0]));
    let tests = "focused-check:related Python tests: filter";
    ledger.observe("pytest", tests, CheckStatus::Passed, [1, 0, 0], false);
    assert!(ledger.current_focused_behavior_pass([1, 0, 0]));
}

fn repro(name: &str, command: &str) -> FocusedCheck {
    FocusedCheck {
        name: name.into(),
        command: command.into(),
        timeout_secs: 1,
    }
}

const RUN: &str = "focused-check:repro: bug";

#[test]
fn a_reproduction_earns_credit_only_after_failing_before_a_change_and_passing_at_the_final_revision()
 {
    let mut ledger = Ledger::default();
    ledger
        .register_repro(repro("repro: bug", "./repro.sh"))
        .unwrap();
    assert_eq!(
        ledger.repro_state("./repro.sh", [0, 0, 0]),
        ReproState::NotShownToReproduce
    );
    ledger.observe("./repro.sh", RUN, CheckStatus::CodeFailed, [0, 0, 0], false);
    assert_eq!(
        ledger.repro_state("./repro.sh", [0, 0, 0]),
        ReproState::NotPassing
    );
    assert_eq!(ledger.passing_behavioral([0, 0, 0]), 0);
    // The fix is an edit; the earlier failure becomes stale until rerun.
    assert_eq!(
        ledger.repro_state("./repro.sh", [1, 0, 0]),
        ReproState::Stale
    );
    ledger.observe("./repro.sh", RUN, CheckStatus::Passed, [1, 0, 0], false);
    assert_eq!(
        ledger.repro_state("./repro.sh", [1, 0, 0]),
        ReproState::Fixed
    );
    assert_eq!(ledger.passing_behavioral([1, 0, 0]), 1);
    assert_eq!(
        ledger.snapshot([1, 0, 0])["reproductions"][0]["state"],
        "fixed"
    );
    // A later effect of any kind makes the pass stale and withholds all behavioral credit.
    for revision in [[2, 0, 0], [1, 1, 0], [1, 0, 1]] {
        assert_eq!(
            ledger.repro_state("./repro.sh", revision),
            ReproState::Stale
        );
        assert_eq!(ledger.passing_behavioral(revision), 0);
    }
}

#[test]
fn never_failed_flaky_and_model_shell_runs_earn_no_reproduction_credit() {
    let mut ledger = Ledger::default();
    ledger.register_repro(repro("repro: a", "pass")).unwrap();
    ledger.register_repro(repro("repro: b", "flaky")).unwrap();
    ledger.register_repro(repro("repro: c", "shell")).unwrap();
    ledger.observe(
        "pass",
        "focused-check:repro: a",
        CheckStatus::Passed,
        [0, 0, 0],
        false,
    );
    ledger.observe(
        "pass",
        "focused-check:repro: a",
        CheckStatus::Passed,
        [1, 0, 0],
        false,
    );
    assert_eq!(
        ledger.repro_state("pass", [1, 0, 0]),
        ReproState::NotShownToReproduce
    );
    // Fail then pass with no change in between is not a fix.
    ledger.observe(
        "flaky",
        "focused-check:repro: b",
        CheckStatus::CodeFailed,
        [1, 0, 0],
        false,
    );
    ledger.observe(
        "flaky",
        "focused-check:repro: b",
        CheckStatus::Passed,
        [1, 0, 0],
        false,
    );
    assert_eq!(
        ledger.repro_state("flaky", [1, 0, 0]),
        ReproState::NotShownFixed
    );
    // Only controller runs are reproduction evidence; a model bash failure is not.
    ledger.observe("shell", "call-7", CheckStatus::CodeFailed, [0, 0, 0], false);
    ledger.observe(
        "shell",
        "focused-check:repro: c",
        CheckStatus::Passed,
        [1, 0, 0],
        false,
    );
    assert_eq!(
        ledger.repro_state("shell", [1, 0, 0]),
        ReproState::NotShownToReproduce
    );
    assert_eq!(ledger.passing_behavioral([1, 0, 0]), 0);
    // An environment failure is not a reproduction either.
    let mut ledger = Ledger::default();
    ledger.register_repro(repro("repro: env", "env")).unwrap();
    ledger.observe(
        "env",
        "focused-check:repro: env",
        CheckStatus::EnvironmentUnavailable,
        [0, 0, 0],
        false,
    );
    ledger.observe(
        "env",
        "focused-check:repro: env",
        CheckStatus::Passed,
        [1, 0, 0],
        false,
    );
    assert_eq!(ledger.passing_behavioral([1, 0, 0]), 0);
}

#[test]
fn reproductions_are_capped_replaced_by_name_and_combine_with_configured_behavioral_checks() {
    let mut ledger = Ledger::default();
    for index in 0..MAX_REPROS {
        assert!(
            ledger
                .register_repro(repro(&format!("repro: {index}"), &format!("r{index}")))
                .unwrap()
                .is_none()
        );
    }
    assert!(
        ledger
            .register_repro(repro("repro: extra", "extra"))
            .is_err()
    );
    assert_eq!(ledger.repros().len(), MAX_REPROS);
    // Replacing drops the old command's history: the new command must fail again.
    ledger.observe(
        "r0",
        "focused-check:repro: 0",
        CheckStatus::CodeFailed,
        [0, 0, 0],
        false,
    );
    let old = ledger.register_repro(repro("repro: 0", "r0b")).unwrap();
    assert_eq!(old.unwrap().command, "r0");
    assert!(!ledger.is_repro("r0"));
    assert_eq!(ledger.latest_status("r0"), None);
    ledger.observe(
        "r0b",
        "focused-check:repro: 0",
        CheckStatus::Passed,
        [1, 0, 0],
        false,
    );
    assert_eq!(
        ledger.repro_state("r0b", [1, 0, 0]),
        ReproState::NotShownToReproduce
    );

    // A fixed reproduction adds to configured behavioral credit; a failing one withholds it all.
    let mut ledger = Ledger::default();
    ledger.observe("test", "t", CheckStatus::Passed, [1, 0, 0], true);
    ledger.register_repro(repro("repro: bug", "bug")).unwrap();
    assert_eq!(ledger.passing_behavioral([1, 0, 0]), 1);
    ledger.observe("bug", RUN, CheckStatus::CodeFailed, [1, 0, 0], false);
    assert_eq!(ledger.passing_behavioral([1, 0, 0]), 0);
    ledger.observe("test", "t", CheckStatus::Passed, [2, 0, 0], true);
    ledger.observe("bug", RUN, CheckStatus::Passed, [2, 0, 0], false);
    assert_eq!(ledger.passing_behavioral([2, 0, 0]), 2);
}

#[test]
fn baseline_causality_compares_diagnostics_not_status_and_preserves_requested_scope() {
    let mut ledger = Ledger::default();
    ledger.observe("suite", "baseline", CheckStatus::CodeFailed, [0; 3], false);
    ledger.diagnose("suite", "FAILED tests::unrelated: expected 1 got 2");
    ledger.capture_baseline();
    ledger.observe("suite", "after", CheckStatus::CodeFailed, [1, 0, 0], false);
    ledger.diagnose("suite", "FAILED tests::unrelated: expected 1 got 2");
    assert!(ledger.unchanged_unrelated_failure("suite"));
    assert_eq!(
        ledger.snapshot([1, 0, 0])["latest"]["suite"]["status"],
        "code_failed"
    );
    ledger.diagnose("suite", "FAILED tests::new_regression: expected 1 got 3");
    assert!(!ledger.unchanged_unrelated_failure("suite"));
    ledger.observe(
        "suite",
        "explicit",
        CheckStatus::CodeFailed,
        [1, 0, 0],
        true,
    );
    ledger.diagnose("suite", "FAILED tests::unrelated: expected 1 got 2");
    assert!(!ledger.unchanged_unrelated_failure("suite"));
    ledger.observe(
        "suite",
        "unknown",
        CheckStatus::CodeFailed,
        [1, 0, 0],
        false,
    );
    assert!(!ledger.unchanged_unrelated_failure("suite"));
    ledger.diagnose("suite", &"x".repeat(65_537));
    assert!(!ledger.unchanged_unrelated_failure("suite"));
}

type Step = (&'static str, &'static str, CheckStatus, Revision);
/// (failed call, rerun call, change, verdict, checkpointed edits, uncertain effects)
type Link = (
    &'static str,
    &'static str,
    ChangeLink,
    CheckStatus,
    u64,
    u64,
);

#[test]
fn each_verdict_links_only_to_the_failure_it_reruns_with_its_change_kind() {
    use ChangeLink::{
        CheckedAfterCheckpointedEdits as Edited, CheckedAfterUncertainEffectsOnly as Effects,
        NoLongerReproduced as Gone,
    };
    use CheckStatus::{CodeFailed, EnvironmentUnavailable, Passed, Unverified};
    type Case = (&'static str, Vec<Step>, Vec<Option<Link>>);
    let cases: Vec<Case> = vec![
        (
            "a pass after a checkpointed edit resolves the failure",
            vec![
                ("t", "f1", CodeFailed, [1, 0, 0]),
                ("t", "r1", Passed, [2, 0, 0]),
            ],
            vec![None, Some(("f1", "r1", Edited, Passed, 1, 0))],
        ),
        (
            "negative: a same-revision pass is non-reproduction, and a later edit does not inherit it",
            vec![
                ("t", "f1", CodeFailed, [1, 0, 0]),
                ("t", "p0", Passed, [1, 0, 0]),
                ("t", "r1", Passed, [2, 0, 0]),
            ],
            vec![None, Some(("f1", "p0", Gone, Passed, 0, 0)), None],
        ),
        (
            "a same-revision failure replaces the pending identity",
            vec![
                ("t", "f1", CodeFailed, [1, 0, 0]),
                ("t", "f2", CodeFailed, [1, 0, 0]),
                ("t", "r1", Passed, [2, 0, 0]),
            ],
            vec![None, None, Some(("f2", "r1", Edited, Passed, 1, 0))],
        ),
        (
            "a same-revision inconclusive run keeps the failure pending",
            vec![
                ("t", "f1", CodeFailed, [1, 0, 0]),
                ("t", "u0", Unverified, [1, 0, 0]),
                ("t", "r1", Passed, [2, 0, 0]),
            ],
            vec![None, None, Some(("f1", "r1", Edited, Passed, 1, 0))],
        ),
        (
            "shell effects alone are not an edit",
            vec![
                ("t", "f1", CodeFailed, [1, 0, 0]),
                ("t", "r1", Passed, [1, 1, 0]),
            ],
            vec![None, Some(("f1", "r1", Effects, Passed, 0, 1))],
        ),
        (
            "extension effects alone are not an edit",
            vec![
                ("t", "f1", CodeFailed, [1, 0, 0]),
                ("t", "r1", Passed, [1, 0, 1]),
            ],
            vec![None, Some(("f1", "r1", Effects, Passed, 0, 1))],
        ),
        (
            "effects followed by an edit count the edit and the effects separately",
            vec![
                ("t", "f1", CodeFailed, [1, 0, 0]),
                ("t", "r1", Passed, [2, 1, 0]),
            ],
            vec![None, Some(("f1", "r1", Edited, Passed, 1, 1))],
        ),
        (
            "a still-failing run after an edit is its own verdict and becomes pending",
            vec![
                ("t", "f1", CodeFailed, [1, 0, 0]),
                ("t", "f2", CodeFailed, [2, 0, 0]),
                ("t", "r1", Passed, [3, 0, 0]),
            ],
            vec![
                None,
                Some(("f1", "f2", Edited, CodeFailed, 1, 0)),
                Some(("f2", "r1", Edited, Passed, 1, 0)),
            ],
        ),
        (
            "an inconclusive run after an edit is reported and clears the failure",
            vec![
                ("t", "f1", CodeFailed, [1, 0, 0]),
                ("t", "u1", Unverified, [2, 0, 0]),
                ("t", "r1", Passed, [3, 0, 0]),
            ],
            vec![None, Some(("f1", "u1", Edited, Unverified, 1, 0)), None],
        ),
        (
            "an environment problem after an edit is reported, not treated as a repair",
            vec![
                ("t", "f1", CodeFailed, [1, 0, 0]),
                ("t", "e1", EnvironmentUnavailable, [2, 0, 0]),
            ],
            vec![
                None,
                Some(("f1", "e1", Edited, EnvironmentUnavailable, 1, 0)),
            ],
        ),
        (
            "another check's run never resolves this failure",
            vec![
                ("t", "f1", CodeFailed, [1, 0, 0]),
                ("u", "o1", Passed, [2, 0, 0]),
                ("t", "r1", Passed, [2, 0, 0]),
                ("u", "o2", Passed, [3, 0, 0]),
            ],
            vec![None, None, Some(("f1", "r1", Edited, Passed, 1, 0)), None],
        ),
        (
            "an unresolved failure yields no verdict, and a pass without failure links nothing",
            vec![
                ("t", "f1", CodeFailed, [1, 0, 0]),
                ("u", "o1", Passed, [2, 0, 0]),
            ],
            vec![None, None],
        ),
    ];
    for (name, steps, expected) in cases {
        let mut ledger = Ledger::default();
        let actual: Vec<_> = steps
            .iter()
            .map(|(command, call, status, revision)| {
                ledger
                    .link_verdict(command, call, *status, *revision)
                    .map(|link| {
                        (
                            link.failed_call_id,
                            link.rerun_call_id,
                            link.change,
                            link.verdict,
                            link.checkpointed_edits_since_failure,
                            link.uncertain_effects_since_failure,
                        )
                    })
            })
            .collect();
        let expected: Vec<_> = expected
            .into_iter()
            .map(|link| {
                link.map(|(failed, rerun, change, verdict, edits, effects)| {
                    (
                        failed.to_owned(),
                        rerun.to_owned(),
                        change,
                        verdict,
                        edits,
                        effects,
                    )
                })
            })
            .collect();
        assert_eq!(actual, expected, "{name}");
    }
}
