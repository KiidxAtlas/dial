use super::*;

#[test]
fn whisper_cpp_command_prints_plain_transcript() {
    let spec = whisper_cpp_command(
        Path::new("/opt/whisper-cli"),
        Path::new("/m/ggml-base.en.bin"),
        Path::new("/t/u.wav"),
        "en",
    );
    assert_eq!(spec.program, Path::new("/opt/whisper-cli"));
    assert_eq!(
        spec.arg_strings(),
        [
            "-m",
            "/m/ggml-base.en.bin",
            "-f",
            "/t/u.wav",
            "-l",
            "en",
            "-nt",
            "-np"
        ]
    );
    let auto = whisper_cpp_command(Path::new("w"), Path::new("m"), Path::new("a"), "--evil");
    assert!(auto.arg_strings().windows(2).any(|w| w == ["-l", "auto"]));
}

#[test]
fn mlx_whisper_command_writes_named_txt() {
    let spec = mlx_whisper_command(
        Path::new("mlx_whisper"),
        "mlx-community/whisper-large-v3-turbo",
        Path::new("/t/u.wav"),
        "de",
        Path::new("/t"),
        "transcript",
    );
    assert_eq!(
        spec.arg_strings(),
        [
            "/t/u.wav",
            "--model",
            "mlx-community/whisper-large-v3-turbo",
            "--output-format",
            "txt",
            "--output-dir",
            "/t",
            "--output-name",
            "transcript",
            "--verbose",
            "False",
            "--language",
            "de"
        ]
    );
    let auto = mlx_whisper_command(
        Path::new("m"),
        "x",
        Path::new("a"),
        "auto",
        Path::new("/t"),
        "n",
    );
    assert!(!auto.arg_strings().contains(&"--language".to_owned()));
}

#[test]
fn multipart_body_is_well_formed() {
    let body = multipart_body("B", &[("model", "whisper-1")], "u.wav", b"RIFF");
    let text = String::from_utf8(body).unwrap();
    assert!(
        text.starts_with(
            "--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n"
        )
    );
    assert!(text.contains(
        "name=\"file\"; filename=\"u.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF\r\n--B--\r\n"
    ));
}

#[test]
fn transcripts_are_cleaned() {
    assert_eq!(clean_transcript("  hello \n world  "), "hello world");
    assert_eq!(clean_transcript("[BLANK_AUDIO]"), "");
    assert_eq!(clean_transcript(" (silence) "), "");
    assert_eq!(clean_transcript("[a] and [b]"), "[a] and [b]");
}

#[test]
fn explicit_backends_report_missing_prerequisites() {
    let mut env = VoiceEnv::default();
    env.voice.stt.backend = SttBackendChoice::WhisperCpp;
    let error = resolve_backend(&env).unwrap_err().to_string();
    assert!(error.contains("whisper_cpp_model"), "{error}");
    env.voice.stt.backend = SttBackendChoice::MlxWhisper;
    env.voice.stt.mlx_whisper_program = "definitely-not-mlx-whisper".into();
    let error = resolve_backend(&env).unwrap_err().to_string();
    assert!(error.contains("not installed"), "{error}");
}
