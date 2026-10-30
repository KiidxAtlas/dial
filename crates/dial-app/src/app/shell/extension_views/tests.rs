use super::*;
use crate::runtime::extensions::{Component, View};
fn snapshot(revision: u64) -> OwnedView {
    OwnedView {
        plugin: "demo".into(),
        revision,
        view: View {
            id: "workspace".into(),
            title: "Workspace".into(),
            location: Location::Sidebar,
            elements: vec![Component::Button {
                id: "run".into(),
                label: "Run".into(),
            }],
        },
    }
}
fn click(views: &mut ExtensionViews) {
    views.rows(80, 30, &Theme::dark());
    assert!(views.key(
        KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
        &Keymap::defaults()
    ));
}
#[test]
fn commands_keep_persistent_workspace_and_hide_without_destroying_state() {
    let mut views = ExtensionViews::default();
    assert!(views.sync(vec![snapshot(1)]));
    assert!(views.command("list").unwrap().contains("demo/workspace"));
    assert!(!views.rows(80, 30, &Theme::dark()).is_empty());
    views.command("demo/workspace focus").unwrap();
    click(&mut views);
    assert!(views.next_event().is_some());
    views.command("demo/workspace hide").unwrap();
    assert!(views.rows(80, 30, &Theme::dark()).is_empty());
    assert!(!views.sync(vec![snapshot(1)]));
    assert!(views.rows(80, 30, &Theme::dark()).is_empty());
    views.command("demo/workspace show").unwrap();
    assert!(!views.rows(80, 30, &Theme::dark()).is_empty());
    assert!(!views.focused);
    assert!(views.command("unknown/view").is_err());
}
#[test]
fn queued_events_are_revoked_by_revision_or_removal() {
    for remove in [false, true] {
        let mut views = ExtensionViews::default();
        views.sync(vec![snapshot(1)]);
        views.command("demo/workspace focus").unwrap();
        click(&mut views);
        views.sync(if remove { vec![] } else { vec![snapshot(2)] });
        assert!(views.next_event().is_none());
    }
}
#[test]
fn builtins_and_composer_remain_accessible_when_a_view_is_focused() {
    let mut views = ExtensionViews::default();
    views.sync(vec![snapshot(1)]);
    views.command("demo/workspace focus").unwrap();
    let keys = Keymap::defaults();
    assert!(!views.key(
        KeyEvent::new(KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL),
        &keys
    ));
    assert!(views.key(
        KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
        &keys
    ));
    assert!(!views.focused);
    assert!(!views.key(
        KeyEvent::new(KeyCode::Char('x'), crossterm::event::KeyModifiers::NONE),
        &keys
    ));
}

#[test]
fn every_application_shortcut_passes_through_focused_view() {
    let mut views = ExtensionViews::default();
    views.sync(vec![snapshot(1)]);
    views.command("demo/workspace focus").unwrap();
    let keys = Keymap::defaults();
    for (code, modifiers) in [
        (KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL),
        (KeyCode::Char('g'), crossterm::event::KeyModifiers::CONTROL),
    ] {
        assert!(!views.key(KeyEvent::new(code, modifiers), &keys));
    }
    assert!(views.focused);
}
