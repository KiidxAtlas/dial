//! Runs one plan, health-checks the result, and reports the installed location. Nothing here
//! writes config; the caller saves settings only after [`install`] returns `Ok`.

use super::plan::{Plan, Step, plan};
use super::prebuilt;
use super::{
    BUN_MIN, Env, NODE_MIN, Recipe, Status, WHISPER_MODEL_BYTES, WHISPER_MODEL_SHA256,
    WHISPER_MODEL_URL, is_ready, locate, recipe, report, settings_for, whisper_model_path,
};
use crate::voice::{SttBackendChoice, TtsBackendChoice};
use anyhow::ensure;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
        .envs(step.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group, so cancel and timeout also stop what npm, uv and brew spawned.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}; is it installed?", step.program))?;
    let (send, lines) = std::sync::mpsc::channel::<String>();
    if let Some(stdout) = child.stdout.take() {
        spawn_reader(stdout, send.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_reader(stderr, send.clone());
    }
    drop(send);
    let started = Instant::now();
    let mut last = String::new();
    loop {
        let finished = child.try_wait()?;
        while let Ok(line) = lines.try_recv() {
            progress(&line);
            last = line;
        }
        if let Some(status) = finished {
            // Reader threads may still hold output the child wrote before it exited; wait for
            // them so the failure tail and progress are complete.
            while let Ok(line) = lines.recv_timeout(Duration::from_millis(500)) {
                progress(&line);
                last = line;
            }
            if !status.success() {
                let tail = if last.is_empty() {
                    String::new()
                } else {
                    format!(": {last}")
                };
                bail!("{} failed ({status}){tail}", step.display());
            }
            return Ok(());
        }
        if cancel.load(Ordering::Relaxed) {
            kill_step(&mut child);
            bail!("cancelled");
        }
        if started.elapsed() > STEP_TIMEOUT {
            kill_step(&mut child);
            bail!(
                "{} timed out after {}s",
                step.display(),
                STEP_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Kills a step and everything it spawned (its process group), then reaps the direct child.
fn kill_step(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        if let Ok(pid) = libc::pid_t::try_from(child.id()) {
            // SAFETY: the child leads its own process group (`process_group(0)` at spawn), so
            // this signals only that group, and the child is not reaped yet, so the id is valid.
            unsafe { libc::killpg(pid, libc::SIGKILL) };
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn spawn_reader(stream: impl Read + Send + 'static, send: std::sync::mpsc::Sender<String>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            let line = line.trim_end().to_owned();
            if !line.is_empty() && send.send(line).is_err() {
                break;
            }
        }
    });
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
fn probe(program: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group, so the timeout kill also reaches anything the probe spawned.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;
    let mut stdout = child.stdout.take().context("probe stdout unavailable")?;
    let mut stderr = child.stderr.take().context("probe stderr unavailable")?;
    // Readers report over channels so the probe can bound the wait: a grandchild that inherited
    // the pipe keeps it open after the probe exits, and a plain join would block forever.
    let (err_send, err_recv) = std::sync::mpsc::channel::<String>();
    let (out_send, out_recv) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut err = String::new();
        let _ = stderr.read_to_string(&mut err);
        let _ = err_send.send(err);
    });
    std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        let _ = out_send.send(out);
    });
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let out = out_recv.recv_timeout(PROBE_TIMEOUT).with_context(|| {
                format!("{} kept its output open after exiting", program.display())
            })?;
            if !status.success() {
                let err = err_recv.recv_timeout(PROBE_TIMEOUT).unwrap_or_default();
                let tail = err
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .map(|line| format!(": {}", line.trim()))
                    .unwrap_or_default();
                bail!("{} exited with {status}{tail}", program.display());
            }
            return Ok(out.trim().to_owned());
        }
        if started.elapsed() > PROBE_TIMEOUT {
            kill_step(&mut child);
            bail!(
                "{} did not answer within {}s",
                program.display(),
                PROBE_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
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
        locate(target, env)
    } else {
        planned.expected.is_file().then(|| planned.expected.clone())
    }
    .with_context(|| format!("{} installed but no program was found", target.title))?;
    let model = (id == "whisper-cpp")
        .then(|| ensure_whisper_model(&env.user_dir, cancel, progress))
        .transpose()?;
    progress("checking that it works");
    let detail = health(id, &location, model.as_deref())?;
    progress(&format!("ok: {detail}"));
    Ok(location)
}

/// Installs `id` after whatever it requires (Node.js for npm servers, uv for Python tools) is
/// ready. Settings are not saved: callers find the result in Dial's install directory.
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
    file.sync_all()?;
    Ok(())
}

