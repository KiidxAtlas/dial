//! Voice input and output: text-to-speech (the `tts` tool and spoken replies), push-to-talk
//! dictation into the composer, and live voice sessions for protocol clients.
//!
//! Audio I/O is optional and detected at runtime. Native microphone capture uses `cpal` behind
//! the `voice` cargo feature on macOS (CoreAudio needs no extra system libraries); everywhere else
//! the microphone is read through an installed recorder program (`arecord`, `parecord`, `sox`,
//! `ffmpeg`). Speech synthesis and transcription run through installed programs (`say`, `piper`,
//! `espeak-ng`, `whisper-cli`, `mlx_whisper`) or the OpenAI audio API, so a missing backend is a
//! clear runtime error, never a build failure.
//!
//! Configured programs are operator-owned, like hooks and MCP servers: they come from the user or
//! per-project state configuration (both outside the workspace) and receive model-supplied text
//! only on stdin, never as arguments.
use crate::config::voice::VoiceConfig;

pub mod capture;
pub mod dictation;
pub mod endpointer;
pub mod gesture;
pub mod live;
mod process;
pub mod speakable;
pub mod stt;
pub mod tool;
pub mod tts;
pub mod tui;
pub mod wav;

use std::path::PathBuf;

/// Everything voice operations need, detached from the full configuration so worker threads can
/// own a copy.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VoiceEnv {
    pub voice: VoiceConfig,
    /// Dial home, for credentials saved through `/providers`.
    pub user_dir: PathBuf,
}

impl VoiceEnv {
    pub fn from_config(config: &crate::config::types::Config) -> Self {
        Self {
            voice: config.voice.clone(),
            user_dir: config.user_dir.clone(),
        }
    }

    /// The API key for the OpenAI audio endpoint, if one is configured.
    pub(crate) fn openai_key(&self) -> anyhow::Result<String> {
        let openai = &self.voice.openai;
        crate::llm::catalog::resolve_credential(
            &self.user_dir,
            openai.base_url.trim_end_matches('/'),
            openai.api_key_env.as_deref(),
        )?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no API key for {}; set {} or save a key through /providers",
                openai.base_url,
                openai.api_key_env.as_deref().unwrap_or("OPENAI_API_KEY")
            )
        })
    }

    pub(crate) fn openai_available(&self) -> bool {
        self.openai_key().is_ok()
    }

    pub(crate) fn openai_url(&self, path: &str) -> String {
        format!(
            "{}/{path}",
            self.voice.openai.base_url.trim_end_matches('/')
        )
    }
}

/// Program availability for the settings panel, as `(setting path, hint)` pairs. Only probes PATH and
/// the OpenAI key presence; no speech program is run.
pub fn settings_hints(config: &crate::config::types::Config) -> Vec<(&'static str, String)> {
    let env = VoiceEnv::from_config(config);
    let found = |program: &std::path::Path| -> &'static str {
        if process::which(program).is_some() {
            "found"
        } else {
            "not on PATH"
        }
    };
    let say = if cfg!(target_os = "macos") {
        "built in"
    } else {
        found(&env.voice.tts.say_program)
    };
    let tts = &env.voice.tts;
    let stt = &env.voice.stt;
    let tts_list = format!(
        "say {say} · piper {} · espeak-ng {}",
        found(&tts.piper_program),
        found(&tts.espeak_program)
    );
    let stt_list = format!(
        "whisper-cpp {} · mlx-whisper {}",
        found(&stt.whisper_cpp_program),
        found(&stt.mlx_whisper_program)
    );
    let tts_ready = say == "built in"
        || say == "found"
        || found(&tts.piper_program) == "found"
        || found(&tts.espeak_program) == "found"
        || env.openai_available();
    vec![
        ("voice.tts.backend", format!("Programs: {tts_list}")),
        (
            "voice.tts.auto_speak",
            if tts_ready {
                "A speech backend is available".to_owned()
            } else {
                "No speech backend yet: Ctrl+L can set one up, or set an OpenAI key".to_owned()
            },
        ),
        ("voice.stt.enabled", format!("Programs: {stt_list}")),
        ("voice.stt.backend", format!("Programs: {stt_list}")),
    ]
}

/// A voice name a backend may receive as an argument: short, printable, never option-like.
pub(crate) fn valid_voice(voice: &str) -> bool {
    !voice.is_empty()
        && voice.len() <= 64
        && !voice.starts_with('-')
        && voice.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '-' | '.' | '+' | '(' | ')' | ':')
        })
}

/// Clamp a speaking rate to the supported range; non-finite values mean normal speed.
pub(crate) fn clamp_speed(speed: f32) -> f32 {
    if speed.is_finite() {
        speed.clamp(0.5, 2.5)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests;
