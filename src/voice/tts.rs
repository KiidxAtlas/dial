//! TTS backend resolution, safe command construction and audio playback.
use super::{
    TtsBackendChoice, VoiceEnv,
    process::{self, CommandSpec},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsBackend {
    Say(PathBuf),
    Piper(PathBuf),
    Espeak(PathBuf),
    OpenAi,
}

pub fn resolve_backend(env: &VoiceEnv) -> Result<TtsBackend> {
    let tts = &env.voice.tts;
    let say = || {
        process::which(&tts.say_program)
            .map(TtsBackend::Say)
            .context("macOS say is unavailable")
    };
    let piper = || {
        ensure!(
            tts.piper_model.as_ref().is_some_and(|p| p.is_file()),
            "Piper needs voice.tts.piper_model pointing to an ONNX model"
        );
        process::which(&tts.piper_program)
            .map(TtsBackend::Piper)
            .context("Piper is not installed")
    };
    let espeak = || {
        process::which(&tts.espeak_program)
            .map(TtsBackend::Espeak)
            .context("espeak-ng is not installed")
    };
    match tts.backend {
        TtsBackendChoice::Say => say(),
        TtsBackendChoice::Piper => piper(),
        TtsBackendChoice::Espeak => espeak(),
        TtsBackendChoice::Openai => {
            env.openai_key()?;
            Ok(TtsBackend::OpenAi)
        }
        TtsBackendChoice::Auto => {
            if cfg!(target_os = "macos")
                && let Ok(v) = say()
            {
                return Ok(v);
            }
            if let Ok(v) = piper() {
                return Ok(v);
            }
            if let Ok(v) = espeak() {
                return Ok(v);
            }
            if env.openai_available() {
                return Ok(TtsBackend::OpenAi);
            }
            bail!(
                "no TTS backend available: install say, Piper or espeak-ng, or configure an OpenAI audio key"
            )
        }
    }
}

pub fn command(backend: &TtsBackend, env: &VoiceEnv, output: &Path) -> Result<CommandSpec> {
    let cfg = &env.voice.tts;
    let voice = if !cfg.voice.is_empty() {
        &cfg.voice
    } else {
        &cfg.openai_voice
    };
    if !voice.is_empty() {
        ensure!(super::valid_voice(voice), "invalid TTS voice name");
    }
    Ok(match backend {
        TtsBackend::Say(program) => {
            let mut spec = CommandSpec::new(program)
                .arg("-f")
                .arg("-")
                .arg("-o")
                .arg(output);
            if !cfg.voice.is_empty() {
                spec = spec.arg("-v").arg(&cfg.voice);
            }
            spec.arg("-r")
                .arg((super::clamp_speed(cfg.speed) * 200.0).round().to_string())
        }
        TtsBackend::Piper(program) => {
            let mut spec = CommandSpec::new(program)
                .arg("--model")
                .arg(cfg.piper_model.as_ref().expect("resolved Piper model"));
            if !cfg.voice.is_empty() {
                spec = spec.arg("--speaker").arg(&cfg.voice);
            }
            spec.arg("--output_file").arg(output)
        }
        TtsBackend::Espeak(program) => {
            let mut spec = CommandSpec::new(program)
                .arg("--stdin")
                .arg("-w")
                .arg(output)
                .arg("-s")
                .arg((super::clamp_speed(cfg.speed) * 175.0).round().to_string());
            if !cfg.voice.is_empty() {
                spec = spec.arg("-v").arg(&cfg.voice);
            }
            spec
        }
        TtsBackend::OpenAi => bail!("OpenAI TTS is an HTTP backend, not a process command"),
    })
}

pub fn speak(env: &VoiceEnv, text: &str, cancel: &Arc<AtomicBool>) -> Result<()> {
    match speak_inner(env, text, cancel) {
        // A user-requested stop kills the child on purpose; it is not a failure.
        Err(_) if cancel.load(std::sync::atomic::Ordering::SeqCst) => Ok(()),
        result => result,
    }
}

fn speak_inner(env: &VoiceEnv, text: &str, cancel: &Arc<AtomicBool>) -> Result<()> {
    let text = text.trim();
    ensure!(!text.is_empty(), "nothing to speak");
    let limit = env.voice.tts.auto_speak_max_chars.max(1);
    let text = match text.char_indices().nth(limit) {
        Some((end, _)) => &text[..end],
        None => text,
    };
    let backend = resolve_backend(env)?;
    let temp = tempfile::Builder::new().prefix("dial-tts-").tempdir()?;
    // `say -o` infers the container from the file extension, so each backend writes its real format.
    let audio = temp.path().join(match backend {
        TtsBackend::Say(_) => "speech.aiff",
        TtsBackend::OpenAi => "speech.mp3",
        TtsBackend::Piper(_) | TtsBackend::Espeak(_) => "speech.wav",
    });
    match backend {
        TtsBackend::OpenAi => {
            let cfg = &env.voice.tts;
            let key = env.openai_key()?;
            let voice = if cfg.voice.is_empty() {
                &cfg.openai_voice
            } else {
                &cfg.voice
            };
            let agent = crate::llm::agent_with(
                env.voice.openai.timeout_secs,
                true,
                Some(cancel.clone()),
                None,
            );
            let mut response = agent
                .post(&env.openai_url("audio/speech"))
                .header("authorization", format!("Bearer {key}"))
                .send_json(json!({"model":cfg.openai_model,"voice":voice,"input":text,"speed":super::clamp_speed(cfg.speed),"response_format":"mp3"}))
                .context("requesting OpenAI TTS audio")?;
            let status = response.status().as_u16();
            let bytes = response
                .body_mut()
                .read_to_vec()
                .context("reading TTS audio")?;
            ensure!(
                (200..300).contains(&status),
                "OpenAI TTS failed (HTTP {status})"
            );
            std::fs::write(&audio, bytes)?;
        }
        local => {
            let spec = command(&local, env, &audio)?;
            process::run(
                &spec,
                Some(text.as_bytes()),
                cancel,
                Duration::from_secs(180),
            )
            .context("synthesizing speech")?;
        }
    }
    if cancel.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(());
    }
    let configured = &env.voice.tts.player;
    let (program, args) = if let Some((first, rest)) = configured.split_first() {
        (PathBuf::from(first), rest.to_vec())
    } else {
        let player = ["afplay", "paplay", "aplay", "pw-play", "ffplay"]
            .iter()
            .find_map(|p| process::which(Path::new(p)))
            .context("no audio player installed; configure voice.tts.player")?;
        (player, Vec::new())
    };
    let mut playback = CommandSpec::new(program);
    for arg in args {
        playback = playback.arg(arg);
    }
    playback = playback.arg(&audio);
    process::run(&playback, None, cancel, Duration::from_secs(300))
        .context("playing synthesized speech")?;
    Ok(())
}

#[cfg(test)]
mod tests;
