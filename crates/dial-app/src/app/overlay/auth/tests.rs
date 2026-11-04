use super::*;
use crate::app::core::App;

/// Prepared settings are written only after successful, uncancelled background work.
#[test]
fn prepared_settings_save_only_after_successful_uncancelled_work() {
    let (_home, _project, layout, config) = crate::app::tests::config();
    let app = App::new(
        layout,
        config,
        toml::Table::new(),
        Default::default(),
        vec![],
        Default::default(),
    )
    .unwrap();
    let model = _home.path().join("voice.onnx");
    std::fs::write(&model, b"fixture").unwrap();
    std::fs::write(
        model.with_added_extension("json"),
        br#"{"audio":{"sample_rate":22050},"phoneme_id_map":{}}"#,
    )
    .unwrap();
    let program = std::env::current_exe().unwrap();
    let prepared_changes = vec![
        Change::Set {
            path: "voice.tts.piper_program".into(),
            value: toml::Value::String(program.display().to_string()),
        },
        Change::Set {
            path: "voice.stt.whisper_cpp_program".into(),
            value: toml::Value::String(program.display().to_string()),
        },
        Change::Set {
            path: "voice.tts.piper_model".into(),
            value: toml::Value::String(model.display().to_string()),
        },
        Change::Set {
            path: "voice.stt.whisper_cpp_model".into(),
            value: toml::Value::String(model.display().to_string()),
        },
    ];
    for (path, value, label) in [
        (
            "local_runtime.draft_model",
            "mlx-community/draft-test",
            "Model",
        ),
        ("voice.tts.backend", "piper", "Voice"),
        ("voice.stt.backend", "whisper-cpp", "Voice"),
    ] {
        for (cancelled, success, model_present, settings_changed) in [
            (true, true, true, false),
            (false, false, true, false),
            (false, true, false, false),
            (false, true, true, true),
            (false, true, true, false),
        ] {
            if model_present {
                std::fs::write(&model, b"fixture").unwrap();
            } else {
                std::fs::remove_file(&model).unwrap();
            }
            let (sender, receiver) = std::sync::mpsc::channel();
            let mut job = Some(AuthJob {
                label,
                receiver,
                lines: None,
                cancel: Arc::new(AtomicBool::new(cancelled)),
                model: Some(PreparedModel {
                    changes: vec![Change::Set {
                        path: path.into(),
                        value: toml::Value::String(value.into()),
                    }],
                    role: None,
                    scope: Scope::Global,
                    review: None,
                    voice: if label == "Voice" {
                        let (tx, rx) = std::sync::mpsc::channel();
                        tx.send(prepared_changes.clone()).unwrap();
                        Some(PreparedVoice {
                            original: app.reload().unwrap().voice,
                            changes: rx,
                        })
                    } else {
                        None
                    },
                }),
            });
            if settings_changed {
                app.save_settings(
                    Scope::Global,
                    &[Change::Set {
                        path: "voice.stt.enabled".into(),
                        value: toml::Value::Boolean(!app.reload().unwrap().voice.stt.enabled),
                    }],
                    None,
                )
                .unwrap();
            }
            let get = || {
                crate::config::schema::get(
                    &toml::Table::try_from(app.reload().unwrap()).unwrap(),
                    path,
                )
                .cloned()
            };
            let before = get();
            let before_voice = app.reload().unwrap().voice;
            assert!(AuthJob::collect(&mut job, &app).is_none());
            assert_eq!(get(), before, "nothing is written while preparation runs");
            sender
                .send(if success {
                    Ok("downloaded".into())
                } else {
                    Err(anyhow::anyhow!("download failed"))
                })
                .unwrap();
            let (_, result) = AuthJob::collect(&mut job, &app).unwrap();
            let should_save =
                success && !cancelled && (label != "Voice" || (model_present && !settings_changed));
            assert_eq!(result.is_ok(), should_save);
            assert_eq!(
                get() == Some(toml::Value::String(value.into())),
                should_save
            );
            if !should_save {
                assert_eq!(
                    app.reload().unwrap().voice,
                    before_voice,
                    "failed/cancelled preparation writes no voice settings"
                );
            }
        }
    }
}

