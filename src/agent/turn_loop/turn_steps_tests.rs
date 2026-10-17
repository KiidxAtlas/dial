use super::TurnSteps;

#[test]
fn initial_repair_and_stop_hook_rounds_share_one_finite_budget() {
    let mut steps = TurnSteps::new(10);
    assert_eq!(steps.charge(4), 4);
    assert_eq!(steps.remaining(), Some(6));
    // A repair round reports the cumulative turn count, not only its own steps.
    assert_eq!(steps.charge(5), 9);
    assert_eq!(steps.remaining(), Some(1));
    // The stop-hook continuation gets only what is left.
    assert_eq!(steps.charge(1), 10);
    assert_eq!(steps.remaining(), None);
}

#[test]
fn zero_budget_stays_unlimited_but_still_counts_steps() {
    let mut steps = TurnSteps::new(0);
    assert_eq!(steps.charge(1_000), 1_000);
    assert_eq!(steps.remaining(), Some(0));
}
