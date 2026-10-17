use super::*;

#[test]
fn state_round_trips_through_control_state() {
    let state = State {
        vibe: true,
        skillful: Some(false),
        ..Default::default()
    };
    let control = state.persisted(&json!({"other": 1}));
    assert_eq!(control["other"], 1);
    let restored = State::restore(&control);
    assert_eq!((restored.vibe, restored.skillful), (true, Some(false)));
}

#[test]
fn vibe_offers_only_read_only_tools() {
    let state = State {
        vibe: true,
        ..Default::default()
    };
    assert!(state.vibe_allows("read"));
    assert!(!state.vibe_allows("edit"));
    assert!(State::default().vibe_allows("edit"));
}
