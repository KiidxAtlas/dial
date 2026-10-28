use super::*;
use crate::app::shell::testkit::shell;
use std::cell::Cell;
use std::collections::VecDeque;

fn key(code: KeyCode) -> Event {
    Event::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn tool_startup_keeps_polling_after_enter_and_preserves_input_order() {
    let (_home, _project, _app, mut shell) = shell();
    let enter = key(KeyCode::Enter);
    let later = Event::Paste("next command".into());
    let resize = Event::Resize(110, 40);
    let mut events = VecDeque::from([
        key(KeyCode::Char('a')),
        enter.clone(),
        later.clone(),
        resize.clone(),
    ]);
    let pending = Cell::new(true);
    let polls = Cell::new(0);
    shell.edit_while_events(
        || pending.get(),
        |_| {
            polls.set(polls.get() + 1);
            let event = events.pop_front();
            if event.is_none() {
                pending.set(false);
            }
            Ok(event)
        },
    );
    assert_eq!(
        polls.get(),
        5,
        "do not enter synchronous construction before tools are ready"
    );
    assert_eq!(shell.composer.draft(), "a");
    assert_eq!(shell.held, [enter, later, resize]);
}

#[test]
fn rebuild_with_held_input_still_waits_cooperatively_for_tools() {
    let (_home, _project, _app, mut shell) = shell();
    let enter = key(KeyCode::Enter);
    shell.held.push_back(enter.clone());
    let pending = Cell::new(true);
    let typed = key(KeyCode::Char('b'));
    shell.edit_while_events(
        || pending.get(),
        |_| {
            pending.set(false);
            Ok(Some(typed.clone()))
        },
    );
    assert!(!pending.get());
    assert_eq!(shell.held, [enter, typed]);
    assert!(shell.composer.draft().is_empty());
}

#[test]
fn tool_startup_edits_draft_until_an_action_is_held() {
    let (_home, _project, _app, mut shell) = shell();
    let pending = Cell::new(true);
    let mut events = VecDeque::from([
        Event::Paste("hello".into()),
        key(KeyCode::Backspace),
        key(KeyCode::Char('p')),
    ]);
    shell.edit_while_events(
        || pending.get(),
        |_| {
            let event = events.pop_front();
            if event.is_none() {
                pending.set(false);
            }
            Ok(event)
        },
    );
    assert_eq!(shell.composer.draft(), "hellp");
    assert!(shell.held.is_empty());
}
