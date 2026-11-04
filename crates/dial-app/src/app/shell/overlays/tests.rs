use super::*;
use crate::app::shell::testkit::shell;
use crate::config::settings_store::{Change, Scope};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;

#[test]
fn unavailable_busy_commands_explain_without_dispatching_or_queueing() {
    let (_home, _project, app, mut shell) = shell();
    shell.git.repository.value = Some(false);
    for (command, reason) in [
        ("/commit", "needs a git repository"),
        ("/watch", "needs an active session"),
        ("/models restart", "needs an idle session"),
    ] {
        assert!(!shell.route_busy_command(&app, command.into()));
        assert!(shell.startup.is_none() && shell.deferred.is_empty());
        let ctx = crate::ui::transcript::TranscriptCtx {
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
        assert!(text.contains(reason), "{command}: {text}");
    }
}

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

#[test]
fn model_settings_inheritance_keeps_other_edit_pins_and_the_original_config() {
    use crate::config::types::{EditMode, EndpointConfig};
    use crate::llm::profiles::ModelProfile;

    for (route, settings, process, expected) in [
        ("local", "", None, "replace"),
        ("hosted", "", None, "hashline"),
        (
            "local",
            "[assistance.local]\nenabled = false",
            None,
            "hashline",
        ),
        (
            "local",
            "[harness]\nedit_mode = 'apply_patch'",
            None,
            "apply_patch",
        ),
        (
            "local",
            "[edit.model_variants]\n'mlx/model' = 'patch'",
            None,
            "patch",
        ),
        (
            "local",
            "[harness]\nedit_mode = 'apply_patch'",
            Some(EditMode::Sloppy),
            "sloppy",
        ),
    ] {
        let mut config: Config = toml::from_str(settings).unwrap();
        config.provider = Some("mlx".into());
        config.model = "model".into();
        if route == "hosted" {
            config.provider = Some("openai".into());
            config.endpoint = Some(EndpointConfig {
                base_url: "https://api.openai.com/v1".into(),
                api_key_env: None,
                allow_insecure: false,
                protocol: None,
                headers: None,
                account_id: None,
            });
        }
        config.edit_mode_override = process;
        config.model_profile = Some("edited".into());
        config.model_profiles.insert(
            "edited".into(),
            ModelProfile {
                provider: config.provider.clone().unwrap(),
                model: config.model.clone(),
                edit_mode: Some(EditMode::Hashline),
                ..Default::default()
            },
        );
        assert_eq!(
            model_settings_inherited_edit_mode(&config, "edited", &config.harness).unwrap(),
            expected,
            "{route}: {settings} with process {process:?}",
        );
        assert_eq!(
            config.active_model_profile().unwrap().edit_mode,
            Some(EditMode::Hashline)
        );
    }
}

#[test]
fn model_settings_reset_uses_the_raw_stored_harness_and_preserves_pins() {
    use crate::config::types::{EditMode, Harness};
    use crate::llm::profiles::ModelProfile;
    use crate::storage::store::Store;

    let state = tempfile::tempdir().unwrap();
    let stored = Harness {
        edit_mode: Some(EditMode::ApplyPatch),
        ..Default::default()
    };
    let store = Store::open(state.path(), "model", "coding", &stored).unwrap();
    for (settings, process, expected) in [
        ("", None, "apply_patch"),
        (
            "[edit.model_variants]\n'mlx/model' = 'patch'",
            None,
            "apply_patch",
        ),
        ("[harness]\nedit_mode = 'replace'", None, "replace"),
        (
            "[harness]\nedit_mode = 'replace'",
            Some(EditMode::Sloppy),
            "sloppy",
        ),
    ] {
        let mut config: Config = toml::from_str(settings).unwrap();
        config.provider = Some("mlx".into());
        config.model = "model".into();
        config.edit_mode_override = process;
        config.model_profile = Some("edited".into());
        config.model_profiles.insert(
            "edited".into(),
            ModelProfile {
                provider: "mlx".into(),
                model: "model".into(),
                edit_mode: Some(EditMode::Hashline),
                ..Default::default()
            },
        );
        let original = serde_json::to_value(&config).unwrap();
        let raw = store.active().unwrap().harness;
        // Agent.version is effective already, so using it here would retain this profile pin.
        assert_eq!(
            config.effective_harness(&raw).unwrap().edit_mode,
            Some(EditMode::Hashline)
        );
        assert_eq!(
            model_settings_inherited_edit_mode(&config, "edited", &raw).unwrap(),
            expected,
            "{settings} with process {process:?}",
        );
        assert_eq!(serde_json::to_value(&config).unwrap(), original);
        assert_eq!(config.edit_mode_override, process);
        assert_eq!(store.active().unwrap().harness, stored);

        config.model_profiles.get_mut("edited").unwrap().temperature = Some(3.0);
        assert!(model_settings_inherited_edit_mode(&config, "edited", &raw).is_err());
    }
}

#[test]
fn model_settings_store_errors_are_not_replaced_with_config_inheritance() {
    use crate::config::types::Harness;
    use crate::storage::store::Store;

    let (_home, _project, mut app, mut shell) = shell();
    app.overlay = toml::from_str(
        "model = 'settings-test-model'\nprovider = 'mlx'\n[improve]\nteach = true\nenabled = false",
    )
    .unwrap();
    *app.models.borrow_mut() = None;
    let effective = app.reload().unwrap();
    // A binding failure is observable instead of presenting the config's unrelated grammar.
    let _wrong_route = Store::open(
        &effective.state_dir,
        "different-model",
        &effective.use_case,
        &Harness::default(),
    )
    .unwrap();
    let error = shell
        .make_overlay(&app, Which::ModelSettings)
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("different model or use_case"),
        "{error:#}"
    );

    // With teaching off the runtime has no store, even if one exists at that path.
    app.overlay
        .get_mut("improve")
        .unwrap()
        .as_table_mut()
        .unwrap()
        .insert("teach".into(), false.into());
    assert!(shell.make_overlay(&app, Which::ModelSettings).is_ok());
}

