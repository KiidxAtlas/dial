use super::*;
use crate::app::shell::testkit::shell;
use crate::config::settings_store::{Change, Scope};

#[test]
fn busy_help_and_shortcuts_open_editable_panels_without_stopping() {
    let (_home, _project, app, mut shell) = shell();
    for command in ["/hotkeys", "/help", "/?"] {
        assert!(!shell.route_busy_command(&app, command.into()));
        let overlay = &shell.busy_overlay.as_ref().unwrap().overlay;
        assert!(match command {
            "/hotkeys" => matches!(overlay, ActiveOverlay::Shortcuts(_)),
            _ => matches!(overlay, ActiveOverlay::Palette(_)),
        });
        assert!(shell.startup.is_none());
        assert!(shell.deferred.is_empty());
        shell.finish_busy_overlay(&app).unwrap();
    }
}

#[test]
fn busy_provider_test_destination_reopens_providers_instead_of_disappearing() {
    let (_home, _project, app, mut shell) = shell();
    shell.start_busy_overlay(&app, Which::Providers).unwrap();
    shell.busy_overlay.as_mut().unwrap().next = Some("test:fixture".into());
    assert!(!shell.finish_busy_overlay(&app).unwrap());
    assert!(matches!(
        shell.busy_overlay.as_ref().map(|state| &state.overlay),
        Some(ActiveOverlay::Providers(_))
    ));
    assert!(shell.startup.is_none());
    assert!(shell.deferred.is_empty());
    shell.finish_busy_overlay(&app).unwrap();
}

#[test]
fn busy_settings_apply_display_now_and_stage_agent_changes() {
    let (_home, _project, app, mut shell) = shell();
    shell.start_busy_overlay(&app, Which::Settings).unwrap();
    let mut fresh = app.config.clone();
    fresh.ui.show_tools = !fresh.ui.show_tools;
    fresh.ui.history_limit = 17;
    fresh.experimental.turn_optimizer = !fresh.experimental.turn_optimizer;
    shell.busy_overlay.as_mut().unwrap().latest = Some(fresh.clone());
    shell.finish_busy_overlay(&app).unwrap();
    assert_eq!(shell.ui.show_tools, fresh.ui.show_tools);
    assert_eq!(shell.ui.history_limit, 17);
    assert_eq!(
        shell.settings_config(&app).experimental.turn_optimizer,
        fresh.experimental.turn_optimizer
    );
    assert_ne!(
        app.config.experimental.turn_optimizer,
        fresh.experimental.turn_optimizer
    );
    assert!(shell.pending_config.is_some());
}

#[test]
fn malformed_live_worker_commands_do_not_silently_queue() {
    let (_home, _project, app, mut shell) = shell();
    for command in ["/message", "/message worker-only", "/cancel"] {
        assert!(!shell.route_busy_command(&app, command.into()));
        assert!(shell.deferred.is_empty(), "{command}");
        assert!(shell.startup.is_none());
    }
}

#[test]
fn busy_settings_command_can_be_reverted_before_agent_adoption() {
    let (_home, _project, app, mut shell) = shell();
    let original = app.config.ui.show_tools;
    for value in [!original, original] {
        shell
            .save(
                &app,
                None,
                &[Change::Set {
                    path: "ui.show_tools".into(),
                    value: value.into(),
                }],
                Scope::Project,
            )
            .unwrap();
        assert_eq!(shell.ui.show_tools, value);
    }
    assert!(shell.pending_config.is_none());
}
