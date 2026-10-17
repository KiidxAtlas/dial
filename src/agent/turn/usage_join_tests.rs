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
