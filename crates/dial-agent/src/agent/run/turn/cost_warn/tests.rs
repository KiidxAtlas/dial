use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use crate::llm::usage::Purpose;

#[derive(Default)]
struct Notices(Vec<String>);

impl Ui for Notices {
    fn notice(&mut self, message: &str) {
        self.0.push(message.to_owned());
    }
}

fn spend(agent: &Agent<'_>, purpose: Purpose, cost: f64) {
    let mut receipt = agent
        .usage_scope
        .for_purpose(purpose)
        .reserve_external("test".into())
        .unwrap();
    receipt.mark_started().unwrap();
    receipt
        .account(&json!({ "cost": cost }), true, false)
        .unwrap();
}

#[test]
fn crossing_reports_each_upward_move_past_the_threshold_once() {
    let mut crossing = Crossing::new(true);
    assert!(!crossing.observe(0.5, 1.0));
    assert!(crossing.observe(1.2, 1.0));
    assert!(!crossing.observe(1.5, 1.0), "staying above does not repeat");
    assert!(!crossing.observe(0.9, 1.0));
    assert!(
        crossing.observe(1.1, 1.0),
        "dropping below re-arms the warning"
    );
}

#[test]
fn session_resumed_past_its_threshold_is_not_warned_again() {
    let mut crossing = Crossing::new(false);
    assert!(!crossing.observe(4.0, 1.0));
}

#[test]
fn session_warning_includes_subagent_spend_and_fires_once() {
    with_agent("[budget]\nwarn_session_cost_usd=1.0", |agent| {
        agent.usage_scope = agent.usage_scope.next_turn(0).unwrap();
        let mut turn_warn = Crossing::new(true);
        let mut ui = Notices::default();
        agent.observe_cost_warnings(&mut turn_warn, &mut ui);
        spend(agent, Purpose::Agent, 0.4);
        agent.observe_cost_warnings(&mut turn_warn, &mut ui);
        assert!(ui.0.is_empty(), "below the threshold nothing is said");
        // A worker's spend lands on the same shared session ledger as the parent's.
        spend(agent, Purpose::Retrieval, 0.7);
        agent.observe_cost_warnings(&mut turn_warn, &mut ui);
        agent.observe_cost_warnings(&mut turn_warn, &mut ui);
        assert_eq!(ui.0.len(), 1, "one notice for the crossing: {:?}", ui.0);
        assert!(ui.0[0].contains("warn_session_cost_usd"));
    });
}

#[test]
fn turn_warning_fires_once_and_never_stops_the_turn() {
    with_agent("[budget]\nwarn_turn_cost_usd=0.5", |agent| {
        agent.usage_scope = agent.usage_scope.next_turn(0).unwrap();
        let mut turn_warn = Crossing::new(true);
        let mut ui = Notices::default();
        spend(agent, Purpose::Agent, 0.6);
        agent.observe_cost_warnings(&mut turn_warn, &mut ui);
        agent.observe_cost_warnings(&mut turn_warn, &mut ui);
        assert_eq!(ui.0.len(), 1);
        assert!(ui.0[0].contains("warn_turn_cost_usd"));
        assert!(agent.stop_boundary().is_none(), "warnings do not stop work");
    });
}
