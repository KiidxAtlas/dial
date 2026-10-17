use super::*;

#[test]
fn linux_recorders_emit_raw_16k_mono_s16le() {
    let candidates = recorder_candidates("linux");
    let programs: Vec<_> = candidates
        .iter()
        .map(|c| c.program.display().to_string())
        .collect();
    assert_eq!(programs, ["arecord", "parecord", "sox", "ffmpeg"]);
    assert_eq!(
        candidates[0].arg_strings(),
        ["-q", "-t", "raw", "-f", "S16_LE", "-r", "16000", "-c", "1"]
    );
    assert!(
        candidates[1]
            .arg_strings()
            .contains(&"--format=s16le".to_owned())
    );
    let ffmpeg = candidates[3].arg_strings();
    assert!(ffmpeg.windows(2).any(|w| w == ["-f", "pulse"]));
    assert!(ffmpeg.windows(2).any(|w| w == ["-ar", "16000"]));
    assert_eq!(ffmpeg.last().map(String::as_str), Some("-"));
}

#[test]
fn macos_recorders_use_default_input_device() {
    let candidates = recorder_candidates("macos");
    assert_eq!(candidates[0].program, Path::new("sox"));
    assert!(candidates[0].arg_strings().contains(&"-d".to_owned()));
    assert!(
        candidates[1]
            .arg_strings()
            .windows(2)
            .any(|w| w == ["-i", ":0"])
    );
}

#[test]
fn missing_configured_recorder_is_a_clear_error() {
    let config = SttConfig {
        capture: CaptureChoice::Command,
        capture_command: vec!["definitely-not-a-recorder".into()],
        ..SttConfig::default()
    };
    let error = Capture::start(&config, &AtomicBool::new(false))
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("not installed"), "{error}");
}

#[cfg(unix)]
#[test]
fn command_capture_decodes_pcm_and_reports_end() {
    // `printf` emits two s16le samples: 0x4000 (0.5) and 0xC000 (-0.5).
    let config = SttConfig {
        capture: CaptureChoice::Command,
        capture_command: vec![
            "sh".into(),
            "-c".into(),
            "printf '\\000\\100\\000\\300'".into(),
        ],
        ..SttConfig::default()
    };
    let (capture, rx) = Capture::start(&config, &AtomicBool::new(false)).unwrap();
    let mut samples = Vec::new();
    let mut error = None;
    for _ in 0..100 {
        if let Some(e) = drain(&rx, &mut samples) {
            error = Some(e);
            break;
        }
    }
    assert_eq!(samples, vec![0.5, -0.5]);
    assert!(error.unwrap().contains("stopped"));
    capture.stop();
}
