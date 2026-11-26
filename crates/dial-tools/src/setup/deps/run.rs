//! Runs one plan, health-checks the result, and reports the installed location. Nothing here
//! writes config; the caller saves settings only after [`install`] returns `Ok`.

use super::plan::{Plan, Step, plan};
use super::prebuilt;
use super::{
    BUN_MIN, Env, NODE_MIN, Recipe, Status, WHISPER_MODEL_BYTES, WHISPER_MODEL_SHA256,
    WHISPER_MODEL_URL, is_ready, locate, recipe, report, settings_for, whisper_model_path,
};
use crate::config::voice::{SttBackendChoice, TtsBackendChoice};
use crate::runtime::fingerprint::sha256_file;
use crate::runtime::process::{self, Stream};
use anyhow::ensure;
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Hard stop for one install step; npm and uv can take minutes on a cold cache.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(900);
/// Hard stop for one health probe.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Runs one step, forwarding each output line to `progress`. Stops when `cancel` is set or
/// the step runs past [`STEP_TIMEOUT`]. A non-zero exit is an error with the last output line.
pub fn run_step(step: &Step, cancel: &AtomicBool, progress: &mut dyn FnMut(&str)) -> Result<()> {
    progress(&format!("$ {}", step.display()));
    let mut command = Command::new(&step.program);
    command
        .args(&step.args)
        .envs(step.env.iter().map(|(k, v)| (k, v)));
    let mut pending = [Vec::new(), Vec::new()];
    let mut last = String::new();
    let mut emit = |bytes: &[u8]| {
        let text = String::from_utf8_lossy(bytes);
        let line = text.trim_end();
        if !line.is_empty() {
            progress(line);
            last.clear();
            last.push_str(line);
        }
    };
    // Keep stdout and stderr line fragments separate; an installer emitting no newlines
    // must not grow an unbounded buffer or delay cancellation.
    let result = process::run(
        command,
        None,
        STEP_TIMEOUT,
        0,
        cancel,
        &mut |stream, bytes| {
            let line = &mut pending[usize::from(matches!(stream, Stream::Stderr))];
            for &byte in bytes {
                if byte == b'\n' {
                    emit(line);
                    line.clear();
                } else {
                    line.push(byte);
                    if line.len() >= 16 * 1024 {
                        emit(line);
                        line.clear();
                    }
                }
            }
        },
    )
    .map_err(anyhow::Error::msg)
    .with_context(|| format!("starting {}; is it installed?", step.program))?;
    for line in pending {
        emit(&line);
    }
    if !result.success() {
        let error = result.failure(&step.display(), STEP_TIMEOUT);
        if !last.is_empty() && result.stderr.is_empty() && !result.cancelled && !result.timed_out {
            bail!("{error}: {last}");
        }
        bail!("{error}");
    }
    Ok(())
}

/// Runs a plan's steps in order and stops at the first failure. Creates Dial's install
/// directory only when the plan targets one.
pub fn run_plan(plan: &Plan, cancel: &AtomicBool, progress: &mut dyn FnMut(&str)) -> Result<()> {
    if plan.destination.is_absolute() {
        std::fs::create_dir_all(&plan.destination)
            .with_context(|| format!("creating {}", plan.destination.display()))?;
    }
    if let Some(fetch) = &plan.fetch {
        prebuilt::fetch(&fetch.asset, &fetch.dest, cancel, progress)?;
    }
    for step in &plan.steps {
        run_step(step, cancel, progress)?;
    }
    Ok(())
}

