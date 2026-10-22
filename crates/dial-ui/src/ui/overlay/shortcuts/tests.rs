use super::super::settings::testkit::*;
use super::*;
use crate::ui::theme::{Glyphs, Theme};
use crossterm::event::{KeyEvent, KeyEventKind, KeyModifiers};

fn select(overlay: &mut ShortcutOverlay, action: Action) {
    overlay.search.set(action.id());
    overlay.cursor = 0;
}

fn draw(overlay: &mut ShortcutOverlay, rig: &Rig, w: u16, h: u16) -> ratatui::buffer::Buffer {
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    render(w, h, |frame| overlay.draw(frame, &ctx))
}

#[test]
fn searches_description_id_group_and_effective_key_including_s() {
    let keys = Keymap::defaults();
    let mut overlay = ShortcutOverlay::new();
    assert_eq!(overlay.rows(&keys), Action::ALL);
    overlay.handle(&ch('s'), &keys);
    assert_eq!(overlay.search.text, "s");
    assert!(overlay.rows(&keys).contains(&Action::Settings));
    for (query, action) in [
        ("prompt history", Action::HistorySearch),
        ("APP.SETTINGS", Action::Settings),
        ("Navigation", Action::SelectDown),
        ("Ctrl+,", Action::Settings),
    ] {
        overlay.search.set(query);
        assert!(overlay.rows(&keys).contains(&action), "{query}");
    }
    overlay.search.set("no-such-action");
    assert!(overlay.rows(&keys).is_empty());
    assert!(matches!(
        overlay.handle(&key(KeyCode::Enter), &keys),
        OverlayEvent::None
    ));
    assert!(matches!(
        overlay.handle(&key(KeyCode::Delete), &keys),
        OverlayEvent::None
    ));
    assert!(overlay.edit.is_none());
}

#[test]
fn edit_paste_save_and_settle_preserve_comma_chords() {
    let keys = Keymap::defaults();
    let mut overlay = ShortcutOverlay::new();
    select(&mut overlay, Action::Settings);
    overlay.handle(&key(KeyCode::Enter), &keys);
    assert_eq!(overlay.edit.as_ref().unwrap().1.text, "Ctrl+, Ctrl+4");
    overlay.handle(&ctrl('u'), &keys);
    overlay.handle(&Event::Paste("Ctrl+,\nAlt+界\x1b[31m".into()), &keys);
    assert_eq!(overlay.edit.as_ref().unwrap().1.text, "Ctrl+, Alt+界");
    let OverlayEvent::Rebind { action, chords } = overlay.handle(&key(KeyCode::Enter), &keys)
    else {
        panic!("expected rebind")
    };
    assert_eq!(action, Action::Settings);
    assert_eq!(chords, ["Ctrl+,", "Alt+界"]);
    assert!(overlay.edit.is_some());
    overlay.settled(Err("Cannot save: read-only".into()));
    assert_eq!(overlay.edit.as_ref().unwrap().1.text, "Ctrl+, Alt+界");
    assert!(!overlay.message.as_ref().unwrap().1);
    let rig = Rig::new();
    assert!(screen(&draw(&mut overlay, &rig, 90, 16)).contains("Cannot save: read-only"));
    overlay.settled(Ok("Saved global shortcuts".into()));
    assert!(overlay.edit.is_none());
    assert_eq!(
        overlay.message,
        Some(("Saved global shortcuts".into(), true))
    );
    assert_eq!(keys.chords(Action::Settings).len(), 2);
}

