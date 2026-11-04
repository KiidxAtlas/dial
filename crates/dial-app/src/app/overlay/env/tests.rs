use super::*;

fn app() -> (tempfile::TempDir, tempfile::TempDir, App) {
    let (home, project, layout, config) = crate::app::tests::config();
    let app = App::new(
        layout,
        config,
        toml::Table::new(),
        Default::default(),
        vec![],
        Default::default(),
    )
    .unwrap();
    (home, project, app)
}

fn seed(app: &App, scope: Scope, entries: &[(&str, toml::Value)]) {
    app.save_settings(
        scope,
        &entries
            .iter()
            .map(|(path, value)| Change::Set {
                path: (*path).into(),
                value: value.clone(),
            })
            .collect::<Vec<_>>(),
        None,
    )
    .unwrap();
}

fn reviewed(app: &App, scope: Scope, paths: &[&str]) -> BTreeMap<String, toml::Value> {
    let layer = settings_store::load_layer(&app.layout, scope).unwrap();
    paths
        .iter()
        .map(|path| {
            (
                (*path).into(),
                crate::config::schema::get(&layer, path).unwrap().clone(),
            )
        })
        .collect()
}

fn reset(app: &App, scope: Scope, expected: &BTreeMap<String, toml::Value>) -> Result<()> {
    save_reviewed_reset(
        app,
        &expected.keys().cloned().collect::<Vec<_>>(),
        scope,
        expected,
    )
}

fn files(app: &App) -> Vec<Option<Vec<u8>>> {
    app.layout
        .config_files()
        .iter()
        .map(|path| std::fs::read(path).ok())
        .collect()
}

#[test]
fn reviewed_reset_removes_exact_paths_only_in_selected_layer() {
    for scope in [Scope::Global, Scope::Project] {
        let (_home, _project, app) = app();
        for layer in [Scope::Global, Scope::Project] {
            seed(
                &app,
                layer,
                &[
                    ("ui.notify", false.into()),
                    ("ui.notify_errors", false.into()),
                    ("ui.theme", "default".into()),
                ],
            );
        }
        let expected = reviewed(&app, scope, &["ui.notify", "ui.notify_errors"]);
        let other = if scope == Scope::Global {
            Scope::Project
        } else {
            Scope::Global
        };
        let before_other = settings_store::load_layer(&app.layout, other).unwrap();
        reset(&app, scope, &expected).unwrap();
        let after = settings_store::load_layer(&app.layout, scope).unwrap();
        assert!(crate::config::schema::get(&after, "ui.notify").is_none());
        assert!(crate::config::schema::get(&after, "ui.notify_errors").is_none());
        assert_eq!(
            crate::config::schema::get(&after, "ui.theme").and_then(toml::Value::as_str),
            Some("default")
        );
        assert_eq!(
            settings_store::load_layer(&app.layout, other).unwrap(),
            before_other
        );
    }
}

#[test]
fn reviewed_reset_refuses_stale_values_and_invalid_path_sets_without_partial_writes() {
    let (_home, _project, app) = app();
    seed(
        &app,
        Scope::Project,
        &[
            ("ui.notify", false.into()),
            ("ui.notify_errors", false.into()),
        ],
    );
    let expected = reviewed(&app, Scope::Project, &["ui.notify", "ui.notify_errors"]);
    seed(&app, Scope::Project, &[("ui.notify_errors", true.into())]);
    let before = files(&app);
    assert!(
        reset(&app, Scope::Project, &expected)
            .unwrap_err()
            .to_string()
            .contains("changed")
    );
    assert_eq!(files(&app), before);

    let expected = reviewed(&app, Scope::Project, &["ui.notify", "ui.notify_errors"]);
    for paths in [
        vec![],
        vec!["ui.notify"],
        vec!["ui.notify", "ui.notify", "ui.notify_errors"],
        vec!["ui.notify", "unknown"],
    ] {
        assert!(
            save_reviewed_reset(
                &app,
                &paths.iter().map(|s| (*s).into()).collect::<Vec<_>>(),
                Scope::Project,
                &expected
            )
            .is_err()
        );
        assert_eq!(files(&app), before);
    }
    for path in ["unknown", "ui", "harness"] {
        let invalid = BTreeMap::from([(path.to_owned(), toml::Value::Table(toml::Table::new()))]);
        assert!(reset(&app, Scope::Project, &invalid).is_err());
        assert_eq!(files(&app), before);
    }
}

