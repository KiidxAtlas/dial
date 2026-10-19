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