#[test]
fn layered_cancel_disable_and_restore_defaults() {
    let mut keys = Keymap::defaults();
    keys.rebind(Action::Settings, &["F8"]).unwrap();
    let mut overlay = ShortcutOverlay::new();
    select(&mut overlay, Action::Settings);
    overlay.handle(&key(KeyCode::Enter), &keys);
    assert_eq!(overlay.edit.as_ref().unwrap().1.text, "F8");
    overlay.handle(&ctrl('u'), &keys);
    assert!(
        matches!(overlay.handle(&key(KeyCode::Enter), &keys), OverlayEvent::Rebind { chords, .. } if chords.is_empty())
    );
    assert!(matches!(
        overlay.handle(&key(KeyCode::Esc), &keys),
        OverlayEvent::None
    ));
    assert!(overlay.edit.is_none());
    let OverlayEvent::Rebind { action, chords } = overlay.handle(&key(KeyCode::Delete), &keys)
    else {
        panic!("expected defaults")
    };
    assert_eq!(action, Action::Settings);
    assert_eq!(chords, Action::Settings.defaults());
    assert!(matches!(
        overlay.handle(&key(KeyCode::Esc), &keys),
        OverlayEvent::None
    ));
    assert!(overlay.search.text.is_empty());
    assert!(matches!(
        overlay.handle(&key(KeyCode::Esc), &keys),
        OverlayEvent::Close
    ));
}

#[test]
fn remapped_navigation_confirmation_cancel_and_release() {
    let mut keys = Keymap::defaults();
    for (action, chord) in [
        (Action::SelectDown, "F2"),
        (Action::SelectUp, "F3"),
        (Action::SelectConfirm, "F4"),
        (Action::SelectCancel, "F5"),
        (Action::SelectPageDown, "F6"),
        (Action::SelectPageUp, "F7"),
    ] {
        keys.rebind(action, &[chord]).unwrap();
    }
    let mut overlay = ShortcutOverlay::new();
    overlay.handle(&key(KeyCode::Down), &keys);
    assert_eq!(overlay.cursor, 0);
    overlay.handle(&key(KeyCode::F(2)), &keys);
    assert_eq!(overlay.cursor, 1);
    overlay.handle(&key(KeyCode::F(3)), &keys);
    assert_eq!(overlay.cursor, 0);
    overlay.handle(&key(KeyCode::F(6)), &keys);
    assert_eq!(overlay.cursor, overlay.page);
    overlay.handle(&key(KeyCode::F(7)), &keys);
    assert_eq!(overlay.cursor, 0);
    let release = Event::Key(KeyEvent::new_with_kind(
        KeyCode::F(2),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ));
    overlay.handle(&release, &keys);
    assert_eq!(overlay.cursor, 0);
    overlay.handle(&key(KeyCode::Enter), &keys);
    assert!(overlay.edit.is_none());
    overlay.handle(&key(KeyCode::F(4)), &keys);
    assert!(overlay.edit.is_some());
    assert!(matches!(
        overlay.handle(&key(KeyCode::F(4)), &keys),
        OverlayEvent::Rebind { .. }
    ));
    overlay.handle(&key(KeyCode::F(5)), &keys);
    assert!(overlay.edit.is_none());
    assert!(matches!(
        overlay.handle(&key(KeyCode::F(5)), &keys),
        OverlayEvent::Close
    ));
}

#[test]
fn short_edit_screens_keep_the_field_and_footer_even_after_failure() {
    let rig = Rig::new();
    let mut overlay = ShortcutOverlay::new();
    select(&mut overlay, Action::Settings);
    overlay.handle(&key(KeyCode::Enter), &rig.keys);
    for failed in [false, true] {
        if failed {
            overlay.settled(Err("Cannot save".into()));
        }
        for height in [5, 6, 8] {
            let shown = screen(&draw(&mut overlay, &rig, 80, height));
            assert!(
                shown.contains("Chords: Ctrl+, Ctrl+4"),
                "height {height}: {shown}"
            );
            assert!(
                shown.contains("Enter save globally"),
                "height {height}: {shown}"
            );
            assert!(shown.contains("Esc cancel"), "height {height}: {shown}");
            if failed && height > 5 {
                assert!(shown.contains("Cannot save"));
            }
        }
    }
}