#[test]
fn reviewed_reset_requires_every_stored_embedding_partner_in_review() {
    let (_home, _project, app) = app();
    let model = "memory.semantic.embedding_model";
    let url = "memory.semantic.embedding_base_url";
    seed(
        &app,
        Scope::Project,
        &[
            (model, "test-embedding".into()),
            (url, "http://localhost:11434/v1".into()),
            ("ui.notify", false.into()),
        ],
    );
    let before = files(&app);
    for path in [model, url] {
        let expected = reviewed(&app, Scope::Project, &["ui.notify", path]);
        assert!(reset(&app, Scope::Project, &expected).is_err());
        assert_eq!(files(&app), before);
    }
    let expected = reviewed(&app, Scope::Project, &["ui.notify", model, url]);
    reset(&app, Scope::Project, &expected).unwrap();
    let after = settings_store::load_layer(&app.layout, Scope::Project).unwrap();
    for path in [model, url, "ui.notify"] {
        assert!(crate::config::schema::get(&after, path).is_none());
    }
}

#[test]
fn reviewed_reset_failed_validation_preserves_both_files() {
    let (_home, _project, app) = app();
    seed(
        &app,
        Scope::Global,
        &[
            ("ui.notify", false.into()),
            ("ui.notify_errors", false.into()),
        ],
    );
    let expected = reviewed(&app, Scope::Global, &["ui.notify", "ui.notify_errors"]);
    let project_path = &app.layout.config_files()[1];
    std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
    std::fs::write(project_path, "[ui\nmalformed").unwrap();
    let before = files(&app);
    assert!(reset(&app, Scope::Global, &expected).is_err());
    assert_eq!(files(&app), before);
}

