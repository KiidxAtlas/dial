use super::*;

#[test]
fn hold_starts_on_press_and_stops_on_release() {
    let mut g = Gesture::default();
    assert_eq!(g.press(true), Command::Start { hold: true });
    assert!(g.recording());
    assert_eq!(g.release(), Some(Command::Stop));
    assert!(!g.recording());
}

#[test]
fn release_while_idle_is_ignored() {
    let mut g = Gesture::default();
    assert_eq!(g.release(), None);
    assert!(!g.recording());
}

#[test]
fn toggle_fallback_ignores_release_and_stops_on_next_press() {
    let mut g = Gesture::default();
    assert_eq!(g.press(false), Command::Start { hold: false });
    assert_eq!(
        g.release(),
        None,
        "release must not stop a toggle recording"
    );
    assert!(g.recording());
    assert_eq!(g.press(false), Command::Stop);
    assert!(!g.recording());
}

#[test]
fn press_while_held_stops_after_lost_release() {
    let mut g = Gesture::default();
    assert_eq!(g.press(true), Command::Start { hold: true });
    assert_eq!(
        g.press(true),
        Command::Stop,
        "a second press stops a lost-release recording"
    );
    assert!(!g.recording());
    assert_eq!(g.release(), None);
}

#[test]
fn finished_worker_resets_so_next_press_starts() {
    let mut g = Gesture::default();
    assert_eq!(g.press(true), Command::Start { hold: true });
    g.finished();
    assert!(!g.recording());
    assert_eq!(g.press(true), Command::Start { hold: true });
}