#[test]
fn reviewed_model_preparation_publishes_only_successful_current_transactions() {
    use crate::config::settings_store::{self, Change, Scope};
    use crate::llm::model_config::ModelSelection;

    for (cancelled, success, stale_scope) in [
        (false, true, None),
        (true, true, None),
        (false, false, None),
        (false, true, Some(Scope::Global)),
        (false, true, Some(Scope::Project)),
    ] {
        let (_home, _project, layout, _) = crate::app::tests::config();
        settings_store::save_config(
            &layout,
            Scope::Project,
            &[
                Change::Set {
                    path: "model".into(),
                    value: "old-project-model".into(),
                },
                Change::Set {
                    path: "context_window".into(),
                    value: 4096.into(),
                },
                Change::Set {
                    path: "task.prefer_subagents".into(),
                    value: true.into(),
                },
            ],
        )
        .unwrap();
        let config = settings_store::load_config(&layout, &toml::Table::new()).unwrap();
        let app = App::new(
            layout,
            config,
            toml::Table::new(),
            Default::default(),
            vec![],
            Default::default(),
        )
        .unwrap();
        let selection = ModelSelection::new("mlx", "reviewed-model");
        let changes = crate::config::selection::changes(&app.config, &selection, "main").unwrap();
        let review =
            settings_store::review_model_config(&app.layout, Scope::Global, &changes, Some("main"))
                .unwrap();
        let expected =
            settings_store::preview_model_config(&app.layout, Scope::Global, &changes, "main")
                .unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut job = Some(AuthJob {
            label: "Model",
            receiver,
            lines: None,
            cancel: cancel.clone(),
            model: Some(PreparedModel {
                changes,
                role: Some("main".into()),
                scope: Scope::Global,
                voice: None,
                review: Some(review),
            }),
        });
        let original = app
            .layout
            .config_files()
            .map(|path| std::fs::read(path).ok());
        assert!(AuthJob::collect(&mut job, &app).is_none());
        assert_eq!(
            app.layout
                .config_files()
                .map(|path| std::fs::read(path).ok()),
            original
        );
        // Simulate completion without network, then cancellation or a competing settings write.
        sender
            .send(if success {
                Ok("downloaded".into())
            } else {
                Err(anyhow::anyhow!("download failed"))
            })
            .unwrap();
        cancel.store(cancelled, Ordering::SeqCst);
        if let Some(scope) = stale_scope {
            settings_store::save_config(
                &app.layout,
                scope,
                &[Change::Set {
                    path: "model".into(),
                    value: "concurrent-model".into(),
                }],
            )
            .unwrap();
        }
        let before_collect = app
            .layout
            .config_files()
            .map(|path| std::fs::read(path).ok());
        let (_, result) = AuthJob::collect(&mut job, &app).unwrap();
        assert!(job.is_none());
        if success && !cancelled && stale_scope.is_none() {
            result.unwrap();
            assert_eq!(
                toml::Table::try_from(app.reload().unwrap()).unwrap(),
                toml::Table::try_from(expected).unwrap()
            );
            let project = settings_store::load_layer(&app.layout, Scope::Project).unwrap();
            assert!(project.get("model").is_none());
            assert!(project.get("context_window").is_none());
            assert_eq!(project["task"]["prefer_subagents"].as_bool(), Some(true));
        } else {
            let error = result.unwrap_err().to_string();
            if stale_scope.is_some() {
                assert!(error.contains("changed since review"), "{error}");
            }
            assert_eq!(
                app.layout
                    .config_files()
                    .map(|path| std::fs::read(path).ok()),
                before_collect,
                "failed, cancelled or stale completion must not publish either layer"
            );
        }
    }
}

#[test]
fn retained_setup_failures_follow_observed_completion_and_redact_before_bounding() {
    let (_home, _project, layout, config) = crate::app::tests::config();
    let app = App::new(
        layout,
        config,
        toml::Table::new(),
        Default::default(),
        vec![],
        Default::default(),
    )
    .unwrap();
    for (label, cancelled, disconnected, success) in [
        ("Setup", false, false, false),
        ("Model", false, false, false),
        ("Voice", false, true, false),
        ("Setup", true, false, false),
        ("Voice", true, true, false),
        ("Model", true, false, true),
        ("Setup", false, false, true),
        ("OpenRouter", false, false, false),
    ] {
        let old = SetupFailure {
            label: "Earlier".into(),
            detail: "historical failure".into(),
        };
        let mut retained = Some(old.clone());
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut slot = Some(AuthJob {
            label,
            receiver,
            cancel: Arc::new(AtomicBool::new(cancelled)),
            lines: None,
            model: None,
        });
        assert!(AuthJob::collect_retained(&mut slot, &app, &mut retained).is_none());
        assert_eq!(retained, Some(old.clone()), "pending is not failure");
        if !disconnected {
            sender
                .send(if success {
                    Ok("prepared".into())
                } else {
                    Err(anyhow::anyhow!(
                        "Authorization: Bearer abcdefghijklmnopqrstuvwxyz1234567890 {}",
                        "é".repeat(4096)
                    ))
                })
                .unwrap();
        }
        drop(sender);
        let (_, result) = AuthJob::collect_retained(&mut slot, &app, &mut retained).unwrap();
        assert!(slot.is_none());
        if !cancelled && !success && label != "OpenRouter" {
            assert!(result.is_err());
            let failure = retained.as_ref().unwrap();
            assert_eq!(failure.label, label);
            assert!(failure.detail.chars().count() <= 2048);
            assert!(
                !failure
                    .detail
                    .contains("abcdefghijklmnopqrstuvwxyz1234567890")
            );
            if !disconnected {
                assert!(failure.detail.contains("REDACTED"));
            }
        } else {
            assert_eq!(retained, Some(old));
        }
        let before = retained.clone();
        assert!(AuthJob::collect_retained(&mut slot, &app, &mut retained).is_none());
        assert_eq!(retained, before);
    }
}
