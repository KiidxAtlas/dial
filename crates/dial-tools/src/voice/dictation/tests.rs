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