/// Writer whose bytes the test can read back after the shell has painted.
#[derive(Clone, Default)]
struct Captured(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// An inline approval prompt that arrives while a busy menu owns the alternate screen must leave
/// that screen (the prompt is inline), keep the menu state and draft, and bring the menu back
/// before its next redraw.
#[test]
fn inline_prompt_during_busy_menu_leaves_alt_screen_and_menu_recovers() {
    const ALT_ENTER: &str = "\x1b[?1049h";
    const ALT_LEAVE: &str = "\x1b[?1049l";
    let (_home, _project, app, mut shell) = shell();
    let out = Captured::default();
    shell.term = crate::ui::term::with_writer(Box::new(out.clone()), (100, 30));
    shell.start_busy_overlay(&app, Which::Settings).unwrap();
    shell.step_busy_overlay(&app, None).unwrap();
    shell.composer.set_draft("keep this draft");

    let mark = out.0.borrow().len();
    shell
        .paint(Some(vec![Line::from("APPROVAL PROMPT")]))
        .unwrap();
    let first = String::from_utf8_lossy(&out.0.borrow()[mark..]).into_owned();
    assert!(
        first.contains(ALT_LEAVE),
        "inline prompt must leave the alternate screen: {first:?}"
    );
    assert!(first.contains("APPROVAL PROMPT"), "{first:?}");
    assert!(
        matches!(
            shell.busy_overlay.as_ref().map(|state| &state.overlay),
            Some(ActiveOverlay::Settings(_))
        ),
        "the busy menu state must survive the prompt"
    );
    assert_eq!(shell.composer.draft(), "keep this draft");

    let mark = out.0.borrow().len();
    shell
        .paint(Some(vec![Line::from("APPROVAL PROMPT 2")]))
        .unwrap();
    let second = String::from_utf8_lossy(&out.0.borrow()[mark..]).into_owned();
    assert!(
        !second.contains(ALT_ENTER) && !second.contains(ALT_LEAVE),
        "a second inline frame must not toggle the screen again: {second:?}"
    );

    let mark = out.0.borrow().len();
    shell.step_busy_overlay(&app, None).unwrap();
    let reopened = String::from_utf8_lossy(&out.0.borrow()[mark..]).into_owned();
    assert!(
        reopened.contains(ALT_ENTER),
        "the menu must reopen before its redraw: {reopened:?}"
    );
    assert!(shell.busy_overlay.is_some());

    assert!(
        !shell
            .step_busy_overlay(
                &app,
                Some(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))),
            )
            .unwrap(),
        "closing a menu must not interrupt the turn"
    );
    assert!(shell.busy_overlay.is_none(), "Esc must close the menu");
    assert_eq!(shell.composer.draft(), "keep this draft");
}

