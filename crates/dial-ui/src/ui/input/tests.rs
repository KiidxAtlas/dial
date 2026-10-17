use super::*;
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
#[test]
fn a_typed_command_line_is_submitted_not_split_by_a_buffered_enter() {
    assert!(enter_submits_command("/settings"));
    assert!(enter_submits_command("/sett"));
    assert!(
        !enter_submits_command("/settings\n"),
        "a real multi-line paste keeps its breaks"
    );
    assert!(!enter_submits_command("plain prompt"));
    assert!(!enter_submits_command(""));
}

#[test]
fn raw_paste_keeps_all_newlines_until_a_separate_enter() {
    let now = Instant::now();
    let mut guard = PasteGuard::default();
    assert_eq!(guard.text_for_key(key(KeyCode::Char('a')), true, now), None);
    assert_eq!(
        guard.text_for_key(key(KeyCode::Enter), true, now),
        Some("\n")
    );
    assert_eq!(guard.text_for_key(key(KeyCode::Char('b')), true, now), None);
    assert_eq!(
        guard.text_for_key(key(KeyCode::Enter), false, now),
        Some("\n")
    );
    assert_eq!(
        guard.text_for_key(key(KeyCode::Enter), false, now + PASTE_SETTLE),
        None
    );
}
#[test]
fn bracketed_paste_discards_trailing_enter_without_queuing_a_turn() {
    let now = Instant::now();
    let mut guard = PasteGuard::default();
    guard.pasted(now);
    assert_eq!(
        guard.text_for_key(key(KeyCode::Enter), false, now),
        Some("")
    );
    assert_eq!(
        guard.text_for_key(key(KeyCode::Enter), false, now + PASTE_SETTLE),
        None
    );
}
#[test]
fn raw_paste_can_begin_with_a_blank_line() {
    let now = Instant::now();
    let mut guard = PasteGuard::default();
    assert_eq!(
        guard.text_for_key(key(KeyCode::Enter), true, now),
        Some("\n")
    );
    assert_eq!(
        guard.text_for_key(key(KeyCode::Enter), false, now),
        Some("\n")
    );
}
#[test]
fn ordinary_typing_navigation_and_explicit_shortcuts_are_preserved() {
    let now = Instant::now();
    let mut guard = PasteGuard::default();
    assert_eq!(
        guard.text_for_key(key(KeyCode::Char('a')), false, now),
        None
    );
    assert_eq!(guard.text_for_key(key(KeyCode::Enter), false, now), None);
    guard.pasted(now);
    assert_eq!(guard.text_for_key(key(KeyCode::Left), false, now), None);
    assert_eq!(guard.text_for_key(key(KeyCode::Enter), false, now), None);
    guard.pasted(now);
    assert_eq!(
        guard.text_for_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), false, now),
        None
    );
}

#[test]
fn a_stalled_burst_ending_in_enter_submits_but_an_inner_line_break_stays() {
    // Keys queued behind a stalled loop: `o` sees the Enter queued after it, the Enter sees nothing after it.
    let now = Instant::now();
    let mut guard = PasteGuard::default();
    assert_eq!(guard.text_for_key(key(KeyCode::Char('o')), true, now), None);
    assert_eq!(
        guard.text_for_key(key(KeyCode::Enter), false, now),
        None,
        "a plain Enter with nothing queued after it must submit"
    );
    // A real unbracketed paste with a break before more text still keeps its newline.
    let mut paste = PasteGuard::default();
    assert_eq!(paste.text_for_key(key(KeyCode::Char('a')), true, now), None);
    assert_eq!(
        paste.text_for_key(key(KeyCode::Enter), true, now),
        Some("\n")
    );
    assert_eq!(
        paste.text_for_key(key(KeyCode::Char('b')), false, now),
        None
    );
    assert_eq!(
        paste.text_for_key(key(KeyCode::Enter), false, now),
        Some("\n")
    );
}
