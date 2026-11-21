//! Dial-launched image servers for downloaded image models.
//!
//! Checkpoints run on stable-diffusion.cpp's `sd-server`: [`components`] maps a checkpoint on disk
//! to `sd-server` loading flags from its files alone (no weights are read, only JSON configs and
//! safetensors headers). MLX-quantized packs (mflux, MLX-Serve) store packed integer weights that
//! stable-diffusion.cpp cannot read; on macOS with Apple Silicon they run on MLX-Serve instead
//! ([`ImageServer::start_mlx_serve`]), and elsewhere they are refused with a reason.
//!
//! [`ImageServer`] runs one server for one generation: it listens on loopback only, runs in its own
//! process group (or Windows Job Object) and is killed with its descendants when dropped, so no
//! server outlives the request that started it. The model loads again for the next generation.
//!
//! `sd-server` runs on every platform stable-diffusion.cpp ships for (macOS Metal, Linux and
//! Windows CPU, with Vulkan/CUDA builds selectable through `DIAL_SD_SERVER`).

use super::{
    ERROR_TAIL_BYTES, POLL_INTERVAL, PORT_ATTEMPTS, Supervised, Tail, collect_stderr, free_port,
    settle_stderr, tail_text,
};
use crate::llm;
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::Value;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The model id `sd-server` reports on `/v1/models`; any value is accepted on requests.
pub const REQUEST_MODEL: &str = "sd-cpp-local";

/// What to tell the user when no `sd-server` is installed.
pub const MISSING_RUNTIME: &str = "local image checkpoints run on stable-diffusion.cpp's `sd-server`: run `/setup sd-cpp` to install it, or set DIAL_SD_SERVER to an existing sd-server";

/// What to tell the user when no `mlx-serve` is installed.
pub const MISSING_MLX_SERVE: &str = "MLX image packs run on MLX-Serve: run `/setup mlx-serve` to install it, or set DIAL_MLX_SERVE to an existing mlx-serve";

/// Whether this machine can run MLX-Serve, which ships for macOS on Apple Silicon only.
#[must_use]
pub const fn mlx_serve_supported() -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
}

/// Whether `model` is a downloaded MLX image pack: a folder whose weights are MLX-quantized, which
/// only MLX-Serve can run (see [`ImageServer::start_mlx_serve`]).
#[must_use]
pub fn is_mlx_pack(model: &Path) -> bool {
    model.is_dir() && mlx_quantized(model).is_some()
}

/// Whether Dial runs the downloaded image model at `model` on MLX-Serve rather than
/// stable-diffusion.cpp: an MLX image pack, or a model in MLX-Serve's own store
/// (`~/.mlx-serve/models`), which it downloaded for itself.
#[must_use]
pub fn runs_on_mlx_serve(model: &Path) -> bool {
    is_mlx_pack(model)
        || (model.is_dir()
            && crate::config::paths::home_dir()
                .is_some_and(|home| model.starts_with(home.join(".mlx-serve").join("models"))))
}

/// Weight file extensions stable-diffusion.cpp loads as a checkpoint.
const WEIGHT_EXTENSIONS: [&str; 4] = ["safetensors", "sft", "ckpt", "gguf"];
/// Largest JSON config or safetensors header read while planning.
const MAX_HEADER_BYTES: u64 = 16 * 1024 * 1024;

/// One `sd-server` loading flag and the file or directory it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Component {
    pub flag: &'static str,
    pub path: PathBuf,
}

impl Component {
    fn new(flag: &'static str, path: PathBuf) -> Self {
        Self { flag, path }
    }
}