#[test]
fn busy_back_keeps_settings_search_but_refreshes_saved_values() {
    use crate::ui::overlay::OverlayEvent;
    let (_home, _project, app, mut shell) = shell();
    shell.composer.set_draft("keep the composer draft");
    shell.start_busy_overlay(&app, Which::Settings).unwrap();
    let state = shell.busy_overlay.as_mut().unwrap();
    let ActiveOverlay::Settings(settings) = &mut state.overlay else {
        panic!("settings")
    };
    assert!(settings.focus_path("ui.tips"));
    for c in "tips".chars() {
        settings.handle(
            &Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            &shell.keys,
        );
    }
    state.next = Some("open:models".into());
    shell.finish_busy_overlay(&app).unwrap();
    assert_eq!(shell.busy_overlay.as_ref().unwrap().history.len(), 1);
    app.save_settings(
        Scope::Global,
        &[Change::Set {
            path: "ui.tips".into(),
            value: toml::Value::Boolean(false),
        }],
        None,
    )
    .unwrap();
    shell.busy_overlay.as_mut().unwrap().next = Some("navigate:back".into());
    shell.finish_busy_overlay(&app).unwrap();
    let state = shell.busy_overlay.as_mut().unwrap();
    assert!(state.history.is_empty());
    let ActiveOverlay::Settings(settings) = &mut state.overlay else {
        panic!("retained settings")
    };
    assert!(matches!(
        settings.handle(
            &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &shell.keys
        ),
        OverlayEvent::None
    ));
    settings.handle(
        &Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &shell.keys,
    );
    let event = settings.handle(
        &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &shell.keys,
    );
    assert!(
        matches!(event, OverlayEvent::Set { ref path, value: toml::Value::Boolean(true), .. } if path == "ui.tips"),
        "{event:?}"
    );
    assert_eq!(shell.composer.draft(), "keep the composer draft");
}

#[test]
fn settings_model_review_returns_to_retained_editor_after_save_or_cancel() {
    use crate::llm::model_config::ModelSelection;
    use crate::ui::overlay::settings::{ChoiceAction, ValueChoice};
    fn press(shell: &mut Shell, app: &App, code: KeyCode) {
        assert!(
            !shell
                .step_busy_overlay(
                    app,
                    Some(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
                )
                .unwrap()
        );
    }
    for save in [false, true] {
        let (_home, _project, app, mut shell) = shell();
        let before = app.reload().unwrap().selection();
        shell.composer.set_draft("keep draft through model review");
        shell.start_busy_overlay(&app, Which::Settings).unwrap();
        let ActiveOverlay::Settings(settings) = &mut shell.busy_overlay.as_mut().unwrap().overlay
        else {
            panic!("settings")
        };
        assert!(settings.focus_path("model"));
        settings.set_value_choices(std::collections::BTreeMap::from([(
            "model".into(),
            vec![ValueChoice {
                value: "gpt-4.1".into(),
                label: "gpt-4.1".into(),
                action: ChoiceAction::Assign {
                    role: "main".into(),
                    selection: ModelSelection::new("openai", "gpt-4.1"),
                },
            }],
        )]));
        press(&mut shell, &app, KeyCode::Enter);
        for c in "gpt-4.1".chars() {
            press(&mut shell, &app, KeyCode::Char(c));
        }
        press(&mut shell, &app, KeyCode::Enter);
        let state = shell.busy_overlay.as_ref().unwrap();
        assert!(
            matches!(state.overlay, ActiveOverlay::Models(_)),
            "selection must be reviewed, never saved directly"
        );
        assert!(matches!(
            state.history.last(),
            Some(ActiveOverlay::Settings(_))
        ));
        assert_eq!(
            app.reload().unwrap().selection(),
            before,
            "review must not publish"
        );
        shell.step_busy_overlay(&app, None).unwrap();
        if save {
            press(&mut shell, &app, KeyCode::Enter);
            assert_eq!(app.reload().unwrap().selection().model, "gpt-4.1");
        } else {
            press(&mut shell, &app, KeyCode::Esc);
            shell
                .step_busy_overlay(
                    &app,
                    Some(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT))),
                )
                .unwrap();
            assert_eq!(app.reload().unwrap().selection(), before);
        }
        let state = shell.busy_overlay.as_mut().unwrap();
        assert!(state.history.is_empty());
        let ActiveOverlay::Settings(settings) = &mut state.overlay else {
            panic!("return to originating Settings, not an unrelated Models browser")
        };
        // The original model row remains selected, rather than resetting the category/editor.
        assert!(matches!(
            settings.handle(
                &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                &shell.keys
            ),
            crate::ui::overlay::OverlayEvent::None
        ));
        assert_eq!(shell.composer.draft(), "keep draft through model review");
    }
}

