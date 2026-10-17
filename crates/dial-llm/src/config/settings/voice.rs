//! Voice setting declarations. Defaults derive from typed configuration fields.
use super::*;
pub(super) static DECLARATIONS: &[SettingDef] = &[
    field!(
        "voice.tts.backend",
        voice.tts.backend,
        VO,
        "Read-aloud",
        "Speech program",
        "Program that reads answers aloud. Auto uses the first one installed: macOS say (built in), Piper, espeak-ng, or OpenAI with an API key.",
        Kind::Enum(TTS_BACKENDS),
    ),
    field!(
        "voice.tts.auto_speak",
        voice.tts.auto_speak,
        VO,
        "Read-aloud",
        "Read answers aloud",
        "Read each final answer aloud when a turn finishes. Press Esc or send a new prompt to stop.",
        Kind::Bool,
    ),
    field!(
        "voice.stt.enabled",
        voice.stt.enabled,
        VO,
        "Dictation",
        "Dictation",
        "Allow the dictation shortcut to record speech and insert the transcript into the composer. See /hotkeys for the active binding. Needs a recognition program: whisper.cpp, MLX Whisper, or OpenAI with an API key.",
        Kind::Bool,
    ),
    field!(
        "voice.stt.backend",
        voice.stt.backend,
        VO,
        "Dictation",
        "Recognition program",
        "Program that turns speech into text. Auto uses the first one installed: whisper-cli (whisper.cpp), mlx-whisper (pip install mlx-whisper), or OpenAI with an API key.",
        Kind::Enum(STT_BACKENDS),
    ),
];
