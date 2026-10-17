//! Agent-facing text-to-speech tool.
use super::{VoiceEnv, tts};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::sync::{Arc, atomic::AtomicBool};

pub fn schema() -> Value {
    serde_json::json!({"type":"function","function":{"name":"tts","description":"Speak a short message aloud using the configured text-to-speech backend.","parameters":{"type":"object","properties":{"text":{"type":"string","description":"Text to speak aloud"}},"required":["text"],"additionalProperties":false}}})
}

pub fn execute(env: &VoiceEnv, args: &Value, cancel: &Arc<AtomicBool>) -> Result<String> {
    let text = args["text"].as_str().unwrap_or("").trim();
    ensure!(!text.is_empty(), "tts requires a non-empty text field");
    ensure!(text.len() <= 16_000, "tts text exceeds 16000 bytes");
    tts::speak(env, text, cancel)?;
    if cancel.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok("Speech was stopped before it finished.".into());
    }
    Ok("Speech played successfully.".into())
}