#[test]
fn reviewed_background_model_completion_returns_to_settings_without_an_extra_key() {
    use crate::app::overlay::auth::{AuthJob, PreparedModel};
    use crate::config::settings_store;
    use crate::llm::model_config::ModelSelection;
    use crate::ui::overlay::models::ModelOverlay;
    for success in [false, true] {
        let (_home, _project, app, mut shell) = shell();
        shell.start_busy_overlay(&app, Which::Settings).unwrap();
        let changes = crate::config::selection::changes(
            &app.config,
            &ModelSelection::new("openai", "gpt-4.1"),
            "main",
        )
        .unwrap();
        let review =
            settings_store::review_model_config(&app.layout, Scope::Global, &changes, Some("main"))
                .unwrap();
        let state = shell.busy_overlay.as_mut().unwrap();
        let origin = std::mem::replace(
            &mut state.overlay,
            ActiveOverlay::Models(Box::new(ModelOverlay::new(
                None,
                app.config.role_statuses(),
                Scope::Global,
            ))),
        );
        state.history.push(origin);
        let before = app
            .layout
            .config_files()
            .map(|path| std::fs::read(path).ok());
        let (send, receiver) = std::sync::mpsc::channel();
        shell.auth_job = Some(AuthJob {
            label: "Model",
            receiver,
            lines: None,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            model: Some(PreparedModel {
                changes,
                role: Some("main".into()),
                scope: Scope::Global,
                voice: None,
                review: Some(review),
            }),
        });
        shell.step_busy_overlay(&app, None).unwrap();
        assert!(matches!(
            shell.busy_overlay.as_ref().unwrap().overlay,
            ActiveOverlay::Models(_)
        ));
        send.send(if success {
            Ok("prepared".into())
        } else {
            Err(anyhow::anyhow!("preparation failed"))
        })
        .unwrap();
        shell.step_busy_overlay(&app, None).unwrap();
        let state = shell.busy_overlay.as_ref().unwrap();
        if success {
            assert!(matches!(state.overlay, ActiveOverlay::Settings(_)));
            assert!(state.history.is_empty());
            assert_eq!(app.reload().unwrap().selection().model, "gpt-4.1");
        } else {
            assert!(
                matches!(state.overlay, ActiveOverlay::Models(_)),
                "errors must stay inspectable, not silently leave the review"
            );
            assert_eq!(
                app.layout
                    .config_files()
                    .map(|path| std::fs::read(path).ok()),
                before
            );
        }
    }
}

