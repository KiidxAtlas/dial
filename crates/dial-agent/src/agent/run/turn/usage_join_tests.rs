use super::*;
use crate::llm::usage::{Purpose, Scope};

fn endpoint(model: &str) -> Backend {
    Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: model.into(),
            api_key: None,
        },
        Default::default(),
        5,
    )
}

#[test]
fn replacement_backends_join_the_turn_ledger() {
    let usage = Scope::default().next_turn(0).unwrap();
    let mut backend = endpoint("fallback");
    join_turn_usage(&usage, &mut backend);
    backend.record_discarded_response().unwrap();
    assert_eq!(usage.snapshot().0.all.discarded_attempts, 1);
}

#[test]
fn rejected_replies_are_discarded_under_their_requesting_purpose() {
    let usage = Scope::default().next_turn(0).unwrap();
    let mut backend = endpoint("primary");
    record_rejected_reply(&usage, &mut backend, true).unwrap();
    let (total, _) = usage.snapshot();
    assert_eq!(total.by_purpose[&Purpose::Repair].discarded_attempts, 1);
    assert!(!total.by_purpose.contains_key(&Purpose::Agent));
}

#[test]
fn continuation_rounds_stop_at_the_external_turn_cost_including_auxiliary_usage() {
    use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
    with_agent("[budget]\nmax_turn_cost_usd=1.0", |agent| {
        agent.usage_scope = agent.usage_scope.next_turn(0).unwrap();
        for (purpose, cost) in [(Purpose::Agent, 0.6), (Purpose::Memory, 0.4)] {
            let mut receipt = agent
                .usage_scope
                .for_purpose(purpose)
                .reserve_external("test".into())
                .unwrap();
            receipt.mark_started().unwrap();
            receipt.account(&json!({"cost":cost}), true, false).unwrap();
        }
        let mut backend = agent.backend.take().unwrap();
        // Verification repairs and stop hooks reenter this same loop, not a new external turn.
        for _ in 0..2 {
            let (report, _) = agent
                .run_loop(&mut backend, &[], &mut vec![], 10, "continue", &mut NullUi)
                .unwrap();
            assert_eq!(report.outcome, TurnOutcome::Incomplete);
            assert_eq!(report.steps, 0, "no continuation request may start");
            assert!(report.text.contains("this turn is $1.0000"));
        }
        agent.backend = Some(backend);
    });
}
