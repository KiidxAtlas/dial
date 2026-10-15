//! Protocol-scoped live voice sessions: spoken replies, mute, and one-shot listening.
use super::{VoiceEnv, speakable, tts};
use anyhow::{Result, ensure};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// One `live_start` session. The voice is fixed into its environment at start; the mute flag is
/// shared so every clone observes a later `live_mute`.
#[derive(Clone)]
pub struct LiveSession {
    env: VoiceEnv,
    instructions: String,
    muted: Arc<AtomicBool>,
}

/// Validates the requested voice and instructions and creates a session that speaks with them.
pub fn start(
    env: &VoiceEnv,
    voice: Option<&str>,
    instructions: Option<&str>,
) -> Result<LiveSession> {
    let mut env = env.clone();
    let voice = voice.map_or_else(|| env.voice.live.voice.clone(), str::to_owned);
    if !voice.is_empty() {
        ensure!(super::valid_voice(&voice), "invalid live voice name");
        env.voice.tts.voice = voice;
    }
    let instructions = instructions
        .unwrap_or(&env.voice.live.instructions)
        .trim()
        .to_owned();
    ensure!(
        instructions.len() <= 8192,
        "live voice instructions exceed 8192 bytes"
    );
    Ok(LiveSession {
        env,
        instructions,
        muted: Arc::new(AtomicBool::new(false)),
    })
}

impl LiveSession {
    /// The voice replies are spoken with (the configured default when none was requested).
    pub fn voice(&self) -> &str {
        &self.env.voice.tts.voice
    }
    /// The user's text with the session instructions in front, when any are set.
    pub fn prompt_for(&self, text: &str) -> String {
        let mut prompt = String::new();
        if !self.instructions.is_empty() {
            prompt.push_str(&self.instructions);
            prompt.push_str("\n\n");
        }
        prompt.push_str(text);
        prompt
    }
    pub fn mute(&self, muted: bool) {
        self.muted.store(muted, Ordering::SeqCst);
    }
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }
    /// Speaks one reply unless muted. Blocks until playback ends or `cancel` is set.
    pub fn speak(&self, text: &str, cancel: &Arc<AtomicBool>) -> Result<()> {
        if self.is_muted() {
            return Ok(());
        }
        let text = speakable::speakable(text, self.env.voice.tts.auto_speak_max_chars.max(1));
        tts::speak(&self.env, &text, cancel)
    }
    /// Records one utterance until trailing silence and returns its transcript.
    pub fn transcribe(&self, cancel: &Arc<AtomicBool>) -> Result<String> {
        super::dictation::record_and_transcribe(&self.env, cancel)
    }
}
