//! Typed attempt contract: model failures are unsolved and memoised, infrastructure failures are
//! never memoised (so retries run), and workflow/profile consumers share one paired population.
use super::*;
use crate::config::types::SuiteScore;
use crate::llm::errors::ThinkingLoop;

fn measured(passed: bool, calls: u64) -> workflow::Attempt {
    workflow::Attempt::Measured(workflow::CaseMeasurement {
        passed,
        metrics: workflow::Metrics {
            tool_calls: calls,
            ..Default::default()
        },
    })
}

fn case(id: &str) -> Case {
    Case {
        id: id.into(),
        prompt: format!("prompt for {id}"),
        files: Default::default(),
        check: crate::learn::verify::Check::AnswerExact {
            expected: "ok".into(),
        },
    }
}

#[test]
fn model_failures_are_unsolved_attempts_while_infrastructure_and_fatal_errors_are_not_cached() {
    let model = Err(anyhow::Error::new(ThinkingLoop("loop".into())));
    assert_eq!(
        settle(model).unwrap(),
        workflow::Attempt::ModelFailed,
        "model failure is memoised as unsolved"
    );
    assert!(
        settle(Err(anyhow::anyhow!(
            "model server returned HTTP 503: service unavailable"
        )))
        .is_err(),
        "infrastructure failure must propagate so the gate retries it"
    );
    assert!(
        settle(Err(anyhow::anyhow!("SECRET prompt and output"))).is_err(),
        "fatal failures propagate"
    );
    let measurement = workflow::CaseMeasurement {
        passed: true,
        metrics: workflow::Metrics::default(),
    };
    assert_eq!(
        settle(Ok(measurement.clone())).unwrap(),
        workflow::Attempt::Measured(measurement)
    );
}

#[test]
fn workflow_and_profiles_share_one_population_and_model_failures_block_cost_claims() {
    let mut cache = Cache::new();
    cache.insert((1, "a".into()), workflow::Attempt::ModelFailed);
    cache.insert((2, "a".into()), measured(true, 3));
    cache.insert((1, "b".into()), measured(true, 5)); // candidate side excluded by infrastructure
    cache.insert((1, "c".into()), measured(true, 5));
    cache.insert((2, "c".into()), measured(true, 2));
    let suite = Suite {
        name: "tools".into(),
        primary: true,
        cases: vec![case("a"), case("b"), case("c")],
    };
    let pairs = paired_attempts(&[suite], 1, 2, &cache);
    assert_eq!(pairs.len(), 2, "excluded pairs are not in either consumer");
    let attempts: Vec<_> = pairs
        .iter()
        .map(|(_, before, after)| (before.clone(), after.clone()))
        .collect();
    let mut admission = Admission {
        baseline: 1,
        candidate: 2,
        passed: true,
        reason: "sealed gate verdict".into(),
        scores: vec![SuiteScore {
            name: "tools".into(),
            primary: true,
            baseline: 50.0,
            candidate: 100.0,
            cases: 2,
            candidate_only: 1,
            baseline_only: 0,
            tier: None,
        }],
    };
    let report = workflow::compare("workflow:protocol", &mut admission, &attempts);
    assert!(!report.admitted, "unmeasured cost blocks cost claims");
    assert_eq!(report.unmeasured_cost, 1);
    assert_eq!(
        report.candidate.tool_calls, 2,
        "cost totals cover only fully measured pairs, not a zero for the model failure"
    );
    assert!(report.reason.contains("without measured cost"));
}

#[test]
fn curriculum_denominator_keeps_model_failures_and_holds_on_infrastructure_exclusions() {
    let clean = TierProbe {
        passed: 9,
        total: 10,
        excluded: 0,
    };
    assert_eq!(next_tier(0, &clean), 1);
    let excluded = TierProbe {
        passed: 9,
        total: 9,
        excluded: 1,
    };
    assert_eq!(
        next_tier(0, &excluded),
        0,
        "a shrunken denominator must not promote"
    );
    let with_model_failure = TierProbe {
        passed: 8,
        total: 10,
        excluded: 0,
    };
    assert_eq!(
        next_tier(0, &with_model_failure),
        0,
        "a model failure stays unsolved in the denominator"
    );
}

#[test]
fn lora_capacity_grows_only_on_primary_gain_shortfalls() {
    use gate::Outcome;
    assert!(Outcome::TooSmall.needs_more_signal());
    assert!(Outcome::Futile.needs_more_signal());
    assert!(Outcome::NotSignificant.needs_more_signal());
    assert!(!Outcome::Regressed.needs_more_signal());
    assert!(!Outcome::TooFewNetWins.needs_more_signal());
    assert!(!Outcome::Overridden.needs_more_signal());
    assert!(!Outcome::Accepted.needs_more_signal());
}
