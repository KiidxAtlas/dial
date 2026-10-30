//! Bounded microphone recording and energy-endpointed transcription for push-to-talk.
use super::{
    VoiceEnv,
    capture::{Capture, Chunk},
    endpointer::{Endpointer, EndpointerConfig, EndpointerEvent},
    stt::{self, SttBackend},
};
use anyhow::{Result, ensure};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError},
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
    attempted: usize,
}

impl Segments {
    fn add(
        &mut self,
        env: &VoiceEnv,
        backend: &SttBackend,
        segment: &[f32],
        abort: &Arc<AtomicBool>,
    ) -> Result<()> {
        self.attempted += 1;
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

    fn finish(self, sample_count: usize, min_speech_ms: u32, source: &str) -> Result<String> {
        ensure!(
            sample_count > 0,
            "no microphone audio received from {source}; hold the speech key while speaking and check microphone permission and the input device"
        );
        ensure!(
            sample_count as u64 * 1000 >= u64::from(min_speech_ms) * 16_000,
            "recording from {source} was too short; hold the speech key while speaking"
        );
        ensure!(
            self.attempted > 0,
            "no speech detected in audio from {source}; check the input device and microphone level"
        );
        if self.texts.is_empty()
            && let Some(error) = self.first_failure
        {
            return Err(error);
        }
        ensure!(
            !self.texts.is_empty(),
            "speech was detected, but the transcription backend returned no text"
        );
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
    let source = capture.source().to_owned();
    let mut endpointer = Endpointer::new(EndpointerConfig {
        end_silence_ms: config.end_silence_ms.clamp(100, 5000),
        min_speech_ms: config.min_speech_ms.clamp(50, 5000),
        ..Default::default()
    });
    let max_secs = config.max_recording_secs.clamp(1, 600);
    let deadline = Instant::now() + Duration::from_secs(max_secs);
    let sample_limit = max_secs as usize * super::wav::SAMPLE_RATE as usize;
    // Backend calls can block for minutes. Keep recording responsive to release/cancellation,
    // then decode only after capture is stopped. The sample cap bounds buffered speech too.
    let mut pending = Vec::new();
    let mut completed = false;
    let mut sample_count = 0usize;
    while !completed
        && sample_count < sample_limit
        && !finish.load(Ordering::SeqCst)
        && !abort.load(Ordering::SeqCst)
    {
        if Instant::now() >= deadline {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(samples)) => {
                let samples = &samples[..samples.len().min(sample_limit - sample_count)];
                sample_count += samples.len();
                for event in endpointer.push(samples) {
                    let EndpointerEvent::Segment(segment) = event;
                    pending.push(segment);
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
    stop_and_drain(
        &rx,
        || capture.stop(),
        abort,
        |chunk| {
            // Silence completion is already an endpoint; never admit beyond the hard cap.
            if completed || sample_count == sample_limit {
                return Ok(());
            }
            let samples =
                chunk.map_err(|error| anyhow::anyhow!("microphone capture failed: {error}"))?;
            let samples = &samples[..samples.len().min(sample_limit - sample_count)];
            sample_count += samples.len();
            for event in endpointer.push(samples) {
                let EndpointerEvent::Segment(segment) = event;
                pending.push(segment);
            }
            Ok(())
        },
    )?;
    ensure!(!abort.load(Ordering::SeqCst), "speech input was cancelled");
    if endpointer.in_speech() {
        // Finishing only ends recording: trailing speech is still transcribed, not aborted.
        for event in endpointer.flush() {
            let EndpointerEvent::Segment(segment) = event;
            pending.push(segment);
        }
    }
    let mut segments = Segments::default();
    for segment in pending {
        ensure!(!abort.load(Ordering::SeqCst), "speech input was cancelled");
        segments.add(env, &backend, &segment, abort)?;
    }
    ensure!(!abort.load(Ordering::SeqCst), "speech input was cancelled");
    segments.finish(sample_count, config.min_speech_ms.clamp(50, 5000), &source)
}

/// Stop and join the producer before draining, retaining chunks queued during shutdown as well.
fn stop_and_drain(
    rx: &Receiver<Chunk>,
    stop: impl FnOnce(),
    abort: &AtomicBool,
    mut consume: impl FnMut(Chunk) -> Result<()>,
) -> Result<()> {
    stop();
    for chunk in rx.try_iter() {
        ensure!(!abort.load(Ordering::SeqCst), "speech input was cancelled");
        consume(chunk)?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests;