/// The `sd-server` loading flags for the checkpoint at `model`:
///
/// - a single weight file, or a directory holding exactly one at its root: `--model`;
/// - a Diffusers UNet layout (SD 1.x/2.x/XL, `unet/diffusion_pytorch_model.safetensors`): the
///   directory as `--model`, which stable-diffusion.cpp loads as Diffusers;
/// - a Diffusers transformer layout (FLUX, SD3, Krea 2, Qwen-Image…): `--diffusion-model` for the
///   transformer (see [`transformer_source`]), `--vae`, and each text encoder by its declared
///   architecture (`--clip_l`, `--clip_g`, `--t5xxl`, or `--llm` for language-model encoders).
///
/// Whether stable-diffusion.cpp supports the architecture is only known once it loads.
///
/// # Errors
/// Missing or ambiguous weights, and MLX-quantized packs, which stable-diffusion.cpp cannot read.
pub fn components(model: &Path) -> Result<Vec<Component>> {
    if model.is_file() {
        ensure!(
            is_weight_file(model),
            "{} is not a .safetensors, .sft, .ckpt or .gguf image checkpoint",
            model.display()
        );
        return Ok(vec![Component::new("--model", model.to_path_buf())]);
    }
    ensure!(
        model.is_dir(),
        "image checkpoint {} does not exist",
        model.display()
    );
    if let Some(reason) = mlx_quantized(model) {
        bail!(
            "{} is an MLX-quantized image pack ({reason}); stable-diffusion.cpp cannot read its packed weights on any OS. Download the original safetensors (Diffusers) or a single-file checkpoint of this model for Dial to run, or serve this pack with MLX-Serve on Apple Silicon",
            model.display()
        );
    }
    if model
        .join("unet")
        .join("diffusion_pytorch_model.safetensors")
        .is_file()
    {
        return Ok(vec![Component::new("--model", model.to_path_buf())]);
    }
    let components_layout = model.join("vae").is_dir() && model.join("text_encoder").is_dir();
    if model.join("transformer").is_dir() || components_layout {
        let source = transformer_source(model);
        let diffusion = weights(&source).with_context(|| {
            if source == model && model.join("transformer").is_dir() {
                format!(
                    "diffusion transformer: stable-diffusion.cpp cannot read the Diffusers weights in {}/transformer, so it loads the original single-file checkpoint the repository keeps at its root (for FLUX.2 klein, flux-2-klein-4b.safetensors); download the model again from /models → Get models → Images to fetch it",
                    model.display()
                )
            } else {
                "diffusion transformer".to_owned()
            }
        })?;
        let mut out = vec![Component::new("--diffusion-model", diffusion)];
        out.push(Component::new(
            "--vae",
            weights(&model.join("vae")).context("VAE (vae/)")?,
        ));
        for name in ["text_encoder", "text_encoder_2", "text_encoder_3"] {
            let dir = model.join(name);
            if !dir.is_dir() {
                continue;
            }
            let flag = encoder_flag(&dir);
            ensure!(
                !out.iter().any(|component| component.flag == flag),
                "{} has two text encoders that both load as {flag}; pass the components to sd-server yourself",
                model.display()
            );
            out.push(Component::new(
                flag,
                weights(&dir).with_context(|| format!("text encoder ({name}/)"))?,
            ));
        }
        return Ok(out);
    }
    let files = weight_files(model)?;
    match files.as_slice() {
        [only] => Ok(vec![Component::new("--model", only.clone())]),
        [] => bail!(
            "{} has no image weights stable-diffusion.cpp can load (.safetensors, .sft, .ckpt or .gguf, or a Diffusers layout)",
            model.display()
        ),
        _ => bail!(
            "{} holds several checkpoint files; select one file as the image model",
            model.display()
        ),
    }
}

/// Whether `model` names an image checkpoint Dial routes to `sd-server`: a Diffusers or
/// component layout directory, or a single non-GGUF weight file (GGUF files are chat models).
pub fn is_checkpoint_path(model: &Path) -> bool {
    if model.is_file() {
        return is_weight_file(model)
            && !model
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("gguf"));
    }
    model.join("vae").is_dir()
        && (model.join("text_encoder").is_dir()
            || model.join("unet").is_dir()
            || model.join("transformer").is_dir())
}

/// Diffusers transformer classes whose tensor names stable-diffusion.cpp does not map: it detects
/// a Diffusers `Flux2Transformer2DModel` as FLUX.1 ("Flux Control") and refuses its shapes, and
/// ignores the Qwen3 text encoder. The original-format checkpoint loads as FLUX.2.
const ORIGINAL_FORMAT_ONLY: [&str; 1] = ["Flux2Transformer2DModel"];

