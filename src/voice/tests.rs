use super::*;

#[test]
fn voice_names_cannot_smuggle_options_or_controls() {
    assert!(valid_voice("Samantha"));
    assert!(valid_voice("en-us+f3"));
    assert!(valid_voice("Eddy (English (US))"));
    assert!(!valid_voice(""));
    assert!(!valid_voice("-o/etc/passwd"));
    assert!(!valid_voice("a\nb"));
    assert!(!valid_voice("x;rm"));
    assert!(!valid_voice(&"a".repeat(65)));
}

#[test]
fn speed_is_clamped() {
    assert_eq!(clamp_speed(10.0), 2.5);
    assert_eq!(clamp_speed(0.1), 0.5);
    assert_eq!(clamp_speed(f32::NAN), 1.0);
    assert_eq!(clamp_speed(1.25), 1.25);
}

#[test]
fn config_parses_from_toml_with_defaults() {
    let config: VoiceConfig = toml::from_str(
        "[tts]\nbackend='espeak'\nauto_speak=true\n[stt]\nenabled=true\nbackend='whisper-cpp'\nsubmit_trigger='say-submit'\n",
    )
    .unwrap();
    assert_eq!(config.tts.backend, TtsBackendChoice::Espeak);
    assert!(config.tts.auto_speak);
    assert_eq!(config.stt.backend, SttBackendChoice::WhisperCpp);
    assert_eq!(config.stt.submit_trigger, SubmitTrigger::SaySubmit);
    assert_eq!(config.stt.language, "en");
    assert!(toml::from_str::<VoiceConfig>("[tts]\nbogus=1\n").is_err());
}