/// Runs a short probe bounded by [`PROBE_TIMEOUT`] and returns its trimmed stdout.
fn probe(program: &Path, args: &[&str], cancel: &AtomicBool) -> Result<String> {
    let mut command = Command::new(program);
    command.args(args);
    let result = process::run(
        command,
        None,
        PROBE_TIMEOUT,
        1024 * 1024,
        cancel,
        &mut |_, _| {},
    )
    .map_err(anyhow::Error::msg)
    .with_context(|| format!("starting {}", program.display()))?;
    ensure!(
        result.success(),
        "{}",
        result.failure(&program.display().to_string(), PROBE_TIMEOUT)
    );
    ensure!(
        result.stdout.len() == result.stdout_total && !result.output_incomplete,
        "incomplete probe output from {}",
        program.display()
    );
    Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

/// Parses `1.3.14` or `bun 1.3.14` into a comparable triple.
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let token = text
        .split_whitespace()
        .find(|part| part.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut parts = token.split('.').map(|part| {
        part.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u64>()
            .ok()
    });
    Some((
        parts.next()??,
        parts.next()??,
        parts.next().flatten().unwrap_or(0),
    ))
}

/// Confirms a dependency works, not just that a file exists. Probes are short and bounded so a
/// stuck tool cannot hang the UI. Returns a one-line description of what answered.
pub fn health(id: &str, location: &Path, model: Option<&Path>) -> Result<String> {
    health_with_cancel(id, location, model, &AtomicBool::new(false))
}

pub(in crate::setup) fn health_with_cancel(
    id: &str,
    location: &Path,
    model: Option<&Path>,
    cancel: &AtomicBool,
) -> Result<String> {
    let probe = |program: &Path, args: &[&str]| probe(program, args, cancel);
    match id {
        "bun" => {
            let version = parse_version(&probe(location, &["--version"])?)
                .context("could not read the Bun version")?;
            if version < BUN_MIN {
                bail!(
                    "Bun {}.{}.{} is older than the required {}.{}.{}",
                    version.0,
                    version.1,
                    version.2,
                    BUN_MIN.0,
                    BUN_MIN.1,
                    BUN_MIN.2
                );
            }
            Ok(format!("Bun {}.{}.{}", version.0, version.1, version.2))
        }
        "node" => {
            let out = probe(location, &["--version"])?;
            let version = parse_version(out.trim_start_matches('v'))
                .context("could not read the Node.js version")?;
            if version < NODE_MIN {
                bail!(
                    "Node.js {}.{}.{} is older than the required {}.{}.{}",
                    version.0,
                    version.1,
                    version.2,
                    NODE_MIN.0,
                    NODE_MIN.1,
                    NODE_MIN.2
                );
            }
            Ok(format!("Node.js {}.{}.{}", version.0, version.1, version.2))
        }
        // These Python CLIs expose --help, not --version. A version probe rejects a healthy
        // installation before the settings picker can prepare its voice/model files.
        "piper" | "mlx-whisper" => {
            probe(location, &["--help"]).map(|_| format!("{id} CLI available"))
        }
        crate::llm::download::HUB_HELPER => probe(
            location,
            &[
                "-I",
                "-c",
                "from huggingface_hub import list_repo_files, hf_hub_download, snapshot_download",
            ],
        )
        .map(|_| "Hugging Face client imports".into()),
        "mlx-runtime" => probe(location, &["-c", "import mlx"]).map(|_| "MLX imports".into()),
        "whisper-cpp" => {
            let model = model.context("whisper.cpp needs its model; run setup again")?;
            let clip =
                std::env::temp_dir().join(format!("dial-whisper-check-{}.wav", std::process::id()));
            fs::write(&clip, silence_wav(1)).context("writing the whisper check clip")?;
            let out = probe(
                location,
                &[
                    "-m",
                    &model.to_string_lossy()[..],
                    "-f",
                    &clip.to_string_lossy()[..],
                    "-nt",
                ],
            );
            let _ = fs::remove_file(&clip);
            let text = out?;
            ensure!(
                text.lines().any(|line| !line.trim().is_empty()),
                "whisper.cpp printed no transcript for the check clip"
            );
            Ok("transcribed the check clip".into())
        }
        _ => {
            let out = probe(location, &["--version"])?;
            Ok(out.lines().next().unwrap_or("ok").to_owned())
        }
    }
}

/// Resolve standard package-manager bins absent from PATH. Executes package managers, so callers
/// must obtain installation/start authorization first.
pub(crate) fn standard_location(target: &Recipe, env: &Env) -> Result<Option<PathBuf>> {
    standard_location_with_cancel(target, env, &AtomicBool::new(false))
}

fn standard_location_with_cancel(
    target: &Recipe,
    env: &Env,
    cancel: &AtomicBool,
) -> Result<Option<PathBuf>> {
    let probe = |program: &Path, args: &[&str]| probe(program, args, cancel);
    for method in target.methods {
        let bin = match method {
            super::Method::NpmGlobal(_) => {
                let Some(npm) = env.find("npm") else { continue };
                let prefix = PathBuf::from(probe(&npm, &["prefix", "--global"])?);
                if cfg!(windows) {
                    prefix
                } else {
                    prefix.join("bin")
                }
            }
            super::Method::UvTool(_) => {
                let Some(uv) = env.find("uv") else { continue };
                PathBuf::from(probe(&uv, &["tool", "dir", "--bin"])?)
            }
            super::Method::Go(_) => {
                let Some(go) = env.find("go") else { continue };
                let gobin = probe(&go, &["env", "GOBIN"])?;
                if gobin.is_empty() {
                    let gopath = probe(&go, &["env", "GOPATH"])?;
                    std::env::split_paths(&gopath)
                        .next()
                        .context("go returned no GOPATH")?
                        .join("bin")
                } else {
                    PathBuf::from(gobin)
                }
            }
            super::Method::Rustup(_) => {
                let Some(rustup) = env.find("rustup") else {
                    continue;
                };
                // Rustup installs components into the active toolchain. Homebrew's rustup puts no
                // proxy for them on PATH, so ask rustup where the program is; `rustup which`
                // fails while the component is not installed, which means "not found" here.
                let Ok(program) = probe(&rustup, &["which", target.program]) else {
                    ensure!(!cancel.load(Ordering::Relaxed), "cancelled");
                    continue;
                };
                match Path::new(&program).parent() {
                    Some(bin) => bin.to_path_buf(),
                    None => continue,
                }
            }
            _ => continue,
        };
        anyhow::ensure!(
            bin.is_absolute(),
            "package manager returned a non-absolute bin directory: {}",
            bin.display()
        );
        let program = if cfg!(windows) {
            format!(
                "{}{}",
                target.program,
                if matches!(method, super::Method::NpmGlobal(_)) {
                    ".cmd"
                } else {
                    ".exe"
                }
            )
        } else {
            target.program.to_owned()
        };
        let path = bin.join(program);
        if crate::platform::fs::is_executable(&path) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Plans and runs one install, then health-checks it. Returns the healthy location.
///
/// `progress` receives every output line so a background job can show it. The caller saves
/// settings from the returned location only after this returns `Ok`.
pub fn install(
    id: &str,
    env: &Env,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<PathBuf> {
    let target: &Recipe = recipe(id).with_context(|| format!("unknown dependency {id}"))?;
    let planned = plan(target, env).map_err(|blocked| anyhow::anyhow!("{blocked}"))?;
    run_plan(&planned, cancel, progress)?;
    let location = if planned.expected.as_os_str().is_empty() {
        standard_location_with_cancel(target, env, cancel)?.or_else(|| locate(target, env))
    } else {
        planned.expected.is_file().then(|| planned.expected.clone())
    }
    .with_context(|| format!("{} installed but no program was found", target.title))?;
    let model = (id == "whisper-cpp")
        .then(|| ensure_whisper_model(&env.user_dir, cancel, progress))
        .transpose()?;
    progress("checking that it works");
    let detail = health_with_cancel(id, &location, model.as_deref(), cancel)?;
    progress(&format!("ok: {detail}"));
    Ok(location)
}

/// Installs `id` after its declared prerequisites. Language servers require an existing package
/// manager and use its normal installation directories; the returned path is authoritative.
pub fn install_with_requirements(
    id: &str,
    env: &Env,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<PathBuf> {
    let target: &Recipe = recipe(id).with_context(|| format!("unknown dependency {id}"))?;
    for required in target.requires {
        let needed = recipe(required).with_context(|| format!("unknown dependency {required}"))?;
        if locate(needed, env).is_none() {
            progress(&format!("== {}", needed.title));
            install(required, env, cancel, progress)?;
        }
    }
    progress(&format!("== {}", target.title));
    install(id, env, cancel, progress)
}

/// Downloads the whisper.cpp model if it is missing, then checks its SHA-256. Returns its path.
/// A file that fails the checksum is deleted, never installed.
pub fn ensure_whisper_model(
    user_dir: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<PathBuf> {
    let path = whisper_model_path(user_dir);
    if path.is_file() && sha256_file(&path)? == WHISPER_MODEL_SHA256 {
        progress("whisper model already present (sha256 ok)");
        return Ok(path);
    }
    let dir = path.parent().context("model path has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let partial = path.with_extension("bin.part");
    progress(&format!(
        "downloading ggml-base.en.bin (about {} MB) from {WHISPER_MODEL_URL}",
        WHISPER_MODEL_BYTES / 1_000_000
    ));
    download_to(
        WHISPER_MODEL_URL,
        &partial,
        WHISPER_MODEL_BYTES,
        cancel,
        progress,
    )?;
    let got = sha256_file(&partial)?;
    if got != WHISPER_MODEL_SHA256 {
        let _ = fs::remove_file(&partial);
        bail!("whisper model checksum mismatch: expected {WHISPER_MODEL_SHA256}, got {got}");
    }
    fs::rename(&partial, &path).with_context(|| format!("saving {}", path.display()))?;
    progress("model verified (sha256 ok)");
    Ok(path)
}

pub(super) fn download_to(
    url: &str,
    dest: &Path,
    expected: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<()> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(STEP_TIMEOUT))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut reader = response.body_mut().as_reader();
    let mut file =
        fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    let step = if expected > 0 {
        expected / 10
    } else {
        10_000_000
    };
    let mut next_report = step;
    loop {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled while downloading {url}");
        }
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("reading {url}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        total += n as u64;
        if total >= next_report {
            progress(&if expected > 0 {
                format!(
                    "downloaded {} of {} MB",
                    total / 1_000_000,
                    expected / 1_000_000
                )
            } else {
                format!("downloaded {} MB", total / 1_000_000)
            });
            next_report += step;
        }
    }
    crate::platform::fs::sync_file(&file)?;
    Ok(())
}

/// A mono 16 kHz 16-bit PCM WAV of silence, used as the whisper.cpp health-check clip.
fn silence_wav(seconds: u32) -> Vec<u8> {
    let data_len = 16_000 * 2 * seconds;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&16_000u32.to_le_bytes());
    out.extend_from_slice(&32_000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.resize(44 + data_len as usize, 0);
    out
}

#[cfg(test)]
mod tests;

/// Capabilities grouped by status: what is installed (with its path) and what is missing (with the
/// command that installs it on this surface: `/setup` in the TUI, `dial doctor --fix` on the
/// command line). Names are padded so the second column lines up.
pub fn list_text(env: &Env, install_command: &str) -> String {
    capability_text(env, install_command, None)
}

/// Like [`list_text`], but missing capabilities are one summary line pointing at `list_all`
/// (the full list is long and mostly irrelevant to any one project).
pub fn summary_text(env: &Env, list_all: &str) -> String {
    capability_text(env, "", Some(list_all))
}

fn capability_text(env: &Env, install_command: &str, summarize: Option<&str>) -> String {
    let rows = report(env);
    let width = rows
        .iter()
        .map(|row| row.title.chars().count())
        .max()
        .unwrap_or(0);
    let pad = |title: &str| format!("{title}{}", " ".repeat(width - title.chars().count()));
    let ready: Vec<String> = rows
        .iter()
        .filter(|row| matches!(row.status, Status::Ready))
        .map(|row| format!("  ✓ {}  {}", pad(row.title), row.detail))
        .collect();
    let missing: Vec<String> = rows
        .iter()
        .filter(|row| matches!(row.status, Status::Missing))
        .map(|row| format!("  · {}  {install_command} {}", pad(row.title), row.id))
        .collect();
    let mut lines = Vec::new();
    if !ready.is_empty() {
        lines.push(format!("Installed ({}):", ready.len()));
        lines.extend(ready);
    }
    if !missing.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        if let Some(list_all) = summarize {
            lines.push(format!(
                "Not installed: {} optional capabilit{}; `{list_all}` lists them with their setup commands.",
                missing.len(),
                if missing.len() == 1 { "y" } else { "ies" }
            ));
        } else {
            lines.push(format!(
                "Not installed ({}) — each line ends with the command that previews its setup:",
                missing.len()
            ));
            lines.extend(missing);
        }
    }
    if lines.is_empty() {
        lines.push("No optional capabilities are known.".to_owned());
    }
    lines.join("\n")
}

/// A settings path whose program is not installed, with the setup that fixes it.
pub struct SettingOffer {
    pub path: String,
    pub id: &'static str,
    pub title: &'static str,
    pub preview: String,
}

/// Every settings path backed by a dependency that is missing right now.
pub fn setting_offers(
    env: &Env,
    tts: &TtsBackendChoice,
    stt: &SttBackendChoice,
) -> Vec<SettingOffer> {
    // The voice rows offer the program their backend would use first. On macOS `auto` speaks
    // with the built-in `say`, so nothing is offered for it.
    let tts_ids: &[&str] = match tts {
        TtsBackendChoice::Auto if cfg!(target_os = "macos") => &[],
        TtsBackendChoice::Auto => &["piper", "espeak-ng"],
        TtsBackendChoice::Piper => &["piper"],
        TtsBackendChoice::Espeak => &["espeak-ng"],
        TtsBackendChoice::Say | TtsBackendChoice::Openai => &[],
    };
    let stt_ids: &[&str] = match stt {
        SttBackendChoice::Auto => &["whisper-cpp", "mlx-whisper"],
        SttBackendChoice::WhisperCpp => &["whisper-cpp"],
        SttBackendChoice::MlxWhisper => &["mlx-whisper"],
        SttBackendChoice::Openai => &[],
    };
    let mut offers = Vec::new();
    for (row, ids) in [
        ("voice.tts.backend", tts_ids),
        ("voice.stt.backend", stt_ids),
    ] {
        let candidates: Vec<&Recipe> = ids.iter().filter_map(|id| recipe(id)).collect();
        // Auto and a named backend both work once any candidate is installed, so offer only when
        // none is.
        if !candidates.iter().any(|recipe| is_ready(recipe, env))
            && let Some(first) = candidates.first()
        {
            offers.push(offer(row, first, env));
        }
    }
    // Every other row names exactly one program (each LSP server).
    for recipe in super::RECIPES {
        if VOICE_RECIPES.contains(&recipe.id) || is_ready(recipe, env) {
            continue;
        }
        for (path, _) in settings_for(recipe.id, Path::new("")) {
            offers.push(offer(&path, recipe, env));
        }
    }
    offers
}

/// Recipes that voice rows reach through their backend, not through a row of their own.
const VOICE_RECIPES: &[&str] = &["piper", "espeak-ng", "whisper-cpp", "mlx-whisper"];

fn offer(path: &str, recipe: &Recipe, env: &Env) -> SettingOffer {
    SettingOffer {
        path: path.to_owned(),
        id: recipe.id,
        title: recipe.title,
        preview: preview_text(recipe.id, env),
    }
}

/// Exactly what `/setup <id> yes` (or `dial doctor --fix <id> --yes`) will do.
pub fn preview_text(id: &str, env: &Env) -> String {
    let Some(recipe) = recipe(id) else {
        return format!("Unknown capability `{id}`. Run /setup to list them.");
    };
    if is_ready(recipe, env) {
        return format!("{} is already set up.", recipe.title);
    }
    let mut lines = vec![format!("{}:", recipe.title)];
    let missing = super::plan::unmet_requirements(recipe, env);
    if !missing.is_empty() {
        lines.push(format!("Also sets up first: {}", missing.join(", ")));
    }
    match plan(recipe, env) {
        Ok(planned) => {
            lines.push(format!("Method: {}", planned.method));
            lines.push(format!("Installs into: {}", planned.destination.display()));
            if id == "whisper-cpp" {
                lines.push(format!(
                    "Model: ggml-base.en.bin (about {} MB) from {WHISPER_MODEL_URL} into {}, checked with sha256",
                    WHISPER_MODEL_BYTES / 1_000_000,
                    whisper_model_path(&env.user_dir).display()
                ));
            }
            if let Some(fetch) = &planned.fetch {
                lines.push(format!(
                    "Downloads: {} (sha256 {})",
                    fetch.asset.url, fetch.asset.sha256
                ));
            }
            for step in &planned.steps {
                lines.push(format!("Runs: {}", step.display()));
            }
            lines.push(format!("Confirm with: /setup {id} yes"));
        }
        Err(blocked) => lines.push(blocked.to_string()),
    }
    lines.join("\n")
}

/// Installs `id` and whatever it requires, in order. Each dependency is health-checked before
/// its settings are written, so a failed install leaves the config untouched. Returns one
/// line per dependency that was set up.
pub fn install_and_save(
    layout: &crate::config::paths::Layout,
    id: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<Vec<String>> {
    let env = Env::current(&layout.user_dir);
    let mut done = Vec::new();
    let mut stack = vec![id];
    let mut order = Vec::new();
    while let Some(next) = stack.pop() {
        let target = recipe(next).with_context(|| format!("unknown dependency {next}"))?;
        if order.contains(&next) || (locate(target, &env).is_some() && next != id) {
            continue;
        }
        order.push(next);
        for required in target.requires {
            stack.push(required);
        }
    }
    for next in order.into_iter().rev() {
        let target = recipe(next).expect("validated above");
        if locate(target, &env).is_some() && next != id {
            continue;
        }
        progress(&format!("== {}", target.title));
        let location = install(next, &env, cancel, progress)?;
        let mut changes: Vec<crate::config::settings_store::Change> = settings_for(next, &location)
            .into_iter()
            .map(|(path, value)| crate::config::settings_store::Change::Set {
                path,
                value: toml::Value::String(value),
            })
            .collect();
        if next == "whisper-cpp" {
            changes.push(crate::config::settings_store::Change::Set {
                path: "voice.stt.whisper_cpp_model".to_owned(),
                value: toml::Value::String(whisper_model_path(&env.user_dir).display().to_string()),
            });
        }
        if !changes.is_empty() {
            crate::config::settings_store::save_config(
                layout,
                crate::config::settings_store::Scope::Global,
                &changes,
            )?;
        }
        done.push(format!("{}: {}", target.title, location.display()));
    }
    Ok(done)
}

#[cfg(all(test, unix))]
mod fake_pm_tests;
