use super::*;
use crate::app::overlay::ActiveOverlay;
use crate::app::shell::testkit::shell;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[test]
fn escape_dismisses_completion_but_only_interrupt_cancels_compute_wait() {
    let (_home, _project, app, mut shell) = shell();
    shell.composer.set_busy(true);
    shell.composer.set_draft("/mod");
    assert!(shell.composer.menu_open());
    let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(shell.wait_key(&app, escape).unwrap(), WaitFlow::Continue);
    assert!(!shell.composer.menu_open());
    assert_eq!(shell.composer.draft(), "/mod");
    assert_eq!(shell.wait_key(&app, escape).unwrap(), WaitFlow::Continue);
    assert_eq!(
        shell
            .wait_key(
                &app,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            )
            .unwrap(),
        WaitFlow::Cancel
    );
    assert!(shell.startup.is_none());
}

#[test]
fn wait_preserves_send_choice_and_explicit_interrupt_precedence() {
    let (_home, _project, app, mut shell) = shell();
    shell.composer.set_busy(true);
    shell.composer.set_draft("Keep the API stable");
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(shell.wait_key(&app, enter).unwrap(), WaitFlow::Continue);
    assert!(shell.composer.choice_open());
    assert_eq!(
        shell
            .wait_key(&app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap(),
        WaitFlow::Continue
    );
    assert!(!shell.composer.choice_open());
    assert_eq!(shell.composer.draft(), "Keep the API stable");
    assert_eq!(
        shell
            .wait_key(
                &app,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            )
            .unwrap(),
        WaitFlow::Cancel
    );
    assert_eq!(shell.composer.draft(), "Keep the API stable");
}

#[test]
fn provider_shortcut_opens_live_panel_while_waiting() {
    let (_home, _project, app, mut shell) = shell();
    shell.composer.set_busy(true);
    shell.keys.rebind(Action::ProviderSetup, &["F6"]).unwrap();
    assert_eq!(
        shell
            .wait_key(&app, KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE))
            .unwrap(),
        WaitFlow::Continue
    );
    assert!(matches!(
        shell.busy_overlay.as_ref().map(|state| &state.overlay),
        Some(ActiveOverlay::Providers(_))
    ));
    assert!(shell.startup.is_none());
    assert!(shell.deferred.is_empty());
    shell.finish_busy_overlay(&app).unwrap();
}

#[test]
fn wait_display_controls_work_now_without_discarding_draft() {
    let (_home, _project, app, mut shell) = shell();
    shell.composer.set_busy(true);
    shell.composer.set_draft("Unsent direction");
    let controls = [
        Action::ToolsToggle,
        Action::ThinkingToggle,
        Action::StatusExpand,
        Action::ToolsExpand,
        Action::DisplayReset,
    ];
    let before = (
        shell.ui.show_tools,
        shell.ui.show_thinking,
        shell.tools_expanded,
    );
    for action in controls {
        shell.keys.rebind(action, &["F6"]).unwrap();
        assert_eq!(
            shell
                .wait_key(&app, KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE))
                .unwrap(),
            WaitFlow::Continue
        );
        shell.keys.rebind(action, &[]).unwrap();
        assert_eq!(shell.composer.draft(), "Unsent direction");
        assert!(shell.startup.is_none());
        assert!(shell.deferred.is_empty());
    }
    assert_eq!(
        (
            shell.ui.show_tools,
            shell.ui.show_thinking,
            shell.tools_expanded
        ),
        (!before.0, !before.1, !before.2)
    );
}

#[test]
fn maintenance_wait_dismisses_history_search_but_escape_never_cancels() {
    let (_home, _project, app, mut shell) = shell();
    shell.keys.rebind(Action::HistorySearch, &["F6"]).unwrap();
    assert_eq!(
        shell
            .wait_key(&app, KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE))
            .unwrap(),
        WaitFlow::Continue
    );
    let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(shell.wait_key(&app, escape).unwrap(), WaitFlow::Continue);
    assert_eq!(shell.wait_key(&app, escape).unwrap(), WaitFlow::Continue);
    assert_eq!(
        shell
            .wait_key(
                &app,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            )
            .unwrap(),
        WaitFlow::Cancel
    );
}

#[test]
fn first_stop_key_closes_a_wait_panel_and_only_the_next_cancels() {
    let (_home, _project, app, mut shell) = shell();
    shell.composer.set_busy(true);
    shell.keys.rebind(Action::ProviderSetup, &["F6"]).unwrap();
    let f6 = KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE);
    assert_eq!(shell.wait_key(&app, f6).unwrap(), WaitFlow::Continue);
    assert!(shell.busy_overlay.is_some());
    let stop = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(
        shell.wait_event(&app, stop.clone()).unwrap(),
        WaitFlow::Continue
    );
    assert!(shell.busy_overlay.is_none());
    assert_eq!(shell.wait_event(&app, stop).unwrap(), WaitFlow::Cancel);
}

#[test]
fn busy_shortcuts_answer_instead_of_being_ignored_during_a_wait() {
    let (_home, _project, app, mut shell) = shell();
    shell.composer.set_busy(true);
    let f6 = KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE);
    for action in [Action::EditExternal, Action::RateGood, Action::RateBad] {
        shell.keys.rebind(action, &["F6"]).unwrap();
        assert_eq!(shell.wait_key(&app, f6).unwrap(), WaitFlow::Continue);
        shell.keys.rebind(action, &[]).unwrap();
    }
    // The external editor follows the turn rule: deferred until the work finishes.
    assert!(shell.deferred_editor);
    let ctx = crate::ui::transcript::TranscriptCtx {
        theme: shell.themes.current(),
        ui: &shell.ui,
        tick: 0,
        tools_expanded: false,
        expand_key: "expand",
    };
    let notes = shell
        .transcript
        .document(200, &ctx)
        .0
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(notes.contains("External editor opens after the current turn."));
    assert!(notes.contains("Rating is available after the turn"));
    // With no workers yet, the recent-worker key opens the workers hub as a live panel.
    shell.keys.rebind(Action::SubagentsRecent, &["F6"]).unwrap();
    assert_eq!(shell.wait_key(&app, f6).unwrap(), WaitFlow::Continue);
    assert!(shell.busy_overlay.is_some());
    assert!(shell.startup.is_none());
    shell.finish_busy_overlay(&app).unwrap();
}