/// Where the diffusion transformer of the transformer layout at `model` is read from: its
/// `transformer/` folder, or the repository root (one original-format checkpoint) when there is no
/// `transformer/` folder (Krea 2), the folder holds no weights (a download that fetched the root
/// checkpoint instead), or the folder's declared class is one stable-diffusion.cpp misreads.
pub fn transformer_source(model: &Path) -> PathBuf {
    let folder = model.join("transformer");
    if !folder.is_dir() {
        return model.to_path_buf();
    }
    let misread = read_json(&folder.join("config.json")).is_some_and(|config| {
        config["_class_name"]
            .as_str()
            .is_some_and(|class| ORIGINAL_FORMAT_ONLY.contains(&class))
    });
    let empty = weight_files(&folder).map_or(true, |files| files.is_empty())
        && !folder
            .join("diffusion_pytorch_model.safetensors.index.json")
            .is_file();
    if misread || empty {
        model.to_path_buf()
    } else {
        folder
    }
}

fn is_weight_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            WEIGHT_EXTENSIONS
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known))
        })
}

/// Weight files at the root of `dir`, sorted.
fn weight_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_weight_file(path))
        .collect();
    files.sort();
    Ok(files)
}

/// The file stable-diffusion.cpp loads for one component directory: its safetensors index for
/// sharded weights, else its only weight file.
fn weights(dir: &Path) -> Result<PathBuf> {
    for index in [
        "diffusion_pytorch_model.safetensors.index.json",
        "model.safetensors.index.json",
    ] {
        if dir.join(index).is_file() {
            return Ok(dir.join(index));
        }
    }
    let files = weight_files(dir)?;
    match files.as_slice() {
        [only] => Ok(only.clone()),
        [] => bail!("no weights in {}", dir.display()),
        _ => {
            // Diffusers repositories can hold precision variants beside the default weights.
            let default = files.iter().find(|path| {
                path.file_name().is_some_and(|name| {
                    name == "diffusion_pytorch_model.safetensors" || name == "model.safetensors"
                })
            });
            default.cloned().with_context(|| {
                format!(
                    "{} holds several weight files and no default; keep one",
                    dir.display()
                )
            })
        }
    }
}

/// The loading flag for a text encoder directory, from its declared `config.json`.
fn encoder_flag(dir: &Path) -> &'static str {
    let config = read_json(&dir.join("config.json")).unwrap_or(Value::Null);
    let architectures: Vec<&str> = config["architectures"]
        .as_array()
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let model_type = config["model_type"].as_str().unwrap_or_default();
    if architectures.iter().any(|name| name.starts_with("T5"))
        || matches!(model_type, "t5" | "umt5")
    {
        "--t5xxl"
    } else if architectures
        .iter()
        .any(|name| name.starts_with("CLIPText"))
        || model_type == "clip_text_model"
    {
        // CLIP-G (SDXL/SD3's second encoder) is 1280 wide; CLIP-L is 768.
        if config["hidden_size"].as_u64() == Some(1280) {
            "--clip_g"
        } else {
            "--clip_l"
        }
    } else {
        "--llm"
    }
}

/// Why `model` is an MLX-quantized pack, if it is: a declared MLX/mflux quantization, or weights
/// stored with MLX's `.scales`/`.biases` companions.
fn mlx_quantized(model: &Path) -> Option<String> {
    if let Some(config) = read_json(&model.join("config.json")) {
        let quantization = &config["quantization"];
        if quantization.is_object() {
            let method = quantization["method"].as_str().unwrap_or("mlx");
            return Some(format!("declares {method} quantization"));
        }
    }
    let transformer = if model.join("transformer").is_dir() {
        model.join("transformer")
    } else {
        model.to_path_buf()
    };
    let names = tensor_names(&transformer)?;
    names
        .iter()
        .any(|name| name.ends_with(".scales"))
        .then(|| "its weights carry MLX .scales/.biases".to_owned())
}

/// Tensor names of one component: the safetensors index's weight map, else the header of its only
/// weight file.
fn tensor_names(dir: &Path) -> Option<Vec<String>> {
    for index in [
        "diffusion_pytorch_model.safetensors.index.json",
        "model.safetensors.index.json",
    ] {
        if let Some(value) = read_json(&dir.join(index)) {
            return Some(value["weight_map"].as_object()?.keys().cloned().collect());
        }
    }
    let files = weight_files(dir).ok()?;
    let file = files.iter().find(|path| {
        path.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("safetensors"))
    })?;
    safetensors_names(file)
}

