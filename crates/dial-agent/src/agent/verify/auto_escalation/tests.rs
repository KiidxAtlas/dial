use super::*;

fn settings(after: u32, cap: u32) -> EscalationConfig {
    EscalationConfig {
        enabled: true,
        auto_after_failed_rounds: after,
        max_auto_per_turn: cap,
        ..EscalationConfig::default()
    }
}

#[test]
fn fires_exactly_on_threshold_round() {
    let mut auto = AutoEscalation::default();
    let s = settings(3, 1);
    assert!(!auto.round(true, &s, true));
    assert!(!auto.round(true, &s, true));
    assert!(auto.round(true, &s, true));
}

#[test]
fn successful_round_resets_streak() {
    let mut auto = AutoEscalation::default();
    let s = settings(3, 1);
    assert!(!auto.round(true, &s, true));
    assert!(!auto.round(true, &s, true));
    assert!(!auto.round(false, &s, true));
    assert!(!auto.round(true, &s, true));
    assert!(!auto.round(true, &s, true));
    assert!(auto.round(true, &s, true));
}

#[test]
fn zero_threshold_disables() {
    let mut auto = AutoEscalation::default();
    let s = settings(0, 1);
    for _ in 0..10 {
        assert!(!auto.round(true, &s, true));
    }
}

#[test]
fn disabled_or_unconfigured_role_never_fires() {
    let mut auto = AutoEscalation::default();
    let disabled = EscalationConfig {
        enabled: false,
        ..settings(1, 1)
    };
    for _ in 0..5 {
        assert!(!auto.round(true, &disabled, true));
    }
    let mut auto = AutoEscalation::default();
    for _ in 0..5 {
        assert!(!auto.round(true, &settings(1, 1), false));
    }
}

#[test]
fn per_turn_cap_holds_until_new_turn() {
    let s = settings(1, 1);
    let mut auto = AutoEscalation::default();
    assert!(auto.round(true, &s, true));
    for _ in 0..5 {
        assert!(!auto.round(true, &s, true));
    }
    let mut next_turn = AutoEscalation::default();
    assert!(next_turn.round(true, &s, true));
}

#[test]
fn zero_cap_never_fires() {
    let mut auto = AutoEscalation::default();
    for _ in 0..5 {
        assert!(!auto.round(true, &settings(1, 0), true));
    }
}

#[test]
fn ledger_is_bounded_and_clipped_on_char_boundary() {
    let mut auto = AutoEscalation::default();
    let long = "é".repeat(2000);
    for _ in 0..10 {
        auto.remember(&long);
    }
    let question = auto.question("goal");
    let evidence = question["evidence"].as_array().expect("evidence array");
    assert_eq!(evidence.len(), MAX_LEDGER_ITEMS);
    for item in evidence {
        let text = item.as_str().expect("string item");
        assert!(text.len() <= MAX_ITEM_BYTES);
        assert!(text.chars().all(|c| c == 'é'));
    }
}

#[test]
fn advice_is_transient_user_message() {
    let message = advice_message("check the cache key");
    assert_eq!(message["role"], "user");
    assert_eq!(message["dial_transient"], true);
    assert!(
        message["content"]
            .as_str()
            .unwrap()
            .contains("Unverified advice")
    );
}

#[test]
fn automatic_call_follows_the_escalate_approval_decision() {
    use crate::agent::{Decision, Ui, safety::approval::authorize};
    use crate::{config::types::Approval, llm::ToolCall};
    struct Deny(usize);
    impl Ui for Deny {
        fn approve(&mut self, _: &str, _: &str) -> Decision {
            self.0 += 1;
            Decision::Deny
        }
    }
    let call = ToolCall {
        id: "auto-escalation-1".into(),
        name: "escalate".into(),
        arguments: json!({"goal": "g"}),
        malformed_arguments: false,
    };
    let root = std::path::Path::new("/p");
    let mut rules = Vec::new();
    // A user denial and a read-only policy both refuse, so the coach model is never called.
    let mut ui = Deny(0);
    assert!(authorize(Approval::Ask, &mut rules, root, &call, "", &mut ui).is_err());
    assert_eq!(ui.0, 1);
    assert!(authorize(Approval::Readonly, &mut rules, root, &call, "", &mut ui).is_err());
    assert_eq!(ui.0, 1, "read-only policy must not prompt");
    assert!(authorize(Approval::Auto, &mut rules, root, &call, "", &mut ui).is_ok());
}
