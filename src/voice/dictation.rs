//! Bounded microphone recording and energy-endpointed transcription for push-to-talk.
use super::{
    VoiceEnv,
    capture::Capture,
    endpointer::{Endpointer, EndpointerConfig, EndpointerEvent},
    stt::{self, SttBackend},
};
use anyhow::{Result, ensure};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::RecvTimeoutError,
    },
    time::{Duration, Instant},
};

/// When a recording ends on its own. Toggle and live modes end on trailing silence; a held key
/// ends only when released (cancel) or at the hard cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndRule {
    Silence,
    Manual,
}

/// Record until trailing silence or the configured hard cap, transcribing complete segments.
pub fn record_and_transcribe(env: &VoiceEnv, cancel: &Arc<AtomicBool>) -> Result<String> {
    record(env, cancel, EndRule::Silence)
}

/// Record until `cancel` is set (the key was released) or the hard cap, transcribing segments.
pub fn record_held(env: &VoiceEnv, cancel: &Arc<AtomicBool>) -> Result<String> {
    record(env, cancel, EndRule::Manual)
}

/// Collects segment transcriptions. A failed segment does not discard the others: it is kept as
/// the first failure and returned only if no segment produced text. Cancellation still aborts.
#[derive(Default)]
struct Segments {
    texts: Vec<String>,
    first_failure: Option<anyhow::Error>,
}

impl Segments {
    fn add(
        &mut self,
        env: &VoiceEnv,
        backend: &SttBackend,
        segment: &[f32],
        cancel: &Arc<AtomicBool>,
    ) -> Result<()> {
        match stt::transcribe(env, backend, segment, cancel) {
            Ok(text) => {
                if !text.trim().is_empty() {
                    self.texts.push(text.trim().to_owned());
                }
                Ok(())
            }
            Err(error) if cancel.load(Ordering::SeqCst) => Err(error),
            Err(error) => {
                self.first_failure.get_or_insert(error);
                Ok(())
            }
        }
    }

    fn finish(self) -> Result<String> {
        if self.texts.is_empty()
            && let Some(error) = self.first_failure
        {
            return Err(error);
        }
        ensure!(!self.texts.is_empty(), "no speech detected");
        Ok(self.texts.join(" "))
    }
}

fn record(env: &VoiceEnv, cancel: &Arc<AtomicBool>, end: EndRule) -> Result<String> {
    let config = &env.voice.stt;
    ensure!(
        config.enabled,
        "speech recognition is disabled (set voice.stt.enabled = true)"
    );
    let backend = stt::resolve_backend(env)?;
    let (capture, rx) = Capture::start(config, cancel)?;
    let _source = capture.source().to_owned();
    let mut endpointer = Endpointer::new(EndpointerConfig {
        end_silence_ms: config.end_silence_ms.clamp(100, 5000),
        min_speech_ms: config.min_speech_ms.clamp(50, 5000),
        ..Default::default()
    });
    let deadline = Instant::now() + Duration::from_secs(config.max_recording_secs.clamp(1, 600));
    let mut segments = Segments::default();
    // Release only ends recording: an in-flight segment is still transcribed, not aborted.
    let no_abort = Arc::new(AtomicBool::new(false));
    let mut completed = false;
    while !completed && !cancel.load(Ordering::SeqCst) {
        if Instant::now() >= deadline {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(samples)) => {
                for event in endpointer.push(&samples) {
                    let EndpointerEvent::Segment(segment) = event;
                    segments.add(env, &backend, &segment, &no_abort)?;
                    if end == EndRule::Silence && !endpointer.in_speech() {
                        completed = true;
                        break;
                    }
                }
            }
            Ok(Err(error)) => anyhow::bail!("microphone capture failed: {error}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(rx);
    capture.stop();
    if endpointer.in_speech() {
        let not_cancelled = Arc::new(AtomicBool::new(false));
        for event in endpointer.flush() {
            let EndpointerEvent::Segment(segment) = event;
            segments.add(env, &backend, &segment, &not_cancelled)?;
        }
    }
    segments.finish()
}