#[test]
fn bulk_event_requires_frozen_review_and_settles_only_successful_save() {
    use crate::ui::overlay::OverlayEvent;
    use crate::ui::overlay::settings::SettingsOverlay;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    for case in [
        "missing",
        "unconfirmed",
        "scope",
        "setup",
        "auth",
        "stale",
        "success",
    ] {
        let (_home, _project, app) = app();
        seed(&app, Scope::Project, &[("ui.notify", false.into())]);
        let fresh = app.reload().unwrap();
        let (values, global, project) = settings_tables(&app, &fresh).unwrap();
        let mut settings = SettingsOverlay::new(values, project, global, Scope::Project, vec![]);
        assert!(settings.focus_path("ui.notify"));
        let mut keys = Keymap::load(&app.layout.user_dir).0;
        let mut themes = ThemeManager::new(&app.layout.user_dir, &fresh.ui, None);
        let mut catalog = Catalog::new();
        let runtime = crate::runtime::Tree::default();
        let status = StatusData::default();
        let mut auth_job = None;
        let mut extensions_pending = None;
        let mut extensions_job = None;
        let mut scope = Scope::Project;
        if case != "missing" {
            settings.handle(
                &Event::Key(KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL)),
                &keys,
            );
            assert_eq!(settings.batch_scope(), Some(Scope::Project));
        }
        let expected = settings.batch_expected().cloned();
        if !matches!(case, "missing" | "unconfirmed") {
            let preview =
                |_: &crate::llm::model_config::UiConfig, _: u16| ratatui::text::Line::default();
            let context = crate::ui::overlay::DrawCtx {
                theme: themes.current(),
                keys: &keys,
                status: &status,
                preview_status: &preview,
            };
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
            terminal
                .draw(|frame| settings.draw(frame, &context))
                .unwrap();
            assert!(matches!(
                settings.handle(
                    &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                    &keys
                ),
                OverlayEvent::None
            ));
            terminal
                .draw(|frame| settings.draw(frame, &context))
                .unwrap();
            assert!(matches!(
                settings.handle(
                    &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                    &keys
                ),
                OverlayEvent::UnsetMany { .. }
            ));
        }
        if case == "setup" {
            settings.set_setup_pending(true);
        }
        // A live auth job must block reset even when it has no prepared setting yet.
        let (_sender, receiver) = std::sync::mpsc::channel();
        if case == "auth" {
            auth_job = Some(AuthJob {
                label: "Test",
                receiver,
                lines: None,
                cancel: Arc::new(AtomicBool::new(false)),
                model: None,
            });
        }
        if case == "stale" {
            seed(&app, Scope::Project, &[("ui.notify", true.into())]);
        }
        let before = files(&app);
        let mut overlay = ActiveOverlay::Settings(Box::new(settings));
        let mut history = Vec::new();
        let mut env = OverlayEnv {
            history: &mut history,
            runtime: &runtime,
            todo: &Default::default(),
            model_downloads: None,
            compute_monitor: None,
            live_tasks: None,
            verify_lora: None,
            auth_job: &mut auth_job,
            last_setup_failure: &mut None,
            extensions_pending: &mut extensions_pending,
            extensions_job: &mut extensions_job,
            background_control: None,
            pty_approval: Approval::Readonly,
            app: &app,
            themes: &mut themes,
            keys: &mut keys,
            status: &status,
            catalog: &mut catalog,
            scope: &mut scope,
            dark: None,
            latest: None,
            next: None,
            seen_generation: 0,
            release_compute: None,
            peer_source: None,
        };
        let paths = expected
            .as_ref()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_else(|| vec!["ui.notify".into()]);
        assert!(!env.apply(
            OverlayEvent::UnsetMany {
                paths,
                scope: if case == "scope" {
                    Scope::Global
                } else {
                    Scope::Project
                },
            },
            &mut overlay
        ));
        let ActiveOverlay::Settings(settings) = overlay else {
            unreachable!()
        };
        if case == "success" {
            assert!(settings.batch_expected().is_none());
            assert!(
                crate::config::schema::get(
                    &settings_store::load_layer(&app.layout, Scope::Project).unwrap(),
                    "ui.notify"
                )
                .is_none()
            );
            assert!(env.latest.is_some());
        } else {
            assert_eq!(files(&app), before, "{case}");
            assert_eq!(
                settings.batch_expected(),
                expected.as_ref(),
                "failed reset preserves its review: {case}"
            );
            assert!(env.latest.is_none());
        }
    }
}

#[test]
fn returning_from_provider_configuration_refreshes_the_retained_endpoint() {
    let (_home, _project, app) = app();
    let original: toml::Table =
        toml::from_str("[custom]\nbase_url = 'http://127.0.0.1:8000/v1'\n").unwrap();
    seed(
        &app,
        Scope::Global,
        &[("providers", toml::Value::Table(original.clone()))],
    );
    let configured = app.reload().unwrap().providers;
    let mut panel = ActiveOverlay::Providers(crate::ui::overlay::providers::ProviderOverlay::new(
        None,
        configured,
        Box::new(|_| false),
        Scope::Global,
    ));
    let changed: toml::Table =
        toml::from_str("[custom]\nbase_url = 'http://127.0.0.1:9000/v1'\n").unwrap();
    seed(
        &app,
        Scope::Global,
        &[("providers", toml::Value::Table(changed))],
    );
    let catalog = Catalog::new();
    let before = files(&app);
    refresh_navigation(&app, &catalog, &mut panel, false);
    let theme = crate::ui::theme::Theme::dark();
    let keys = crate::ui::keys::Keymap::defaults();
    let status = crate::ui::status::StatusData::default();
    let ctx = crate::ui::overlay::DrawCtx {
        theme: &theme,
        keys: &keys,
        status: &status,
        preview_status: &|_, _| ratatui::text::Line::default(),
    };
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(180, 30)).unwrap();
    terminal.draw(|frame| panel.draw(frame, &ctx)).unwrap();
    let shown = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(shown.contains("127.0.0.1:9000"), "{shown}");
    assert!(!shown.contains("127.0.0.1:8000"), "{shown}");
    assert_eq!(files(&app), before, "Back refresh is read-only");
}
