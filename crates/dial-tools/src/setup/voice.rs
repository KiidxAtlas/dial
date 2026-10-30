//! Preparation for an explicitly confirmed voice backend selection. Returns runtime/model
//! settings for the caller to commit atomically with its selector; never writes settings.
use super::deps::{self, plan::Step, run};
use crate::config::voice::{SttBackendChoice, TtsBackendChoice};
use crate::config::{paths::Layout, settings_store::Change, types::Config};
use crate::voice::{VoiceEnv, process, stt, tts};
use anyhow::{Context, Result, ensure};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

const PIPER_VOICE: &str = "en_US-lessac-medium";

/// Recipe ids differ from the serialized backend choices (`espeak` in particular).
pub fn recipe_for(path: &str, value: &str) -> Option<&'static str> {
    match (path, value) {
        ("voice.tts.backend", "piper") => Some("piper"),
        ("voice.tts.backend", "espeak") => Some("espeak-ng"),
        ("voice.stt.backend", "whisper-cpp") => Some("whisper-cpp"),
        ("voice.stt.backend", "mlx-whisper") => Some("mlx-whisper"),
        _ => None,
    }
}

fn candidate(config: &Config, id: &str) -> VoiceEnv {
    let mut env = VoiceEnv::from_config(config);
    match id {
        "piper" => env.voice.tts.backend = TtsBackendChoice::Piper,
        "espeak-ng" => env.voice.tts.backend = TtsBackendChoice::Espeak,
        "whisper-cpp" => env.voice.stt.backend = SttBackendChoice::WhisperCpp,
        "mlx-whisper" => env.voice.stt.backend = SttBackendChoice::MlxWhisper,
        _ => unreachable!("validated recipe"),
    }
    env
}

fn nonempty(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.len() > 0)
}

/// Piper's resolver checks only the ONNX path; its runtime also requires this matching sidecar.
fn piper_model_ready(path: &Path) -> bool {
    let json = path.with_added_extension("json");
    nonempty(path)
        && std::fs::read(&json).ok().is_some_and(|bytes| {
            serde_json::from_slice::<serde_json::Value>(&bytes).is_ok_and(|v| {
                v.get("audio")
                    .and_then(|a| a.get("sample_rate"))
                    .and_then(|v| v.as_u64())
                    .is_some_and(|rate| rate > 0)
                    && v.get("phoneme_id_map").is_some_and(|v| v.is_object())
            })
        })
}

fn backend_ready(env: &VoiceEnv, id: &str) -> bool {
    match id {
        "piper" => {
            tts::resolve_backend(env).is_ok()
                && env
                    .voice
                    .tts
                    .piper_model
                    .as_deref()
                    .is_some_and(piper_model_ready)
        }
        "espeak-ng" => tts::resolve_backend(env).is_ok(),
        "whisper-cpp" => {
            stt::resolve_backend(env).is_ok()
                && env
                    .voice
                    .stt
                    .whisper_cpp_model
                    .as_deref()
                    .is_some_and(nonempty)
        }
        "mlx-whisper" => {
            stt::resolve_backend(env).is_ok()
                && mlx_model_ready(&env.voice.stt.mlx_whisper_model, hf_cache().as_deref())
        }
        _ => false,
    }
}

/// Read-only readiness for an exact serialized selection, including Piper's JSON sidecar.
pub fn ready(config: &Config, path: &str, value: &str) -> Result<()> {
    let mut env = VoiceEnv::from_config(config);
    match path {
        "voice.tts.backend" => {
            env.voice.tts.backend =
                serde_json::from_value(serde_json::Value::String(value.into()))?;
            tts::resolve_backend(&env)?;
        }
        "voice.stt.backend" => {
            env.voice.stt.backend =
                serde_json::from_value(serde_json::Value::String(value.into()))?;
            ensure!(
                env.voice.stt.backend != SttBackendChoice::MlxWhisper
                    || cfg!(all(target_os = "macos", target_arch = "aarch64")),
                "mlx-whisper requires macOS on Apple Silicon"
            );
            stt::resolve_backend(&env)?;
        }
        _ => anyhow::bail!("not a voice backend setting"),
    }
    if let Some(id) = recipe_for(path, value) {
        ensure!(
            backend_ready(&env, id),
            "voice backend needs a nonempty model (Piper also needs a valid matching .onnx.json file)"
        );
    }
    Ok(())
}

