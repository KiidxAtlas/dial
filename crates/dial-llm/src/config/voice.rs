//! `[voice]` settings: speech output and input, live mode and their backends.

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

/// How the speech key starts and stops microphone capture.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum InputMode {
    /// Press once, speak, then pause or press again to finish.
    #[default]
    Toggle,
    /// Hold to record and release to finish; falls back to toggle without key releases.
    Hold,
}

/// When a dictated utterance is sent without pressing Enter.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SubmitTrigger {
    /// Insert the transcript and stay in the composer.
    Never,
    /// Send when recording finishes, including after trailing silence in toggle mode.
    #[default]
    Release,
    /// Send when recording finishes if the utterance ends with sentence punctuation.
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
    pub input_mode: InputMode,
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
            input_mode: InputMode::Toggle,
            submit_trigger: SubmitTrigger::Release,
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
