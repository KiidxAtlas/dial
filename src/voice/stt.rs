//! Speech-to-text backends: local whisper.cpp, local mlx-whisper, or the OpenAI transcription API.

use super::{
    SttBackendChoice, VoiceEnv,
    process::{self, CommandSpec},
    wav,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SttBackend {
    WhisperCpp { program: PathBuf, model: PathBuf },
    MlxWhisper { program: PathBuf },
    OpenAi,
}

/// Pick the configured backend, or in `auto` the first available local one, then OpenAI.
pub fn resolve_backend(env: &VoiceEnv) -> Result<SttBackend> {
    let stt = &env.voice.stt;
    let whisper_cpp = || -> Result<SttBackend> {
        let model = stt
            .whisper_cpp_model
            .clone()
            .context("whisper.cpp needs voice.stt.whisper_cpp_model (a ggml model file)")?;
        ensure!(
            model.is_file(),
            "whisper.cpp model {} does not exist",
            model.display()
        );
        let program = process::which(&stt.whisper_cpp_program).with_context(|| {
            format!(
                "{} is not installed (whisper.cpp)",
                stt.whisper_cpp_program.display()
            )
        })?;
        Ok(SttBackend::WhisperCpp { program, model })
    };
    let mlx = || -> Result<SttBackend> {
        let program = process::which(&stt.mlx_whisper_program).with_context(|| {
            format!(
                "{} is not installed (pip install mlx-whisper)",
                stt.mlx_whisper_program.display()
            )
        })?;
        Ok(SttBackend::MlxWhisper { program })
    };
    match stt.backend {
        SttBackendChoice::WhisperCpp => whisper_cpp(),
        SttBackendChoice::MlxWhisper => mlx(),
        SttBackendChoice::Openai => {
            env.openai_key()?;
            Ok(SttBackend::OpenAi)
        }
        SttBackendChoice::Auto => {
            if let Ok(backend) = whisper_cpp() {
                return Ok(backend);
            }
            if cfg!(all(target_os = "macos", target_arch = "aarch64"))
                && let Ok(backend) = mlx()
            {
                return Ok(backend);
            }
            if env.openai_available() {
                return Ok(SttBackend::OpenAi);
            }
            bail!(
                "no speech-to-text backend: set voice.stt.whisper_cpp_model for whisper.cpp, install mlx-whisper (Apple Silicon), or provide an OpenAI API key"
            )
        }
    }
}

/// `whisper-cli -m MODEL -f WAV -l LANG -nt -np`: transcript only, on stdout.
pub fn whisper_cpp_command(
    program: &Path,
    model: &Path,
    audio: &Path,
    language: &str,
) -> CommandSpec {
    CommandSpec::new(program)
        .arg("-m")
        .arg(model)
        .arg("-f")
        .arg(audio)
        .arg("-l")
        .arg(language_or_auto(language))
        .arg("-nt")
        .arg("-np")
}

/// `mlx_whisper WAV --model M --output-format txt --output-dir DIR --output-name NAME --verbose False`.
pub fn mlx_whisper_command(
    program: &Path,
    model: &str,
    audio: &Path,
    language: &str,
    out_dir: &Path,
    out_name: &str,
) -> CommandSpec {
    let mut spec = CommandSpec::new(program)
        .arg(audio)
        .arg("--model")
        .arg(model)
        .arg("--output-format")
        .arg("txt")
        .arg("--output-dir")
        .arg(out_dir)
        .arg("--output-name")
        .arg(out_name)
        .arg("--verbose")
        .arg("False");
    let language = language_or_auto(language);
    if language != "auto" {
        spec = spec.arg("--language").arg(language);
    }
    spec
}

/// A BCP-47-like language code, or `auto`. A leading `-` is rejected so a configured value can never
/// be parsed as a flag by the external transcriber.
fn language_or_auto(language: &str) -> &str {
    let trimmed = language.trim();
    if trimmed.is_empty()
        || trimmed.len() > 12
        || !trimmed.starts_with(|c: char| c.is_ascii_alphanumeric())
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        "auto"
    } else {
        trimmed
    }
}