/// Prepare runtime/model paths and return their settings, without saving any configuration.
/// Must run only inside an approved setup job: it may install packages and download weights.
pub fn prepare(
    layout: &Layout,
    config: &Config,
    path: &str,
    value: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<Vec<Change>> {
    ensure!(!cancel.load(Ordering::Relaxed), "voice setup cancelled");
    let id = recipe_for(path, value).context("this voice backend has no automatic installer")?;
    ensure!(
        id != "mlx-whisper" || cfg!(all(target_os = "macos", target_arch = "aarch64")),
        "mlx-whisper requires macOS on Apple Silicon; choose whisper-cpp or OpenAI instead"
    );
    let mut env = candidate(config, id);
    if backend_ready(&env, id) {
        progress("voice backend is already ready");
        return Ok(vec![]);
    }
    let (program, default, setting) = match id {
        "piper" => (
            &env.voice.tts.piper_program,
            "piper",
            "voice.tts.piper_program",
        ),
        "espeak-ng" => (
            &env.voice.tts.espeak_program,
            "espeak-ng",
            "voice.tts.espeak_program",
        ),
        "whisper-cpp" => (
            &env.voice.stt.whisper_cpp_program,
            "whisper-cli",
            "voice.stt.whisper_cpp_program",
        ),
        "mlx-whisper" => (
            &env.voice.stt.mlx_whisper_program,
            "mlx_whisper",
            "voice.stt.mlx_whisper_program",
        ),
        _ => unreachable!(),
    };
    let location = if let Some(location) = process::which(program) {
        location
    } else {
        ensure!(
            program == Path::new(default),
            "configured executable {} is unavailable; fix {setting} or reset it to {default} before installing",
            program.display()
        );
        let deps_env = deps::Env::current(&layout.user_dir);
        if let Some(location) = deps::locate(deps::recipe(id).expect("known recipe"), &deps_env) {
            location
        } else {
            // Installation returns its executable without changing any settings layer.
            run::install_with_requirements(id, &deps_env, cancel, progress)?
        }
    };
    let mut changes = Vec::new();
    match id {
        "piper" => {
            env.voice.tts.piper_program = location.clone();
            let model = match env.voice.tts.piper_model.as_deref() {
                Some(model) if nonempty(model) => {
                    ensure!(
                        piper_model_ready(model),
                        "Piper model {} needs a valid matching .onnx.json file; repair the configured model before selecting Piper",
                        model.display()
                    );
                    model.to_path_buf()
                }
                _ => ensure_piper_model(layout, &location, cancel, progress)?,
            };
            env.voice.tts.piper_model = Some(model.clone());
            // Load ONNX and its config through the installed runtime, without synthesizing audio.
            // https://github.com/OHF-Voice/piper1-gpl/blob/main/src/piper/voice.py
            run::run_step(
                &Step {
                    program: adjacent_python(&location)?.display().to_string(),
                    args: vec![
                        "-c".into(),
                        "import sys; from piper.voice import PiperVoice; PiperVoice.load(sys.argv[1], download_dir=sys.argv[2])".into(),
                        model.display().to_string(),
                        layout.user_dir.join("models/piper").display().to_string(),
                    ],
                    env: vec![],
                },
                cancel,
                progress,
            )?;
            changes.push(Change::Set {
                path: "voice.tts.piper_model".into(),
                value: toml::Value::String(model.display().to_string()),
            });
        }
        "whisper-cpp" => {
            env.voice.stt.whisper_cpp_program = location.clone();
            let model = match env.voice.stt.whisper_cpp_model.as_deref() {
                Some(model) if nonempty(model) => model.to_path_buf(),
                _ => run::ensure_whisper_model(&layout.user_dir, cancel, progress)?,
            };
            progress(&run::health(id, &location, Some(&model))?);
            env.voice.stt.whisper_cpp_model = Some(model.clone());
            changes.push(Change::Set {
                path: "voice.stt.whisper_cpp_model".into(),
                value: toml::Value::String(model.display().to_string()),
            });
        }
        "espeak-ng" => {
            progress(&run::health(id, &location, None)?);
            env.voice.tts.espeak_program = location.clone();
        }
        "mlx-whisper" => {
            let python = adjacent_python(&location)?;
            // Upstream's loader resolves local directories or downloads a repository to the
            // standard Hugging Face cache, then validates and loads its weights without audio.
            // https://github.com/ml-explore/mlx-examples/blob/main/whisper/mlx_whisper/load_models.py
            progress("preparing the configured MLX Whisper model (may download weights)");
            run::run_step(
                &Step {
                    program: python.display().to_string(),
                    args: vec![
                        "-c".into(),
                        "import sys; from mlx_whisper.load_models import load_model; load_model(sys.argv[1])".into(),
                        env.voice.stt.mlx_whisper_model.clone(),
                    ],
                    env: vec![],
                },
                cancel,
                progress,
            )?;
            env.voice.stt.mlx_whisper_program = location.clone();
        }
        _ => unreachable!(),
    }
    ensure!(!cancel.load(Ordering::Relaxed), "voice setup cancelled");
    ensure!(
        backend_ready(&env, id),
        "voice backend is still unavailable after setup"
    );
    changes.push(Change::Set {
        path: setting.into(),
        value: toml::Value::String(location.display().to_string()),
    });
    Ok(changes)
}

fn ensure_piper_model(
    layout: &Layout,
    program: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<PathBuf> {
    let dir = layout.user_dir.join("models/piper");
    let model = dir.join(format!("{PIPER_VOICE}.onnx"));
    if piper_model_ready(&model) {
        return Ok(model);
    }
    let python = adjacent_python(program)?;
    // Official piper.download_voices CLI downloads both ONNX and its matching JSON. An explicit
    // directory is essential: its default is the current working directory (the user's project).
    // https://github.com/OHF-Voice/piper1-gpl/blob/main/src/piper/download_voices.py
    std::fs::create_dir_all(&dir)?;
    run::run_step(
        &Step {
            program: python.display().to_string(),
            args: vec![
                "-m".into(),
                "piper.download_voices".into(),
                "--download-dir".into(),
                dir.display().to_string(),
                PIPER_VOICE.into(),
            ],
            env: vec![],
        },
        cancel,
        progress,
    )?;
    ensure!(
        piper_model_ready(&model),
        "Piper download did not produce a usable ONNX model and matching JSON"
    );
    Ok(model)
}

// The catalog's cache helper is crate-private and its snapshot validator is for language
// models. Whisper also supports weights.npz, so validate only Whisper's documented layout.
fn hf_cache() -> Option<PathBuf> {
    let env = |name| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    env("HF_HUB_CACHE")
        .or_else(|| env("HUGGINGFACE_HUB_CACHE"))
        .or_else(|| {
            env("HF_HOME")
                .or_else(|| {
                    env("XDG_CACHE_HOME")
                        .or_else(|| crate::config::paths::home_dir().map(|p| p.join(".cache")))
                        .map(|p| p.join("huggingface"))
                })
                .map(|p| p.join("hub"))
        })
}

fn mlx_model_ready(model: &str, cache: Option<&Path>) -> bool {
    let complete = |path: &Path| {
        std::fs::read(path.join("config.json"))
            .ok()
            .is_some_and(|bytes| {
                serde_json::from_slice::<serde_json::Value>(&bytes).is_ok_and(|v| {
                    v.get("n_mels")
                        .and_then(|v| v.as_u64())
                        .is_some_and(|n| n > 0)
                })
            })
            && ["model.safetensors", "weights.safetensors", "weights.npz"]
                .iter()
                .any(|file| nonempty(&path.join(file)))
    };
    if Path::new(model).is_dir() {
        return complete(Path::new(model));
    }
    if super::vision_models::validate_repo(model).is_err() {
        return false;
    }
    let Some(cache) = cache else {
        return false;
    };
    let repo = cache.join(format!("models--{}", model.replace('/', "--")));
    let Ok(revision) = std::fs::read_to_string(repo.join("refs/main")) else {
        return false;
    };
    let revision = revision.trim();
    // Do not follow arbitrary paths from cache metadata, or treat another cached revision
    // as the model the backend would actually resolve (the default revision is main).
    !revision.is_empty()
        && revision.len() <= 64
        && revision.bytes().all(|b| b.is_ascii_hexdigit())
        && complete(&repo.join("snapshots").join(revision))
}

/// Use the package's own environment, not an unrelated Python on PATH. Resolving a uv/pip
/// entry-point symlink first also handles tools exposed in ~/.local/bin.
fn adjacent_python(program: &Path) -> Result<PathBuf> {
    let resolved = std::fs::canonicalize(program)
        .with_context(|| format!("resolving {}", program.display()))?;
    let bin = resolved
        .parent()
        .context("voice executable has no parent directory")?;
    ["python3", "python", "python.exe"].into_iter()
        .map(|name| bin.join(name)).find(|p| process::which(p).is_some())
        .context("voice model preparation needs Python from this backend's installation; install its Python package with uv, or configure a complete local model first")
}

#[cfg(test)]
mod tests;
