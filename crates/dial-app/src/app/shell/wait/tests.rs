use super::*;
use crate::app::overlay::ActiveOverlay;
use crate::app::shell::testkit::shell;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[test]
fn escape_dismisses_completion_before_cancelling_compute_wait() {
    let (_home, _project, app, mut shell) = shell();
    shell.composer.set_busy(true);
    shell.composer.set_draft("/mod");
    assert!(shell.composer.menu_open());
    let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(shell.wait_key(&app, escape).unwrap(), WaitFlow::Continue);
    assert!(!shell.composer.menu_open());
    assert_eq!(shell.composer.draft(), "/mod");
    assert_eq!(shell.wait_key(&app, escape).unwrap(), WaitFlow::Cancel);
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
fn maintenance_wait_dismisses_history_search_then_cancels_when_idle() {
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
    assert_eq!(shell.wait_key(&app, escape).unwrap(), WaitFlow::Cancel);
}
