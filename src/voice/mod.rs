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

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `[voice]` configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct VoiceConfig {
    pub tts: TtsConfig,
    pub stt: SttConfig,
    pub live: LiveConfig,
    pub openai: OpenAiAudioConfig,
}

/// OpenAI-compatible audio endpoint shared by speech synthesis and transcription.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiAudioConfig {
    /// Base URL of an OpenAI-compatible API (`/audio/speech`, `/audio/transcriptions`).
    pub base_url: String,
    /// Environment variable holding the API key. Unset uses the provider's standard variable
    /// (`OPENAI_API_KEY` for api.openai.com) or a key saved through `/providers`.
    pub api_key_env: Option<String>,
    /// Per request timeout in seconds.
    pub timeout_secs: u64,
}

impl Default for OpenAiAudioConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".into(),
            api_key_env: None,
            timeout_secs: 120,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TtsBackendChoice {
    /// First available of: `say` (macOS), `piper` (with a model), `espeak-ng`, OpenAI.
    #[default]
    Auto,
    Say,
    Piper,
    Espeak,
    Openai,
}

/// `[voice.tts]`: speech synthesis.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TtsConfig {
    pub backend: TtsBackendChoice,
    /// Backend voice: a `say -v` name, an `espeak-ng -v` voice, a piper speaker id, or an
    /// OpenAI voice. Empty uses the backend default.
    pub voice: String,
    /// Speaking rate, 1 = normal; clamped to 0.5..2.5.
    pub speed: f32,
    pub say_program: PathBuf,
    pub piper_program: PathBuf,
    /// Piper voice model (`.onnx`, with its `.onnx.json` beside it). Piper is skipped without one.
    pub piper_model: Option<PathBuf>,
    pub espeak_program: PathBuf,
    pub openai_model: String,
    pub openai_voice: String,
    /// Playback command; the audio file path is appended. Empty detects `afplay`, `paplay`,
    /// `aplay`, `pw-play` or `ffplay`.
    pub player: Vec<String>,
    /// Speak each final answer aloud in the terminal UI.
    pub auto_speak: bool,
    /// Longest spoken reply in characters; longer replies are cut at a sentence boundary.
    pub auto_speak_max_chars: usize,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            backend: TtsBackendChoice::Auto,
            voice: String::new(),
            speed: 1.0,
            say_program: "say".into(),
            piper_program: "piper".into(),
            piper_model: None,
            espeak_program: "espeak-ng".into(),
            openai_model: "gpt-4o-mini-tts".into(),
            openai_voice: "alloy".into(),
            player: Vec::new(),
            auto_speak: false,
            auto_speak_max_chars: 4000,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SttBackendChoice {
    /// First available of: whisper.cpp (with a model), mlx-whisper, OpenAI.
    #[default]
    Auto,
    Openai,
    WhisperCpp,
    MlxWhisper,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureChoice {
    /// Native capture when compiled in, otherwise the first installed recorder program.
    #[default]
    Auto,
    Native,
    Command,
}

/// When a dictated utterance is sent without pressing Enter.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SubmitTrigger {
    /// Insert the transcript and stay in the composer.
    #[default]
    Never,
    /// Send on release when the utterance has two or more words.
    Release,
    /// Send on release when the utterance ends with sentence punctuation.
    ReleaseComplete,
    /// Send when the utterance ends with the word "submit" (the word is removed).
    SaySubmit,
}

/// `[voice.stt]`: speech-to-text for push-to-talk and live sessions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SttConfig {
    /// Allows push-to-talk in the composer (the `app.voice.input` key, `Alt+V` by default).
    pub enabled: bool,
    pub backend: SttBackendChoice,
    /// Spoken language (ISO 639-1), or `auto`.
    pub language: String,
    pub submit_trigger: SubmitTrigger,
    pub openai_model: String,
    pub whisper_cpp_program: PathBuf,
    /// ggml model for whisper.cpp; whisper.cpp is skipped without one.
    pub whisper_cpp_model: Option<PathBuf>,
    pub mlx_whisper_program: PathBuf,
    /// Hugging Face repo or local directory of an MLX Whisper model.
    pub mlx_whisper_model: String,
    pub capture: CaptureChoice,
    /// Recorder command writing raw signed 16-bit little-endian 16 kHz mono PCM to stdout.
    pub capture_command: Vec<String>,
    /// Hard stop for one push-to-talk recording.
    pub max_recording_secs: u64,
    /// Trailing silence that ends one utterance.
    pub end_silence_ms: u32,
    /// Shortest speech kept; shorter sounds are treated as noise.
    pub min_speech_ms: u32,
    /// Transcription timeout per utterance.
    pub timeout_secs: u64,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: SttBackendChoice::Auto,
            language: "en".into(),
            submit_trigger: SubmitTrigger::Never,
            openai_model: "whisper-1".into(),
            whisper_cpp_program: "whisper-cli".into(),
            whisper_cpp_model: None,
            mlx_whisper_program: "mlx_whisper".into(),
            mlx_whisper_model: "mlx-community/whisper-large-v3-turbo".into(),
            capture: CaptureChoice::Auto,
            capture_command: Vec::new(),
            max_recording_secs: 120,
            end_silence_ms: 600,
            min_speech_ms: 200,
            timeout_secs: 120,
        }
    }
}

/// `[voice.live]`: hands-free voice sessions started by protocol clients (`live_start`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct LiveConfig {
    /// Voice for spoken replies; empty uses `voice.tts.voice`.
    pub voice: String,
    /// Extra guidance added to every prompt sent during a live session; empty adds none.
    pub instructions: String,
}

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
