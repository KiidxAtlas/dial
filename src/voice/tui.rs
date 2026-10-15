//! Nonblocking push-to-talk controller shared by the terminal composer.
use super::{VoiceEnv, dictation};
use anyhow::{Result, ensure};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
};
use std::thread;

pub struct PushToTalk {
    stop: Arc<AtomicBool>,
    result: Receiver<Result<String>>,
}
impl PushToTalk {
    /// `hold` ends the recording only on [`stop`](Self::stop) (key release); otherwise trailing
    /// silence ends it.
    pub fn start(env: VoiceEnv, hold: bool) -> Result<Self> {
        ensure!(env.voice.stt.enabled, "speech recognition is disabled");
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (tx, result) = mpsc::channel();
        thread::Builder::new()
            .name("dial-dictation".into())
            .spawn(move || {
                let outcome = if hold {
                    dictation::record_held(&env, &worker_stop)
                } else {
                    dictation::record_and_transcribe(&env, &worker_stop)
                };
                let _ = tx.send(outcome);
            })?;
        Ok(Self { stop, result })
    }
    /// Release-to-stop; a pending speech segment is flushed and transcribed before delivery.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
    pub fn try_result(&self) -> Option<Result<String>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!(
                "dictation worker ended without a result"
            ))),
        }
    }
}
impl Drop for PushToTalk {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Speaks a finished answer on a background thread, cut to `auto_speak_max_chars` and stripped of
/// markdown. Setting `cancel` stops playback; the handle reports the first failure, if any.
pub fn speak_in_background(
    env: VoiceEnv,
    text: String,
    cancel: Arc<AtomicBool>,
) -> thread::JoinHandle<Result<()>> {
    thread::Builder::new()
        .name("dial-speak".into())
        .spawn(move || {
            let max = env.voice.tts.auto_speak_max_chars.max(1);
            let spoken = super::speakable::speakable(&text, max);
            super::tts::speak(&env, &spoken, &cancel)
        })
        .expect("spawn dial-speak thread")
}
