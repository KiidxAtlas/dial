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
