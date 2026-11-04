use super::*;
use crossterm::event::KeyModifiers;

fn event(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn help_does_not_steal_punctuation_from_text_entry() {
    for code in [KeyCode::Char('?'), KeyCode::Char('/')] {
        assert!(!PanelKeys::default().handle(&event(code), true, true));
    }
    let mut panel = PanelKeys::default();
    assert!(panel.handle(&event(KeyCode::Char('?')), false, true));
    assert!(panel.handle(&event(KeyCode::Char('x')), false, true));
    assert!(panel.handle(&event(KeyCode::Esc), false, true));
    assert!(!panel.handle(&event(KeyCode::Char('x')), false, true));
    assert!(panel.handle(&event(KeyCode::Char('/')), false, true));
    assert!(!panel.handle(&event(KeyCode::Char('?')), false, true));
    assert!(panel.handle(&event(KeyCode::F(1)), true, true));
}
