use super::*;
use crate::config::settings_store::{self, Scope};

#[test]
fn recipes_use_serialized_choices_and_only_matching_rows() {
    for (path, choice, id) in [
        (
            "voice.tts.backend",
            serde_json::to_value(TtsBackendChoice::Piper).unwrap(),
            "piper",
        ),
        (
            "voice.tts.backend",
            serde_json::to_value(TtsBackendChoice::Espeak).unwrap(),
            "espeak-ng",
        ),
        (
            "voice.stt.backend",
            serde_json::to_value(SttBackendChoice::WhisperCpp).unwrap(),
            "whisper-cpp",
        ),
        (
            "voice.stt.backend",
            serde_json::to_value(SttBackendChoice::MlxWhisper).unwrap(),
            "mlx-whisper",
        ),
    ] {
        assert_eq!(recipe_for(path, choice.as_str().unwrap()), Some(id));
    }
    for (path, choice) in [
        ("voice.tts.backend", "auto"),
        ("voice.stt.backend", "openai"),
        ("voice.stt.backend", "piper"),
        ("voice.tts.backend", "espeak-ng"),
        ("voice.stt.backend", "whisper_cpp"),
        ("unrelated", "piper"),
    ] {
        assert_eq!(recipe_for(path, choice), None);
    }
}

#[test]
fn piper_requires_nonempty_model_and_matching_valid_sidecar() {
    let tmp = tempfile::tempdir().unwrap();
    let model = tmp.path().join("custom.onnx");
    assert!(!piper_model_ready(&model));
    std::fs::write(&model, b"model").unwrap();
    assert!(!piper_model_ready(&model));
    for json in [
        "",
        "not json",
        "{}",
        r#"{"audio":{"sample_rate":0},"phoneme_id_map":{}}"#,
    ] {
        std::fs::write(model.with_added_extension("json"), json).unwrap();
        assert!(!piper_model_ready(&model), "{json}");
    }
    std::fs::write(
        model.with_added_extension("json"),
        r#"{"audio":{"sample_rate":22050},"phoneme_id_map":{"a":[1]}}"#,
    )
    .unwrap();
    assert!(piper_model_ready(&model));
    std::fs::write(&model, b"").unwrap();
    assert!(!piper_model_ready(&model));
}

#[cfg(unix)]
fn executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    // A ready backend must not run anything, even a health probe or installer.
    std::fs::write(path, "#!/bin/sh\nexit 42\n").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn ready_custom_backends_do_not_install_or_change_settings() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let program = home.path().join("custom-program");
    executable(&program);
    let model = home.path().join("custom.onnx");
    std::fs::write(&model, b"custom model").unwrap();
    std::fs::write(
        model.with_added_extension("json"),
        r#"{"audio":{"sample_rate":22050},"phoneme_id_map":{}}"#,
    )
    .unwrap();
    let mut config = Config {
        user_dir: home.path().to_owned(),
        ..Config::default()
    };
    config.voice.tts.piper_program = program.clone();
    config.voice.tts.piper_model = Some(model.clone());
    config.voice.tts.espeak_program = program.clone();
    config.voice.stt.whisper_cpp_program = program;
    config.voice.stt.whisper_cpp_model = Some(model);
    let before = config.voice.clone();
    for (path, value) in [
        ("voice.tts.backend", "piper"),
        ("voice.tts.backend", "espeak"),
        ("voice.stt.backend", "whisper-cpp"),
    ] {
        let changes = prepare(
            &layout,
            &config,
            path,
            value,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert!(changes.is_empty());
        assert_eq!(config.voice, before);
        assert!(!layout.config_files()[0].exists());
    }
}

#[test]
fn cancellation_and_missing_custom_program_fail_without_writing_settings() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let mut config = Config::default();
    config.voice.tts.espeak_program = home.path().join("missing-custom-espeak");
    for cancelled in [true, false] {
        let error = prepare(
            &layout,
            &config,
            "voice.tts.backend",
            "espeak",
            &AtomicBool::new(cancelled),
            &mut |_| {},
        )
        .err()
        .expect("preparation should fail");
        assert!(
            error.to_string().contains(if cancelled {
                "cancelled"
            } else {
                "configured executable"
            }),
            "{error}"
        );
        assert!(!layout.config_files()[0].exists());
        assert!(!config.voice.stt.enabled);
        assert_eq!(config.voice.tts.backend, TtsBackendChoice::Auto);
    }
}

#[cfg(unix)]
#[test]
fn incomplete_custom_piper_model_fails_instead_of_replacing_it() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let program = home.path().join("piper");
    executable(&program);
    let model = home.path().join("custom.onnx");
    std::fs::write(&model, b"keep me").unwrap();
    let mut config = Config::default();
    config.voice.tts.piper_program = program;
    config.voice.tts.piper_model = Some(model.clone());
    let error = prepare(
        &layout,
        &config,
        "voice.tts.backend",
        "piper",
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .err()
    .expect("incomplete custom model should fail");
    assert!(error.to_string().contains("matching .onnx.json"), "{error}");
    assert_eq!(std::fs::read(&model).unwrap(), b"keep me");
    assert!(!layout.config_files()[0].exists());
}

