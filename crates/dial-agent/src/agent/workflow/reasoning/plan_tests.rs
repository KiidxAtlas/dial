use super::*;

#[test]
fn planned_recovery_demands_a_todo_and_stays_bounded() {
    let mut recovery = Recovery::default();
    let message = recovery.intercept_planned("time limit").unwrap();
    assert!(message.contains("call todo"));
    assert!(
        recovery
            .intercept_planned("again")
            .unwrap_err()
            .contains("one recovery attempt")
    );
}

#[test]
fn require_plan_is_opt_in() {
    assert!(!ReasoningGuardConfig::default().require_plan);
}
