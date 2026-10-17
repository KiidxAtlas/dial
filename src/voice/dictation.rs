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
/// ends only when released (finish) or at the hard cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndRule {
    Silence,
    Manual,
}

/// Record until trailing silence or the configured hard cap, transcribing complete segments.
/// `finish` ends the recording early and transcribes what was heard; `abort` discards it.
pub fn record_and_transcribe(
    env: &VoiceEnv,
    finish: &Arc<AtomicBool>,
    abort: &Arc<AtomicBool>,
) -> Result<String> {
    record(env, finish, abort, EndRule::Silence)
}

/// Record until `finish` is set (the key was released) or the hard cap, transcribing segments.
/// `abort` discards the recording: capture stops, transcription in flight is cancelled, and no
/// partial text is returned.
pub fn record_held(
    env: &VoiceEnv,
    finish: &Arc<AtomicBool>,
    abort: &Arc<AtomicBool>,
) -> Result<String> {
    record(env, finish, abort, EndRule::Manual)
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
        abort: &Arc<AtomicBool>,
    ) -> Result<()> {
        match stt::transcribe(env, backend, segment, abort) {
            Ok(text) => {
                if !text.trim().is_empty() {
                    self.texts.push(text.trim().to_owned());
                }
                Ok(())
            }
            Err(error) if abort.load(Ordering::SeqCst) => Err(error),
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

fn record(
    env: &VoiceEnv,
    finish: &Arc<AtomicBool>,
    abort: &Arc<AtomicBool>,
    end: EndRule,
) -> Result<String> {
    let config = &env.voice.stt;
    ensure!(
        config.enabled,
        "speech recognition is disabled (set voice.stt.enabled = true)"
    );
    let backend = stt::resolve_backend(env)?;
    let (capture, rx) = Capture::start(config, abort)?;
    let _source = capture.source().to_owned();
    let mut endpointer = Endpointer::new(EndpointerConfig {
        end_silence_ms: config.end_silence_ms.clamp(100, 5000),
        min_speech_ms: config.min_speech_ms.clamp(50, 5000),
        ..Default::default()
    });
    let deadline = Instant::now() + Duration::from_secs(config.max_recording_secs.clamp(1, 600));
    let mut segments = Segments::default();
    let mut completed = false;
    while !completed && !finish.load(Ordering::SeqCst) && !abort.load(Ordering::SeqCst) {
        if Instant::now() >= deadline {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(samples)) => {
                for event in endpointer.push(&samples) {
                    let EndpointerEvent::Segment(segment) = event;
                    segments.add(env, &backend, &segment, abort)?;
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
    ensure!(!abort.load(Ordering::SeqCst), "speech input was cancelled");
    if endpointer.in_speech() {
        // Finishing only ends recording: trailing speech is still transcribed, not aborted.
        for event in endpointer.flush() {
            let EndpointerEvent::Segment(segment) = event;
            segments.add(env, &backend, &segment, abort)?;
        }
    }
    ensure!(!abort.load(Ordering::SeqCst), "speech input was cancelled");
    segments.finish()
}

#[cfg(all(test, unix))]
mod tests;