#[cfg(unix)]
#[test]
fn preparing_default_piper_model_returns_changes_for_caller_selected_scope() {
    use std::os::unix::fs::PermissionsExt;
    for scope in [Scope::Global, Scope::Project] {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
        let program = home.path().join("piper");
        let python = home.path().join("python3");
        std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
        // Model downloader double: verifies the command's explicit destination through its
        // behavior and writes both required files. No real Python, package manager or network.
        std::fs::write(&python, "#!/bin/sh\nset -eu\nif [ \"$1\" = -c ]; then test -s \"$3\"; exit 0; fi\nprintf model > \"$4/$5.onnx\"\nprintf '%s' '{\"audio\":{\"sample_rate\":22050},\"phoneme_id_map\":{}}' > \"$4/$5.onnx.json\"\n").unwrap();
        for path in [&program, &python] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut config = Config {
            user_dir: home.path().to_owned(),
            ..Config::default()
        };
        config.voice.tts.piper_program = program.clone();
        let changes = prepare(
            &layout,
            &config,
            "voice.tts.backend",
            "piper",
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert!(layout.config_files().iter().all(|path| !path.exists()));
        settings_store::save_config(&layout, scope, &changes).unwrap();
        let fresh = settings_store::load_config(&layout, &toml::Table::new()).unwrap();
        assert_eq!(fresh.voice.tts.piper_program, program);
        assert_eq!(fresh.voice.tts.backend, TtsBackendChoice::Auto);
        assert!(!fresh.voice.stt.enabled);
        ready(&fresh, "voice.tts.backend", "piper").unwrap();
        assert_eq!(layout.config_files()[0].exists(), scope == Scope::Global);
        assert_eq!(layout.config_files()[1].exists(), scope == Scope::Project);
        assert!(!project.path().join(format!("{PIPER_VOICE}.onnx")).exists());
    }
}

#[cfg(unix)]
#[test]
fn backend_python_follows_entry_point_symlink_and_fails_closed_without_interpreter() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("venv/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let program = bin.join("piper");
    executable(&program);
    let link = tmp.path().join("exposed-piper");
    symlink(&program, &link).unwrap();
    assert!(adjacent_python(&link).is_err());
    let python = bin.join("python3");
    executable(&python);
    assert_eq!(adjacent_python(&link).unwrap(), python);
}

#[test]
fn mlx_readiness_requires_complete_local_or_current_cached_model() {
    let cache = tempfile::tempdir().unwrap();
    let repo = cache.path().join("models--owner--whisper");
    let snapshot = repo.join("snapshots/abc123");
    std::fs::create_dir_all(&snapshot).unwrap();
    std::fs::create_dir_all(repo.join("refs")).unwrap();
    assert!(!mlx_model_ready("owner/whisper", Some(cache.path())));
    std::fs::write(repo.join("refs/main"), "abc123\n").unwrap();
    std::fs::write(snapshot.join("config.json"), r#"{"n_mels":128}"#).unwrap();
    assert!(!mlx_model_ready("owner/whisper", Some(cache.path())));
    for weight in ["model.safetensors", "weights.safetensors", "weights.npz"] {
        let path = snapshot.join(weight);
        std::fs::write(&path, b"").unwrap();
        assert!(!mlx_model_ready("owner/whisper", Some(cache.path())));
        std::fs::write(&path, b"weights").unwrap();
        assert!(mlx_model_ready("owner/whisper", Some(cache.path())));
        assert!(mlx_model_ready(snapshot.to_str().unwrap(), None));
        std::fs::remove_file(path).unwrap();
    }
    std::fs::write(snapshot.join("weights.npz"), b"weights").unwrap();
    for revision in ["", "../../abc123", "missing", "def456"] {
        std::fs::write(repo.join("refs/main"), revision).unwrap();
        assert!(!mlx_model_ready("owner/whisper", Some(cache.path())));
    }
    assert!(!mlx_model_ready("../whisper", Some(cache.path())));
}

#[cfg(unix)]
#[test]
fn failed_or_cancelled_piper_preparation_never_writes_settings() {
    use std::os::unix::fs::PermissionsExt;
    for (download_fails, load_fails, cancel_on_load) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
        let program = home.path().join("piper");
        let python = home.path().join("python3");
        executable(&program);
        let script = format!(
            "#!/bin/sh\nset -eu\nif [ \"$1\" = -c ]; then exit {}; fi\nif [ {} = true ]; then exit 24; fi\nprintf model > \"$4/$5.onnx\"\nprintf '%s' '{{\"audio\":{{\"sample_rate\":22050}},\"phoneme_id_map\":{{}}}}' > \"$4/$5.onnx.json\"\n",
            if load_fails { 23 } else { 0 },
            download_fails,
        );
        std::fs::write(&python, script).unwrap();
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = Config::default();
        config.voice.tts.piper_program = program;
        let cancel = AtomicBool::new(false);
        let result = prepare(
            &layout,
            &config,
            "voice.tts.backend",
            "piper",
            &cancel,
            &mut |line| {
                if cancel_on_load && line.contains(" -c ") {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
        );
        assert!(result.is_err());
        assert!(layout.config_files().iter().all(|path| !path.exists()));
        assert_eq!(config.voice.tts.backend, TtsBackendChoice::Auto);
        assert!(!config.voice.stt.enabled);
    }
}
