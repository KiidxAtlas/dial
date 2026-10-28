use super::*;
use crate::extensions::panel::Element;
use crossterm::event::KeyEvent;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn form(generation: u64) -> Panel {
    Panel {
        title: "Name it".into(),
        generation,
        elements: vec![
            Element::Text("Pick a name and mode.".into()),
            Element::Input {
                id: "name".into(),
                label: "Name".into(),
                value: String::new(),
            },
            Element::Select {
                id: "mode".into(),
                label: "Mode".into(),
                options: vec!["fast".into(), "safe".into()],
                selected: 0,
            },
            Element::Button {
                id: "cancel".into(),
                label: "Cancel".into(),
                primary: false,
            },
            Element::Button {
                id: "save".into(),
                label: "Save".into(),
                primary: true,
            },
        ],
    }
}

#[test]
fn typing_and_backspace_edit_the_initial_input() {
    let mut dialog = PanelDialog::new(form(1)).unwrap();
    for c in "abc".chars() {
        assert!(dialog.key(key(KeyCode::Char(c))).is_none());
    }
    dialog.key(key(KeyCode::Backspace));
    assert_eq!(dialog.text_of("name"), "ab");
}

#[test]
fn left_and_right_cycle_the_focused_select() {
    let mut dialog = PanelDialog::new(form(1)).unwrap();
    dialog.key(key(KeyCode::Tab));
    dialog.key(key(KeyCode::Right));
    assert_eq!(dialog.values["mode"], Value::Choice(1));
    dialog.key(key(KeyCode::Right));
    assert_eq!(dialog.values["mode"], Value::Choice(0));
}

#[test]
fn enter_on_the_default_button_submits_typed_values() {
    let mut dialog = PanelDialog::new(form(1)).unwrap();
    dialog.paste("dial\n");
    let outcome = dialog.key(key(KeyCode::Enter));
    let Some(Outcome::Submitted { button, values }) = outcome else {
        panic!("expected submission, got {outcome:?}");
    };
    assert_eq!(button, "save");
    assert_eq!(values["name"], Value::Text("dial".into()));
    assert_eq!(values["mode"], Value::Choice(0));
}

#[test]
fn enter_on_a_focused_button_submits_that_button() {
    let mut dialog = PanelDialog::new(form(1)).unwrap();
    for _ in 0..2 {
        dialog.key(key(KeyCode::Tab));
    }
    let Some(Outcome::Submitted { button, .. }) = dialog.key(key(KeyCode::Enter)) else {
        panic!("expected submission");
    };
    assert_eq!(button, "cancel");
}

#[test]
fn escape_and_ctrl_c_cancel_without_a_submission() {
    let mut dialog = PanelDialog::new(form(1)).unwrap();
    assert_eq!(dialog.key(key(KeyCode::Esc)), Some(Outcome::Cancelled));
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(dialog.key(ctrl_c), Some(Outcome::Cancelled));
}

#[test]
fn a_panel_with_only_a_title_and_text_is_rejected_at_construction() {
    let panel = Panel {
        title: "Empty".into(),
        generation: 1,
        elements: vec![Element::Text("no buttons".into())],
    };
    assert!(PanelDialog::new(panel).is_err());
}

#[test]
fn updates_of_another_generation_are_ignored() {
    let mut dialog = PanelDialog::new(form(1)).unwrap();
    assert!(!dialog.update(form(2)).unwrap());
    assert!(dialog.update(form(1)).unwrap());
}

#[test]
fn an_update_keeps_values_the_new_revision_still_accepts() {
    let mut dialog = PanelDialog::new(form(1)).unwrap();
    dialog.paste("kept");
    dialog.update(form(1)).unwrap();
    assert_eq!(dialog.text_of("name"), "kept");
}
