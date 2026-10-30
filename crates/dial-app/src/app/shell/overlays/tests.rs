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
fn busy_retry_reports_no_wait_or_usage_without_stopping_or_queueing() {
    use crate::llm::provider::RetryControl;
    use crate::ui::transcript::TranscriptCtx;

    for has_control in [false, true] {
        for (command, expected) in [
            ("/retry", "No pending provider retry wait."),
            ("/retry now", "Usage: /retry"),
            ("/retry 120", "Usage: /retry"),
        ] {
            let (_home, _project, app, mut shell) = shell();
            shell.composer.set_busy(true);
            shell.retry_control = has_control.then(RetryControl::default);
            assert!(!shell.route_busy_command(&app, command.into()));
            assert!(shell.deferred.is_empty(), "{command}");
            assert!(shell.startup.is_none(), "{command}");
            let ctx = TranscriptCtx {
                theme: shell.themes.current(),
                ui: &shell.ui,
                tick: 0,
                tools_expanded: false,
                expand_key: "Ctrl+O",
            };
            let text = shell
                .transcript
                .document(100, &ctx)
                .0
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains(expected), "{command}: {text}");
        }
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

#[test]
fn busy_drafter_command_returns_on_lock_contention_and_can_be_retried() {
    let (_home, _project, app, mut shell) = shell();
    let original = app.config.experimental.turn_optimizer;
    let command = if original {
        "/drafter off"
    } else {
        "/drafter on"
    };
    let path = app.layout.config_files()[1].clone();
    let lock = crate::config::settings_store::locked(&path).unwrap();
    let started = std::time::Instant::now();
    assert!(!shell.route_busy_command(&app, command.into()));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert!(shell.pending_config.is_none());
    assert_eq!(app.reload().unwrap().experimental.turn_optimizer, original);
    drop(lock);
    assert!(!shell.route_busy_command(&app, command.into()));
    assert_eq!(
        shell.settings_config(&app).experimental.turn_optimizer,
        !original
    );
    assert_eq!(app.reload().unwrap().experimental.turn_optimizer, !original);
    assert!(!shell.route_busy_command(&app, "/drafter".into()));
    assert!(
        shell.pending_config.is_none(),
        "toggle uses the latest saved state"
    );
    assert!(shell.startup.is_none());
    assert!(shell.deferred.is_empty());
}

#[test]
fn image_settings_open_during_a_turn_and_save_a_per_model_entry() {
    let (_home, _project, app, mut shell) = shell();
    assert!(!shell.route_busy_command(&app, "/image settings".into()));
    assert!(shell.startup.is_none() && shell.deferred.is_empty());
    if let Some(state) = shell.busy_overlay.as_ref() {
        assert!(matches!(state.overlay, ActiveOverlay::ImageSettings(_)));
        shell.finish_busy_overlay(&app).unwrap();
    }

    // What the panel sends: the whole entry under a bare key derived from the model id.
    let (key, mut entry) = app.config.image.entry_for("org/flux-turbo");
    entry.steps = Some(4);
    entry.loras = vec![crate::config::image::ImageLora {
        path: "style.safetensors".into(),
        scale: 1.25,
    }];
    let change = |entry: &crate::config::image::ImageModelSettings| Change::Set {
        path: format!("image.models.{key}"),
        value: toml::Value::try_from(entry).unwrap(),
    };
    shell
        .save(&app, None, &[change(&entry)], Scope::Project)
        .unwrap();
    let saved = app.reload().unwrap();
    assert_eq!(saved.image.settings_for("org/flux-turbo"), Some(&entry));
    assert_eq!(saved.image.entry_for("org/flux-turbo").0, key);
    assert_eq!(saved.model_profile, app.config.model_profile);

    entry.steps = Some(0);
    assert!(
        shell
            .save(&app, None, &[change(&entry)], Scope::Project)
            .is_err()
    );
    assert_eq!(
        app.reload()
            .unwrap()
            .image
            .settings_for("org/flux-turbo")
            .unwrap()
            .steps,
        Some(4)
    );
}