pub(super) fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
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
mod tests {
    use super::*;

    #[test]
    fn parses_bun_versions() {
        assert_eq!(parse_version("1.3.14"), Some((1, 3, 14)));
        assert_eq!(parse_version("bun 1.2.9\n"), Some((1, 2, 9)));
        assert_eq!(parse_version("nothing here"), None);
        assert!(parse_version("1.2.9").unwrap() < BUN_MIN);
    }
}

/// Capabilities grouped by status: what is installed (with its path) and what is missing (with the
/// `/setup` command that installs it). Names are padded so the second column lines up.
pub fn list_text(env: &Env) -> String {
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
        .map(|row| format!("  {}  {}", pad(row.title), row.detail))
        .collect();
    let missing: Vec<String> = rows
        .iter()
        .filter(|row| matches!(row.status, Status::Missing))
        .map(|row| format!("  {}  /setup {}", pad(row.title), row.id))
        .collect();
    let mut lines = Vec::new();
    if !ready.is_empty() {
        lines.push(format!("Installed ({}):", ready.len()));
        lines.extend(ready);
    }
    if !missing.is_empty() {
        lines.push(format!("Not installed ({}):", missing.len()));
        lines.extend(missing);
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
    layout: &crate::storage::paths::Layout,
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
        let mut changes: Vec<crate::storage::settings_store::Change> =
            settings_for(next, &location)
                .into_iter()
                .map(
                    |(path, value)| crate::storage::settings_store::Change::Set {
                        path,
                        value: toml::Value::String(value),
                    },
                )
                .collect();
        if next == "whisper-cpp" {
            changes.push(crate::storage::settings_store::Change::Set {
                path: "voice.stt.whisper_cpp_model".to_owned(),
                value: toml::Value::String(whisper_model_path(&env.user_dir).display().to_string()),
            });
        }
        if !changes.is_empty() {
            crate::storage::settings_store::save_config(
                layout,
                crate::storage::settings_store::Scope::Global,
                &changes,
            )?;
        }
        done.push(format!("{}: {}", target.title, location.display()));
    }
    Ok(done)
}

#[cfg(all(test, unix))]
mod fake_pm_tests {
    use super::*;
    use crate::setup::deps::{install_dir, plan::tests::fake_tools};
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in `npm`: creates `<prefix>/node_modules/.bin/bun`, whose `--version` prints
    /// `version`. It echoes its argv so the test can check the exact command.
    fn fake_npm(root: &Path, version: &str) {
        let bin = root.join("fakebin/npm");
        let script = format!(
            "#!/bin/sh\nset -eu\nprefix=\"\"\nwhile [ $# -gt 0 ]; do case \"$1\" in --prefix) prefix=\"$2\"; shift 2;; install|--no-audit|--no-fund) shift;; *) break;; esac; done\n/bin/mkdir -p \"$prefix/node_modules/.bin\"\nprintf '#!/bin/sh\\necho {version}\\n' > \"$prefix/node_modules/.bin/bun\"\n/bin/chmod +x \"$prefix/node_modules/.bin/bun\"\necho \"added 1 package: $*\"\n"
        );
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn install_runs_fake_npm_into_dial_dir_and_health_checks_bun() {
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_tools(tmp.path(), &["npm"]);
        fake_npm(tmp.path(), "1.3.14");
        let mut lines = Vec::new();
        let location = install("bun", &env, &AtomicBool::new(false), &mut |line| {
            lines.push(line.to_owned())
        })
        .unwrap();
        assert_eq!(
            location,
            install_dir(&env.user_dir, "bun").join("node_modules/.bin/bun")
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("$ ") && line.contains("--prefix"))
        );
        assert!(
            lines.iter().any(|line| line == "ok: Bun 1.3.14"),
            "{lines:?}"
        );
    }

    #[test]
    fn install_rejects_bun_older_than_the_minimum() {
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_tools(tmp.path(), &["npm"]);
        fake_npm(tmp.path(), "1.2.9");
        let error = install("bun", &env, &AtomicBool::new(false), &mut |_| {}).unwrap_err();
        assert!(
            error.to_string().contains("older than the required 1.3.14"),
            "{error}"
        );
    }

    #[test]
    fn cancel_stops_a_running_step() {
        let tmp = tempfile::tempdir().unwrap();
        let slow = tmp.path().join("slow");
        std::fs::write(&slow, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        let step = Step {
            program: slow.display().to_string(),
            args: Vec::new(),
            env: Vec::new(),
        };
        let error = run_step(&step, &AtomicBool::new(true), &mut |_| {}).unwrap_err();
        assert!(error.to_string().contains("cancelled"), "{error}");
    }
}
