use super::*;
use crate::llm::catalog::{Discovery, ModelEntry, ModelKind, ModelMetadata};

fn entry(provider: &str, model: &str, label: &str, source: &str) -> ModelEntry {
    ModelEntry {
        selection: ModelSelection::new(provider, model),
        label: label.into(),
        source: source.into(),
        available: true,
        metadata: ModelMetadata::default(),
    }
}

fn find<'a>(choices: &'a [ValueChoice], value: &str) -> &'a ValueChoice {
    choices
        .iter()
        .find(|choice| choice.value == value)
        .unwrap_or_else(|| panic!("{value} not offered: {choices:?}"))
}

#[test]
fn model_rows_offer_downloads_only_for_missing_suggestions_and_assign_chat_models() {
    let mut local = entry(
        "mlx",
        "/cache/models--mlx-community--Qwen2.5-3B-Instruct-4bit/snapshots/abc",
        "mlx-community/Qwen2.5-3B-Instruct-4bit",
        "Hugging Face cache (MLX)",
    );
    local.metadata.format = Some(crate::llm::local_format::LocalFormat::Mlx);
    let mut chat = entry("openai", "gpt-test", "GPT test", "openai");
    chat.metadata.kind = ModelKind::Chat;
    let mut image = entry("images", "draw-1", "Draw 1", "images");
    image.metadata.kind = ModelKind::Image;
    let discovery = Discovery {
        models: vec![local, chat.clone(), image],
        providers: Vec::new(),
    };
    let mut config = Config::default();
    config.providers.insert("custom".into(), Default::default());
    let choices = settings_value_choices(&config, Some(&discovery), std::path::Path::new("/"));

    let architecture = &choices["codebase.architecture_model"];
    // Downloaded suggestions save at once, by their portable repository id.
    assert_eq!(
        find(architecture, "mlx-community/Qwen2.5-3B-Instruct-4bit").action,
        ChoiceAction::Set
    );
    // Missing suggestions are downloads, which the picker confirms before starting.
    assert!(
        architecture
            .iter()
            .any(|choice| choice.action == ChoiceAction::Download),
        "{architecture:?}"
    );
    assert!(
        architecture
            .iter()
            .all(|choice| !choice.value.starts_with("/cache")),
        "Hugging Face cache entries are saved by repository id"
    );

    // The main model goes through the canonical role writer, not a raw `model` write.
    assert_eq!(
        find(&choices["model"], "gpt-test").action,
        ChoiceAction::Assign {
            role: "main".into(),
            selection: chat.selection.clone(),
        }
    );
    assert_eq!(
        find(&choices["memory.sharpshooter.model"], "openai/gpt-test").action,
        ChoiceAction::Set
    );
    assert!(
        choices["image.model"]
            .iter()
            .all(|choice| choice.value == "draw-1"),
        "only image models are offered for image generation"
    );
    assert_eq!(
        find(&choices["provider"], "custom").action,
        ChoiceAction::Set
    );
    assert!(
        choices
            .values()
            .flatten()
            .all(|choice| choice.action != ChoiceAction::Download || choice.value.contains('/')),
        "downloads name a repository"
    );
}

#[test]
fn voice_choices_distinguish_missing_models_ready_custom_programs_and_credentials() {
    let home = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.user_dir = home.path().into();
    // Readiness probes only executable availability; the fixture never runs this program.
    config.voice.tts.piper_program = std::env::current_exe().unwrap();
    config.voice.tts.espeak_program = std::env::current_exe().unwrap();
    config.voice.stt.whisper_cpp_program = std::env::current_exe().unwrap();
    config.voice.openai.base_url = "https://voice-setup-test.invalid/v1".into();
    config.voice.openai.api_key_env = Some("DIAL_TEST_VOICE_SETUP_MISSING_KEY".into());
    let model = home.path().join("voice.onnx");
    std::fs::write(&model, b"fixture model").unwrap();
    config.voice.tts.piper_model = Some(model.clone());
    let choices = settings_value_choices(&config, None, home.path());
    assert!(
        matches!(
            find(&choices["voice.tts.backend"], "piper").action,
            ChoiceAction::Install { .. }
        ),
        "Piper needs the sidecar too"
    );
    assert_eq!(
        find(&choices["voice.tts.backend"], "espeak").action,
        ChoiceAction::Set
    );
    assert!(
        matches!(
            find(&choices["voice.stt.backend"], "whisper-cpp").action,
            ChoiceAction::Install { .. }
        ),
        "Whisper needs a model, not only a program"
    );
    assert!(matches!(
        find(&choices["voice.tts.backend"], "openai").action,
        ChoiceAction::Unavailable { .. }
    ));
    std::fs::write(
        model.with_added_extension("json"),
        br#"{"audio":{"sample_rate":22050},"phoneme_id_map":{}}"#,
    )
    .unwrap();
    config.voice.stt.whisper_cpp_model = Some(model);
    let choices = settings_value_choices(&config, None, home.path());
    for (path, value) in [
        ("voice.tts.backend", "piper"),
        ("voice.stt.backend", "whisper-cpp"),
    ] {
        assert_eq!(
            find(&choices[path], value).action,
            ChoiceAction::Set,
            "{value} reuses its custom executable and model"
        );
    }
}