/// A `multipart/form-data` body with text `fields` and one WAV file part.
pub fn multipart_body(
    boundary: &str,
    fields: &[(&str, &str)],
    file_name: &str,
    file: &[u8],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(file.len() + 512);
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// Transcribe 16 kHz mono samples. Returns trimmed text (possibly empty for non-speech).
pub fn transcribe(
    env: &VoiceEnv,
    backend: &SttBackend,
    samples: &[f32],
    cancel: &Arc<AtomicBool>,
) -> Result<String> {
    let stt = &env.voice.stt;
    let wav_bytes = wav::encode_pcm16(samples, wav::SAMPLE_RATE);
    let timeout = Duration::from_secs(stt.timeout_secs.clamp(5, 3600));
    let text = match backend {
        SttBackend::WhisperCpp { program, model } => {
            let dir = tempfile::tempdir().context("creating a scratch directory for audio")?;
            let audio = dir.path().join("utterance.wav");
            std::fs::write(&audio, &wav_bytes)?;
            let spec = whisper_cpp_command(program, model, &audio, &stt.language);
            let done = process::run(&spec, None, cancel, timeout)?;
            let text = String::from_utf8_lossy(&done.stdout).into_owned();
            if text.trim().is_empty() && !done.stderr.is_empty() {
                bail!("whisper.cpp produced no transcript: {}", done.stderr);
            }
            text
        }
        SttBackend::MlxWhisper { program } => {
            let dir = tempfile::tempdir().context("creating a scratch directory for audio")?;
            let audio = dir.path().join("utterance.wav");
            std::fs::write(&audio, &wav_bytes)?;
            let spec = mlx_whisper_command(
                program,
                &stt.mlx_whisper_model,
                &audio,
                &stt.language,
                dir.path(),
                "transcript",
            );
            process::run(&spec, None, cancel, timeout)?;
            std::fs::read_to_string(dir.path().join("transcript.txt"))
                .context("mlx_whisper did not write a transcript")?
        }
        SttBackend::OpenAi => openai_transcribe(env, &wav_bytes, cancel.clone(), timeout)?,
    };
    Ok(clean_transcript(&text))
}

fn openai_transcribe(
    env: &VoiceEnv,
    wav_bytes: &[u8],
    cancel: Arc<AtomicBool>,
    timeout: Duration,
) -> Result<String> {
    let key = env.openai_key()?;
    let stt = &env.voice.stt;
    let boundary = format!("dial-voice-{:016x}", rand::random::<u64>());
    let language = language_or_auto(&stt.language);
    let mut fields = vec![
        ("model", stt.openai_model.as_str()),
        ("response_format", "json"),
    ];
    if language != "auto" {
        fields.push(("language", language));
    }
    let body = multipart_body(&boundary, &fields, "utterance.wav", wav_bytes);
    let agent = crate::llm::agent_with(timeout.as_secs(), true, Some(cancel), None);
    let mut response = agent
        .post(&env.openai_url("audio/transcriptions"))
        .header("Authorization", format!("Bearer {key}"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .send(&body[..])
        .map_err(|e| anyhow::anyhow!("transcription request failed: {e}"))?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(4 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .context("reading the transcription response")?;
    if !(200..300).contains(&status) {
        bail!(
            "transcription failed (HTTP {status}): {}",
            api_error(&bytes)
        );
    }
    let value: Value =
        serde_json::from_slice(&bytes).context("transcription response is not JSON")?;
    Ok(value["text"].as_str().unwrap_or_default().to_owned())
}

/// The `error.message` of an OpenAI-style error body, or a short excerpt.
pub(crate) fn api_error(bytes: &[u8]) -> String {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| {
            String::from_utf8_lossy(&bytes[..bytes.len().min(300)])
                .trim()
                .to_owned()
        })
}

/// Collapse whitespace and drop the bracketed non-speech markers Whisper emits for silence.
pub fn clean_transcript(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    let trimmed = out.trim();
    let marker = |s: &str| {
        (s.starts_with('[') && s.ends_with(']')) || (s.starts_with('(') && s.ends_with(')'))
    };
    if marker(trimmed) && !trimmed[1..trimmed.len() - 1].contains(['[', '(']) {
        return String::new();
    }
    trimmed.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whisper_cpp_command_prints_plain_transcript() {
        let spec = whisper_cpp_command(
            Path::new("/opt/whisper-cli"),
            Path::new("/m/ggml-base.en.bin"),
            Path::new("/t/u.wav"),
            "en",
        );
        assert_eq!(spec.program, Path::new("/opt/whisper-cli"));
        assert_eq!(
            spec.arg_strings(),
            [
                "-m",
                "/m/ggml-base.en.bin",
                "-f",
                "/t/u.wav",
                "-l",
                "en",
                "-nt",
                "-np"
            ]
        );
        let auto = whisper_cpp_command(Path::new("w"), Path::new("m"), Path::new("a"), "--evil");
        assert!(auto.arg_strings().windows(2).any(|w| w == ["-l", "auto"]));
    }

    #[test]
    fn mlx_whisper_command_writes_named_txt() {
        let spec = mlx_whisper_command(
            Path::new("mlx_whisper"),
            "mlx-community/whisper-large-v3-turbo",
            Path::new("/t/u.wav"),
            "de",
            Path::new("/t"),
            "transcript",
        );
        assert_eq!(
            spec.arg_strings(),
            [
                "/t/u.wav",
                "--model",
                "mlx-community/whisper-large-v3-turbo",
                "--output-format",
                "txt",
                "--output-dir",
                "/t",
                "--output-name",
                "transcript",
                "--verbose",
                "False",
                "--language",
                "de"
            ]
        );
        let auto = mlx_whisper_command(
            Path::new("m"),
            "x",
            Path::new("a"),
            "auto",
            Path::new("/t"),
            "n",
        );
        assert!(!auto.arg_strings().contains(&"--language".to_owned()));
    }

    #[test]
    fn multipart_body_is_well_formed() {
        let body = multipart_body("B", &[("model", "whisper-1")], "u.wav", b"RIFF");
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with(
            "--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n"
        ));
        assert!(text.contains(
            "name=\"file\"; filename=\"u.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF\r\n--B--\r\n"
        ));
    }

    #[test]
    fn transcripts_are_cleaned() {
        assert_eq!(clean_transcript("  hello \n world  "), "hello world");
        assert_eq!(clean_transcript("[BLANK_AUDIO]"), "");
        assert_eq!(clean_transcript(" (silence) "), "");
        assert_eq!(clean_transcript("[a] and [b]"), "[a] and [b]");
    }

    #[test]
    fn explicit_backends_report_missing_prerequisites() {
        let mut env = VoiceEnv::default();
        env.voice.stt.backend = SttBackendChoice::WhisperCpp;
        let error = resolve_backend(&env).unwrap_err().to_string();
        assert!(error.contains("whisper_cpp_model"), "{error}");
        env.voice.stt.backend = SttBackendChoice::MlxWhisper;
        env.voice.stt.mlx_whisper_program = "definitely-not-mlx-whisper".into();
        let error = resolve_backend(&env).unwrap_err().to_string();
        assert!(error.contains("not installed"), "{error}");
    }
}
