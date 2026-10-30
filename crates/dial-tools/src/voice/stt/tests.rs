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

#[test]
fn timeline_uses_backend_offsets_and_never_invents_speakers() {
    let segments = parse_timeline(
        br#"{"transcription":[{"offsets":{"from":100,"to":700},"text":" hello "}]}"#,
        1000,
    )
    .unwrap();
    assert_eq!(
        segments[0],
        TranscriptSegment {
            start_ms: 100,
            end_ms: 700,
            text: "hello".into(),
            speaker: None
        }
    );
    for offsets in [
        r#"{"from":800,"to":700}"#,
        r#"{"from":100,"to":1001}"#,
        r#"{"from":-1,"to":700}"#,
    ] {
        let input = format!(r#"{{"transcription":[{{"offsets":{offsets},"text":"x"}}]}}"#);
        assert!(parse_timeline(input.as_bytes(), 1000).is_err());
    }
    assert!(parse_timeline(br#"{"text":"no timestamps"}"#, 1000).is_err());
}

#[test]
fn timeline_rejects_text_only_backend_without_network_or_process() {
    let result = transcribe_timeline(
        &VoiceEnv::default(),
        &SttBackend::OpenAi,
        Path::new("absent.wav"),
        1000,
        &AtomicBool::new(false),
    );
    assert!(result.unwrap_err().to_string().contains("timeline adapter"));
}

#[test]
fn timeline_file_reads_are_bounded_before_loading() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), b"{}").unwrap();
    assert_eq!(read_timeline(file.path()).unwrap(), b"{}");
    file.as_file().set_len(4 * 1024 * 1024 + 1).unwrap();
    assert!(
        read_timeline(file.path())
            .unwrap_err()
            .to_string()
            .contains("4 MiB")
    );
    assert!(read_timeline(file.path().with_extension("missing").as_path()).is_err());
}