#[test]
fn disabled_movement_has_no_empty_key_hint() {
    let mut rig = Rig::new();
    rig.keys.rebind(Action::SelectUp, &[]).unwrap();
    rig.keys.rebind(Action::SelectDown, &[]).unwrap();
    let shown = screen(&draw(&mut ShortcutOverlay::new(), &rig, 180, 16));
    assert!(!shown.contains(" move"));
    assert!(shown.contains("Enter edit"));
}

#[test]
fn theme_ascii_selection_scroll_disabled_and_unicode_bounds() {
    let theme = Theme::dark().with_glyphs(Glyphs::Ascii);
    let mut rig = Rig::with_theme(theme);
    rig.keys.rebind(Action::Submit, &[]).unwrap();
    let mut overlay = ShortcutOverlay::new();
    // The selected row is customized (disabled), so restoring its defaults is offered.
    let buffer = draw(&mut overlay, &rig, 100, 16);
    let shown = screen(&buffer);
    assert!(shown.is_ascii());
    for label in [
        "global only",
        "Disabled",
        "customized",
        "Defaults: Enter",
        "Delete restore defaults",
    ] {
        assert!(shown.contains(label), "{label}: {shown}");
    }
    assert_eq!(
        style_of(&buffer, "Send the message").unwrap().bg,
        rig.theme.bg(Tok::SelectedBg).bg
    );
    for _ in 0..Action::ALL.len() {
        overlay.handle(&key(KeyCode::Down), &rig.keys);
    }
    draw(&mut overlay, &rig, 70, 14);
    assert!(overlay.scroll > 0);
    assert!(overlay.cursor >= overlay.scroll && overlay.cursor < overlay.scroll + overlay.page);
    overlay.search.set("界e\u{301}👩‍💻");
    assert!(screen(&draw(&mut overlay, &rig, 100, 16)).contains("No matching shortcuts"));
    select(&mut overlay, Action::Settings);
    overlay.handle(&key(KeyCode::Enter), &rig.keys);
    overlay.handle(&Event::Paste("界e\u{301}👩‍💻".repeat(60)), &rig.keys);
    for editing in [true, false] {
        if !editing {
            overlay.edit = None;
        }
        for (w, h) in [(0, 0), (1, 1), (2, 2), (3, 3), (8, 5), (30, 10), (240, 40)] {
            let lines = overlay.compose(
                w,
                h,
                &DrawCtx {
                    theme: &rig.theme,
                    keys: &rig.keys,
                    status: &rig.status,
                    preview_status: &status_preview,
                },
            );
            assert!(lines.len() <= usize::from(h));
            assert!(
                lines
                    .iter()
                    .all(|line| text::line_width(line) <= usize::from(w))
            );
            draw(&mut overlay, &rig, w, h);
        }
    }
}

#[test]
fn delete_on_default_keys_explains_instead_of_saving_and_home_end_jump() {
    let rig = Rig::new();
    let mut overlay = ShortcutOverlay::new();
    select(&mut overlay, Action::Settings);
    assert!(matches!(
        overlay.handle(&key(KeyCode::Delete), &rig.keys),
        OverlayEvent::None
    ));
    let shown = screen(&draw(&mut overlay, &rig, 100, 16));
    assert!(shown.contains("Already using the default keys"), "{shown}");
    assert!(!shown.contains("Delete restore defaults"), "{shown}");
    let mut overlay = ShortcutOverlay::new();
    overlay.handle(&key(KeyCode::End), &rig.keys);
    assert_eq!(overlay.cursor, Action::ALL.len() - 1);
    overlay.handle(&key(KeyCode::Home), &rig.keys);
    assert_eq!(overlay.cursor, 0);
}

#[test]
fn clipped_descriptions_end_in_an_ellipsis() {
    let rig = Rig::new();
    let shown = screen(&draw(&mut ShortcutOverlay::new(), &rig, 60, 20));
    assert!(shown.contains('…'), "{shown}");
}