fn safetensors_names(path: &Path) -> Option<Vec<String>> {
    let mut file = fs::File::open(path).ok()?;
    let mut length = [0u8; 8];
    file.read_exact(&mut length).ok()?;
    let length = u64::from_le_bytes(length);
    if length > MAX_HEADER_BYTES {
        return None;
    }
    let mut header = vec![0u8; usize::try_from(length).ok()?];
    file.read_exact(&mut header).ok()?;
    let value: Value = serde_json::from_slice(&header).ok()?;
    Some(
        value
            .as_object()?
            .keys()
            .filter(|key| *key != "__metadata__")
            .cloned()
            .collect(),
    )
}

/// A JSON config or index read while planning; over `MAX_HEADER_BYTES` it is unreadable.
fn read_json(path: &Path) -> Option<Value> {
    llm::read_json_within(path, MAX_HEADER_BYTES)
}

/// The command-line arguments for serving `components` on `port`. `model_dir` keeps sd-server's
/// model catalog scan inside the checkpoint instead of the working directory's `./models`.
fn server_args(components: &[Component], model_dir: &Path, port: u16) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--listen-ip".into(),
        "127.0.0.1".into(),
        "--listen-port".into(),
        port.to_string().into(),
        "--model-dir".into(),
        model_dir.as_os_str().to_owned(),
    ];
    for component in components {
        args.push(component.flag.into());
        args.push(component.path.as_os_str().to_owned());
    }
    args
}

/// The command-line arguments for MLX-Serve serving the pack at `model` on loopback `port`.
fn mlx_serve_args(model: &Path, port: u16) -> Vec<OsString> {
    vec![
        "--model".into(),
        model.as_os_str().to_owned(),
        "--serve".into(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string().into(),
    ]
}

/// Which server a launch waits for, by how it reports a loaded model.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    SdCpp,
    MlxServe,
}

/// One running image server with a model loaded; stopped when dropped.
pub struct ImageServer {
    process: Supervised,
    base_url: String,
    /// The id the server lists its model under; requests must name it. `None` for `sd-server`,
    /// which accepts any id ([`REQUEST_MODEL`]).
    model: Option<String>,
}

impl ImageServer {
    /// Start `sd-server` for the checkpoint at `model` and wait until its model has loaded.
    /// `startup_timeout_secs` of 0 waits without a deadline; `interrupt` cancels the wait.
    ///
    /// # Errors
    /// No installed runtime, an unloadable checkpoint (see [`components`]), a load failure reported
    /// by the server (with its log tail), a timeout or cancellation.
    pub fn start(
        model: &Path,
        startup_timeout_secs: u64,
        interrupt: Arc<AtomicBool>,
    ) -> Result<Self> {
        let program = llm::local_format::sd_server().ok_or_else(|| anyhow!(MISSING_RUNTIME))?;
        let parts = components(model)?;
        let model_dir = if model.is_dir() {
            model.to_path_buf()
        } else {
            model
                .parent()
                .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
        };
        start_on_free_port(Kind::SdCpp, startup_timeout_secs, &interrupt, |port| {
            let mut command = Command::new(&program);
            command.args(server_args(&parts, &model_dir, port));
            command
        })
    }

    /// Start MLX-Serve for the MLX image pack at `model` and wait until it reports the model
    /// ready. Same timeout, cancellation and cleanup as [`ImageServer::start`].
    ///
    /// # Errors
    /// An unsupported platform, no installed MLX-Serve, a load failure (MLX-Serve exits and logs
    /// why, for example too little free memory), a timeout or cancellation.
    pub fn start_mlx_serve(
        model: &Path,
        startup_timeout_secs: u64,
        interrupt: Arc<AtomicBool>,
    ) -> Result<Self> {
        ensure!(
            mlx_serve_supported(),
            "{} is an MLX image pack, and MLX-Serve runs only on macOS with Apple Silicon",
            model.display()
        );
        ensure!(
            model.is_dir(),
            "MLX image pack {} does not exist",
            model.display()
        );
        let program = llm::local_format::mlx_serve().ok_or_else(|| anyhow!(MISSING_MLX_SERVE))?;
        start_on_free_port(Kind::MlxServe, startup_timeout_secs, &interrupt, |port| {
            let mut command = Command::new(&program);
            command.args(mlx_serve_args(model, port));
            command
        })
    }

