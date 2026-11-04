use super::*;
use crate::app::shell::testkit::shell;
use crate::ui::keys::{Action, Keymap};
use crate::ui::overlay::OverlayEvent;
use crate::ui::overlay::palette::PaletteOverlay;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

#[test]
fn controls_keep_active_and_saved_values_separate_and_preserve_draft() {
    let (_home, _project, app, mut shell) = shell();
    shell.composer.set_draft("unfinished prompt");
    shell.status.phase = Phase::Working;
    shell.status.model = "active-model".into();
    shell.status.assistance = Some(false);
    shell.status.main_model_explanation =
        Some("active-provider/active-model: Context promotion from original".into());
    let mut saved = app.config.clone();
    saved.model = "saved-model".into();
    saved.assistance.enabled = true;
    let spec = shell.controls_picker(&saved);
    let assistance = spec
        .items
        .iter()
        .find(|i| i.value == "settings assistance.enabled")
        .unwrap();
    assert!(assistance.detail.contains("Active: off · saved: on"));
    assert!(assistance.detail.contains("not active now"));
    let model = spec.items.iter().find(|i| i.value == "model").unwrap();
    assert!(model.detail.contains("active-model"));
    assert!(model.detail.contains("saved-model"));
    assert!(model.detail.contains("Why this main model (recorded): active-provider/active-model: Context promotion from original"));
    assert!(model.detail.contains("Snapshot at last agent boundary"));
    assert!(
        !spec.items.iter().any(|i| i.value == "plan"),
        "planning must be explicit, not a blind toggle"
    );
    assert_eq!(shell.composer.draft(), "unfinished prompt");
}

#[test]
fn controls_keyboard_selection_routes_to_the_owning_command() {
    let (_home, _project, app, shell) = shell();
    let mut panel = PaletteOverlay::choice(shell.controls_picker(&app.config));
    let keys = Keymap::defaults();
    let event = panel.handle(
        &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &keys,
    );
    assert!(matches!(event, OverlayEvent::Action(action) if action == "command:/approval"));
    assert_eq!(
        Action::from_id("app.status.focus"),
        Some(Action::StatusFocus)
    );
}

#[test]
fn stop_and_undo_explanations_navigate_without_stopping_work_or_changing_drafts() {
    let (_home, project, app, mut shell) = shell();
    let path = project.path().join("keep.txt");
    std::fs::write(&path, "keep these edits").unwrap();
    shell.composer.set_draft("unfinished prompt");
    shell.status.phase = Phase::Working;
    let keys = Keymap::defaults();
    for (command, label) in [
        ("help stop-scopes", "Stop · understand the scope"),
        ("help undo-meanings", "Undo · files, history or settings?"),
    ] {
        let mut spec = shell.controls_picker(&app.config);
        let row = spec.items.iter().find(|row| row.value == command).unwrap();
        assert_eq!(row.label, label);
        // Filter as a user does, then Enter: only read-only help is dispatched.
        spec.items.retain(|row| row.value == command);
        let mut panel = PaletteOverlay::choice(spec);
        let event = panel.handle(
            &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &keys,
        );
        assert!(
            matches!(event, OverlayEvent::Action(action) if action == format!("command:/{command}"))
        );
        assert!(!shell.route_busy_command(&app, format!("/{command}")));
        assert!(shell.startup.is_none());
        assert!(shell.deferred.is_empty());
        assert!(!app.interrupt.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(shell.composer.draft(), "unfinished prompt");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep these edits");
    }
}

#[test]
fn controls_never_infer_a_main_route_receipt_from_saved_selection() {
    let (_home, _project, app, mut shell) = shell();
    shell.status.main_model_explanation = None;
    let spec = shell.controls_picker(&app.config);
    let model = spec
        .items
        .iter()
        .find(|item| item.value == "model")
        .unwrap();
    assert!(
        model
            .detail
            .contains("Why this main model (recorded): unavailable (not recorded)")
    );
}
