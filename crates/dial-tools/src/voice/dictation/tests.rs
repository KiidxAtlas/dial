use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn abort_stops_stalled_transcription_without_partial_text() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("stalled-stt");
    std::fs::write(&program, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let backend = SttBackend::WhisperCpp {
        program,
        model: dir.path().join("model.bin"),
    };
    let env = VoiceEnv::default();
    let abort = Arc::new(AtomicBool::new(false));
    let trigger = Arc::clone(&abort);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        trigger.store(true, Ordering::SeqCst);
    });
    let mut segments = Segments::default();
    let started = Instant::now();
    let segment = vec![0.1_f32; 1600];
    assert!(segments.add(&env, &backend, &segment, &abort).is_err());
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(segments.texts.is_empty());
}

#[test]
fn stopping_capture_preserves_queued_speech_and_shutdown_tail() {
    let (tx, rx) = std::sync::mpsc::channel();
    let speech = vec![0.1; 4000];
    tx.send(Ok(speech[..1600].to_vec())).unwrap();
    let mut endpointer = Endpointer::new(EndpointerConfig::default());
    let mut segments = Vec::new();
    stop_and_drain(
        &rx,
        || {
            tx.send(Ok(speech[1600..].to_vec())).unwrap();
            drop(tx);
        },
        &AtomicBool::new(false),
        |chunk| {
            segments.extend(endpointer.push(&chunk.unwrap()));
            Ok(())
        },
    )
    .unwrap();
    segments.extend(endpointer.flush());
    assert_eq!(segments, vec![EndpointerEvent::Segment(speech)]);
}

#[test]
fn shutdown_drain_preserves_errors_and_honors_abort() {
    for aborted in [false, true] {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Err("device disconnected".into())).unwrap();
        let mut consumed = false;
        let result = stop_and_drain(
            &rx,
            || drop(tx),
            &AtomicBool::new(aborted),
            |chunk| {
                consumed = true;
                chunk.map(|_| ()).map_err(anyhow::Error::msg)
            },
        );
        let error = result.unwrap_err().to_string();
        assert_eq!(consumed, !aborted);
        assert!(error.contains(if aborted {
            "cancelled"
        } else {
            "device disconnected"
        }));
    }
}

#[test]
fn empty_transcription_is_not_reported_as_missing_microphone_speech() {
    for (samples, attempted, expected) in [
        (0, 0, "no microphone audio received from test mic"),
        (100, 0, "recording from test mic was too short"),
        (4000, 0, "no speech detected in audio from test mic"),
        (4000, 1, "backend returned no text"),
    ] {
        let segments = Segments {
            attempted,
            ..Default::default()
        };
        let error = segments.finish(samples, 200, "test mic").unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
    let failed = Segments {
        first_failure: Some(anyhow::anyhow!("backend unavailable")),
        attempted: 1,
        ..Default::default()
    };
    assert_eq!(
        failed
            .finish(4000, 200, "test mic")
            .unwrap_err()
            .to_string(),
        "backend unavailable"
    );
}

#[test]
fn recording_stops_before_segment_transcription_and_caps_queued_audio() {
    use crate::config::voice::{CaptureChoice, SttBackendChoice};

    let dir = tempfile::tempdir().unwrap();
    let pcm = dir.path().join("speech.pcm");
    let pid = dir.path().join("recorder.pid");
    let backend = dir.path().join("transcribe");
    std::fs::write(&pcm, [0x00, 0x20].repeat(16_000 * 13)).unwrap();
    std::fs::write(&pid, "0").unwrap();
    std::fs::write(
        &backend,
        "#!/bin/sh\ncp \"$4\" \"$2.wav\"\nif kill -0 \"$(cat \"$2\")\" 2>/dev/null; then\n  echo capture-still-running\nelse\n  echo capture-stopped\nfi\n",
    )
    .unwrap();
    std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut env = VoiceEnv::default();
    let config = &mut env.voice.stt;
    config.enabled = true;
    config.backend = SttBackendChoice::WhisperCpp;
    config.whisper_cpp_program = backend;
    config.whisper_cpp_model = Some(pid.clone());
    config.capture = CaptureChoice::Command;
    config.capture_command = vec![
        "/bin/sh".into(),
        "-c".into(),
        "echo $$ > \"$2\"; cat \"$1\"; exec sleep 30".into(),
        "recorder".into(),
        pcm.to_string_lossy().into_owned(),
        pid.to_string_lossy().into_owned(),
    ];
    config.max_recording_secs = 1;
    let text = record_held(
        &env,
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(text, "capture-stopped");
    // One second of mono PCM16, plus the WAV header; queued excess is never decoded.
    assert_eq!(
        std::fs::metadata(pid.with_extension("pid.wav"))
            .unwrap()
            .len(),
        44 + 16_000 * 2
    );
}