    /// The OpenAI-compatible base URL (`http://127.0.0.1:<port>/v1`).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The model id requests must name, when the server requires one (MLX-Serve).
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

/// Launch `command(port)` on a free loopback port, retrying when another process takes the port
/// between choosing and binding it.
fn start_on_free_port(
    kind: Kind,
    startup_timeout_secs: u64,
    interrupt: &Arc<AtomicBool>,
    command: impl Fn(u16) -> Command,
) -> Result<ImageServer> {
    let deadline = (startup_timeout_secs != 0)
        .then(|| Instant::now() + Duration::from_secs(startup_timeout_secs));
    let mut last_race = None;
    for _ in 0..PORT_ATTEMPTS {
        let port = free_port()?;
        match launch(kind, command(port), port, deadline, interrupt) {
            Ok(server) => return Ok(server),
            Err(Launch::PortTaken(error)) => last_race = Some(error),
            Err(Launch::Fatal(error)) => return Err(error),
        }
    }
    Err(last_race.unwrap_or_else(|| anyhow!("no image server start attempt was made"))).context(
        format!("image server could not claim a port after {PORT_ATTEMPTS} attempts"),
    )
}

impl Drop for ImageServer {
    fn drop(&mut self) {
        self.process.stop();
    }
}

enum Launch {
    PortTaken(anyhow::Error),
    Fatal(anyhow::Error),
}

fn launch(
    kind: Kind,
    mut command: Command,
    port: u16,
    deadline: Option<Instant>,
    interrupt: &Arc<AtomicBool>,
) -> std::result::Result<ImageServer, Launch> {
    let mut child = crate::platform::process::spawn_group(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .context("starting the image server")
    .map_err(Launch::Fatal)?;
    // Both servers log loading progress and errors on both streams; keep bounded tails of each.
    let stdout = child.stdout.take().map(collect_stderr);
    let stderr = child.stderr.take().map(collect_stderr);
    let mut process = Supervised::new(child);
    let origin = format!("http://127.0.0.1:{port}");
    let ready = match kind {
        Kind::SdCpp => wait_loaded(&mut process, &origin, deadline, interrupt).map(|()| None),
        Kind::MlxServe => wait_served(&mut process, &origin, deadline, interrupt).map(Some),
    };
    match ready {
        Ok(model) => Ok(ImageServer {
            process,
            base_url: format!("{origin}/v1"),
            model,
        }),
        Err(error) => {
            #[cfg(unix)]
            let exit_status = process.stop();
            #[cfg(not(unix))]
            process.stop();
            let logs = [stdout, stderr]
                .iter()
                .flatten()
                .map(|tail: &Arc<Mutex<Tail>>| {
                    settle_stderr(tail);
                    tail_text(tail, ERROR_TAIL_BYTES)
                })
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            let program = Path::new(command.get_program()).display().to_string();
            #[cfg(unix)]
            let sigkill = {
                use std::os::unix::process::ExitStatusExt as _;
                exit_status.and_then(|s| s.signal()) == Some(9)
            };
            #[cfg(not(unix))]
            let sigkill = false;
            let logs_lower = logs.to_ascii_lowercase();
            let oom_hint = if sigkill
                || logs_lower.contains("out of memory")
                || logs_lower.contains("bad_alloc")
                || logs_lower.contains("insufficient memory")
            {
                " — killed by SIGKILL or out of memory while loading this checkpoint; try a smaller/quantized checkpoint or free RAM before retrying"
            } else {
                ""
            };
            let message = if logs.is_empty() {
                format!("{error:#}{oom_hint}; image server: {program} (it wrote no log)")
            } else {
                format!("{error:#}{oom_hint}; image server: {program}; last log lines:\n{logs}")
            };
            if logs.contains("Address already in use") || logs.contains("bind") {
                Err(Launch::PortTaken(anyhow!(message)))
            } else {
                Err(Launch::Fatal(anyhow!(message)))
            }
        }
    }
}

/// Poll the native model status until the command-line model is loaded.
fn wait_loaded(
    process: &mut Supervised,
    origin: &str,
    deadline: Option<Instant>,
    interrupt: &Arc<AtomicBool>,
) -> Result<()> {
    let status_url = format!("{origin}/sdcpp/v1/models/status");
    let mut last_probe = "no status response yet".to_owned();
    loop {
        if interrupt.load(Ordering::SeqCst) {
            return Err(llm::Interrupted.into());
        }
        ensure!(
            !process.exited().context("observing image server exit")?,
            "image server exited while loading the model"
        );
        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        ensure!(
            remaining.is_none_or(|remaining| !remaining.is_zero()),
            "image model not loaded before the timeout (image.timeout_secs); last status: {last_probe}"
        );
        match llm::agent_with(2, false, Some(Arc::clone(interrupt)), None)
            .get(&status_url)
            .call()
        {
            Ok(mut response) if response.status().as_u16() == 200 => {
                let status: Value = response
                    .body_mut()
                    .with_config()
                    .limit(1 << 20)
                    .read_json()
                    .unwrap_or(Value::Null);
                match loaded(&status) {
                    Ok(true) => return Ok(()),
                    Ok(false) => last_probe = format!("state {}", status["state"]),
                    Err(error) => return Err(error),
                }
            }
            Ok(response) => last_probe = format!("HTTP {}", response.status().as_u16()),
            Err(error) => last_probe = error.to_string(),
        }
        std::thread::sleep(
            remaining.map_or(POLL_INTERVAL, |remaining| POLL_INTERVAL.min(remaining)),
        );
    }
}

/// Whether a `/sdcpp/v1/models/status` body reports a loaded model; an error when loading failed.
fn loaded(status: &Value) -> Result<bool> {
    let error = status["error"].as_str().filter(|text| !text.is_empty());
    match status["state"].as_str() {
        Some("ready") => Ok(true),
        Some("error") => bail!(
            "the image server could not load the model: {}",
            error.unwrap_or("no reason given")
        ),
        Some("empty") if error.is_some() => bail!(
            "the image server could not load the model: {}",
            error.unwrap_or_default()
        ),
        _ => Ok(false),
    }
}

/// Poll MLX-Serve until it lists a ready model, and return that model's id. MLX-Serve loads the
/// command-line model before it listens and exits when the load fails (for example on too little
/// free memory), so an early exit carries the reason in its log.
fn wait_served(
    process: &mut Supervised,
    origin: &str,
    deadline: Option<Instant>,
    interrupt: &Arc<AtomicBool>,
) -> Result<String> {
    let models_url = format!("{origin}/v1/models");
    let mut last_probe = "no response yet".to_owned();
    loop {
        if interrupt.load(Ordering::SeqCst) {
            return Err(llm::Interrupted.into());
        }
        ensure!(
            !process.exited().context("observing MLX-Serve exit")?,
            "MLX-Serve exited while loading the model"
        );
        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        ensure!(
            remaining.is_none_or(|remaining| !remaining.is_zero()),
            "image model not loaded before the timeout (image.timeout_secs); last status: {last_probe}"
        );
        match llm::agent_with(2, false, Some(Arc::clone(interrupt)), None)
            .get(&models_url)
            .call()
        {
            Ok(mut response) if response.status().as_u16() == 200 => {
                let listing: Value = response
                    .body_mut()
                    .with_config()
                    .limit(1 << 20)
                    .read_json()
                    .unwrap_or(Value::Null);
                match served_model(&listing)? {
                    Some(id) => return Ok(id),
                    None => last_probe = "no ready model listed".to_owned(),
                }
            }
            Ok(response) => last_probe = format!("HTTP {}", response.status().as_u16()),
            Err(error) => last_probe = error.to_string(),
        }
        std::thread::sleep(
            remaining.map_or(POLL_INTERVAL, |remaining| POLL_INTERVAL.min(remaining)),
        );
    }
}

/// The id of the ready model in an MLX-Serve `/v1/models` listing, preferring one that declares
/// the `image` capability; `None` while nothing is ready. A model in the `error` state fails.
fn served_model(listing: &Value) -> Result<Option<String>> {
    let rows = listing["data"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if let Some(failed) = rows.iter().find(|row| row["state"] == "error") {
        bail!(
            "MLX-Serve could not load the model: {}",
            failed["error"].as_str().unwrap_or("no reason given")
        );
    }
    let ready = |row: &&Value| row["state"] == "ready" || row["loaded"] == true;
    let image = |row: &&Value| {
        row["capabilities"]
            .as_array()
            .is_some_and(|caps| caps.iter().any(|cap| cap == "image"))
    };
    Ok(rows
        .iter()
        .filter(ready)
        .find(image)
        .or_else(|| rows.iter().find(ready))
        .and_then(|row| row["id"].as_str())
        .map(str::to_owned))
}

#[cfg(test)]
mod tests;
