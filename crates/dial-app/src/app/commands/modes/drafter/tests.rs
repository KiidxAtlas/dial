use super::*;

#[test]
fn toggle_status_and_invalid_arguments_do_not_guess() {
    for current in [false, true] {
        assert_eq!(next_state(current, "").unwrap(), Some(!current));
        assert_eq!(next_state(current, "on").unwrap(), Some(true));
        assert_eq!(next_state(current, "off").unwrap(), Some(false));
        assert_eq!(next_state(current, "status").unwrap(), None);
        assert!(next_state(current, "on yes").is_err());
        assert!(next_state(current, "accept").is_err());
    }
}

#[test]
fn drafter_is_discoverable_and_saves_without_stopping_a_busy_turn() {
    use crate::app::command_registry::{DuringTurn, command_entries, during_turn};
    let entries = command_entries(&[], &[]);
    assert!(entries.iter().any(|entry| entry.name == "drafter"));
    let completions = crate::app::commands::completions(&Default::default());
    for argument in ["on", "off", "status"] {
        assert!(completions.contains(&format!("/drafter {argument}")));
        assert_eq!(during_turn("drafter", argument), DuringTurn::Now);
    }
}