#[test]
fn memory_model_picker_links_keep_the_settings_origin_and_draft() {
    for role in ["memory", "embedding"] {
        let (_home, _project, app, mut shell) = shell();
        shell.composer.set_draft("retain memory draft");
        shell.start_busy_overlay(&app, Which::Settings).unwrap();
        let ActiveOverlay::Settings(settings) = &mut shell.busy_overlay.as_mut().unwrap().overlay
        else {
            panic!("Settings")
        };
        assert!(settings.focus_path("memory.backend"));
        shell.busy_overlay.as_mut().unwrap().next = Some(format!("open:role:{role}"));
        shell.finish_busy_overlay(&app).unwrap();
        assert!(matches!(
            shell.busy_overlay.as_ref().unwrap().overlay,
            ActiveOverlay::Models(_)
        ));
        shell
            .step_busy_overlay(
                &app,
                Some(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT))),
            )
            .unwrap();
        assert!(matches!(
            shell.busy_overlay.as_ref().unwrap().overlay,
            ActiveOverlay::Settings(_)
        ));
        assert_eq!(shell.composer.draft(), "retain memory draft");
    }
    assert!(overlay_destination("open:role:not-a-role").is_none());
}

#[test]
fn models_and_activity_keep_the_same_session_download_owner_between_views() {
    use crate::ui::overlay::models::ModelDownloads;
    let (_home, _project, app, mut shell) = shell();
    let manager = ModelDownloads::idle().into_shared();
    shell.model_downloads = Some(manager.clone());
    for destination in [Which::Models, Which::ModelDownloads] {
        shell.start_busy_overlay(&app, destination).unwrap();
        assert!(std::rc::Rc::ptr_eq(
            shell.model_downloads.as_ref().unwrap(),
            &manager
        ));
        assert_eq!(
            std::rc::Rc::strong_count(&manager),
            3,
            "session, test observer and visible model view share one owner"
        );
        shell.finish_busy_overlay(&app).unwrap();
        assert_eq!(
            std::rc::Rc::strong_count(&manager),
            2,
            "closing a view must retain the session manager"
        );
    }
    drop(shell);
    assert_eq!(std::rc::Rc::strong_count(&manager), 1);
}

#[test]
fn settings_pending_rows_compare_active_app_not_reloaded_saved_config() {
    let (_home, _project, mut app, mut shell) = shell();
    shell.status.phase = Phase::Working;
    let original = app.config.approval;
    let saved = if original == crate::config::types::Approval::Auto {
        "ask"
    } else {
        "auto"
    };
    app.save_settings(
        Scope::Global,
        &[Change::Set {
            path: "approval".into(),
            value: saved.into(),
        }],
        None,
    )
    .unwrap();
    shell.refresh_resources(&app, true, true);
    assert_ne!(shell.resources.reload.config().approval, original);
    let mut overlay = shell.make_overlay(&app, Which::Settings).unwrap();
    let ActiveOverlay::Settings(settings) = &mut overlay else {
        panic!("Settings");
    };
    assert!(settings.focus_path("approval"));
    assert!(settings.pending_application_count() > 0);
    assert_eq!(
        app.config.approval, original,
        "a saved preference never mutates the borrowed active config"
    );
    let theme = shell.themes.current();
    let ctx = crate::ui::overlay::DrawCtx {
        theme,
        keys: &shell.keys,
        status: &shell.status,
        preview_status: &|_, _| Line::default(),
    };
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(180, 30)).unwrap();
    terminal.draw(|frame| settings.draw(frame, &ctx)).unwrap();
    let shown = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        shown.contains("Active ") && shown.contains("Saved ") && shown.contains("after this turn"),
        "{shown}"
    );
    app.adopt(app.reload().unwrap());
    shell.status.phase = Phase::Idle;
    crate::app::overlay::env::refresh_navigation(&app, &shell.catalog, &mut overlay, false);
    let ActiveOverlay::Settings(settings) = &overlay else {
        panic!("Settings");
    };
    assert_eq!(
        settings.pending_application_count(),
        0,
        "adoption clears pending application"
    );
    // The adopted row no longer contributes to pending application.
    let active = app.config.settings_values().unwrap();
    assert_eq!(
        crate::config::schema::get(&active, "approval").and_then(toml::Value::as_str),
        Some(saved)
    );
    let ctx = crate::ui::overlay::DrawCtx {
        theme: shell.themes.current(),
        keys: &shell.keys,
        status: &shell.status,
        preview_status: &|_, _| Line::default(),
    };
    let _ = settings;
    terminal.draw(|frame| overlay.draw(frame, &ctx)).unwrap();
    let shown = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!shown.contains("after this turn"), "{shown}");
}
