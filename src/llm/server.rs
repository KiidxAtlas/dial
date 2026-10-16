//! Supervision of a persistent local MLX server: MLX-LM for text, MLX-VLM for vision.
//!
//! The server listens on loopback only, runs in its own process group (or Windows Job Object), and
//! is killed with its descendants on `stop()` or drop. Model and adapter are fixed at
//! start: `mlx_lm.server` can switch them per request (`model` / `adapters`
//! body fields, loaded on demand from any path), but dial never uses that and
//! restarts the server instead, via [`ensure`].
//!
//! Residual limits: any local process, including a tool the agent runs, can
//! reach the loopback port and send requests, which can make the server load
//! files the process can read. Nothing here authenticates the port.

use super::{self as llm, Chat, SERVER_MODEL};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::json;
use std::io::{self, Read};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Stderr kept in memory for diagnostics; older output is discarded.
const STDERR_KEEP_BYTES: usize = 8192;
/// Portion of the stderr tail quoted in start-up errors.
const ERROR_TAIL_BYTES: usize = 2048;
/// Port races are retried; model-load failures are not.
const PORT_ATTEMPTS: usize = 3;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Share of physical memory the server's prompt (KV prefix) cache may hold. mlx_lm keeps up to ten
/// caches with no byte limit by default; a few long-context caches next to the weights push unified
/// memory into swap, which costs far more than the occasional re-prefill an evicted cache causes.
#[cfg(unix)]
const PROMPT_CACHE_MEMORY_DIVISOR: u64 = 4;

fn prompt_cache_bytes() -> Option<u64> {
    #[cfg(unix)]
    {
        // SAFETY: sysconf only reads static system configuration.
        let (pages, page_size) = unsafe {
            (
                libc::sysconf(libc::_SC_PHYS_PAGES),
                libc::sysconf(libc::_SC_PAGESIZE),
            )
        };
        let total = u64::try_from(pages)
            .ok()?
            .checked_mul(u64::try_from(page_size).ok()?)?;
        (total > 0).then_some(total / PROMPT_CACHE_MEMORY_DIVISOR)
    }
    #[cfg(windows)]
    {
        None
    }
}

#[derive(Default)]
struct Tail {
    bytes: Vec<u8>,
    eof: bool,
}

/// Drain the child's stderr into a bounded in-memory tail. A pipe that nobody
/// reads would eventually block the server; a file would grow without bound.
/// At `--log-level WARNING` the server writes request lines and warnings only,
/// never prompt text.
fn collect_stderr(mut stderr: impl Read + Send + 'static) -> Arc<Mutex<Tail>> {
    let tail = Arc::new(Mutex::new(Tail::default()));
    let shared = Arc::clone(&tail);
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            let count = match stderr.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let mut tail = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tail.bytes.extend_from_slice(&buffer[..count]);
            if tail.bytes.len() > STDERR_KEEP_BYTES {
                let excess = tail.bytes.len() - STDERR_KEEP_BYTES;
                tail.bytes.drain(..excess);
            }
        }
        shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .eof = true;
    });
    tail
}

fn tail_text(tail: &Mutex<Tail>, max_bytes: usize) -> String {
    let tail = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let start = tail.bytes.len().saturating_sub(max_bytes);
    String::from_utf8_lossy(&tail.bytes[start..])
        .trim()
        .to_string()
}

/// Wait briefly for the reader thread to see EOF so a dead child's final
/// words are included in the error.
fn settle_stderr(tail: &Mutex<Tail>) {
    for _ in 0..50 {
        if tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .eof
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The child and every process it started share one process group (or Windows Job Object).
struct Supervised {
    child: Child,
    stopped: bool,
}

impl Supervised {
    /// Exit status is observed without reaping, so the group identifier stays reserved until
    /// `stop` signals the group.
    fn exited(&self) -> io::Result<bool> {
        crate::platform::process::exited(&self.child)
    }

    fn stop(&mut self) {
        if !self.stopped {
            let _ = crate::platform::process::kill_group(self.child.id());
            let _ = self.child.wait();
            crate::platform::process::forget_group(self.child.id());
            self.stopped = true;
        }
    }
}

impl Drop for Supervised {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct ModelServer {
    process: Supervised,
    base_url: String,
    model: String,
    vision: bool,
    adapter: Option<PathBuf>,
    steering: Option<crate::steering::Specification>,
    runtime: llm::local_runtime::LocalRuntime,
    tool_grammar: bool,
    /// The context window a GGUF server's `--ctx-size` was sized from; `None` for MLX.
    gguf_context: Option<Option<u64>>,
    tracking: Option<crate::storage::process_record::Record>,
    stderr: Arc<Mutex<Tail>>,
}

enum Failure {
    /// Another process took the port between probing and binding.
    PortTaken(anyhow::Error),
    Fatal(anyhow::Error),
}

impl ModelServer {
    /// Start a server for the resolved local model directory `model`, with an
    /// optional LoRA adapter directory, and wait until it can generate.
    #[cfg(test)]
    pub fn start(
        python: &Path,
        model: &str,
        adapter: Option<&Path>,
        startup_timeout_secs: u64,
    ) -> Result<ModelServer> {
        Self::start_controlled(python, model, adapter, startup_timeout_secs, None)
    }

    #[cfg(test)]
    fn start_controlled(
        python: &Path,
        model: &str,
        adapter: Option<&Path>,
        startup_timeout_secs: u64,
        interrupt: Option<Arc<AtomicBool>>,
    ) -> Result<ModelServer> {
        Self::start_profile(
            python,
            model,
            adapter,
            startup_timeout_secs,
            interrupt,
            None,
            &llm::local_runtime::LocalRuntime::default(),
            None,
        )
    }

    fn start_profile(
        python: &Path,
        model: &str,
        adapter: Option<&Path>,
        startup_timeout_secs: u64,
        interrupt: Option<Arc<AtomicBool>>,
        steering: Option<&crate::steering::Specification>,
        runtime: &llm::local_runtime::LocalRuntime,
        context: Option<&crate::config::types::Config>,
    ) -> Result<ModelServer> {
        ensure!(python.is_absolute(), "python path must be absolute");
        ensure!(!model.is_empty(), "model must not be empty");
        let deadline = (startup_timeout_secs != 0)
            .then(|| Instant::now() + Duration::from_secs(startup_timeout_secs));
        let mut last_race = None;
        for _ in 0..PORT_ATTEMPTS {
            let port = free_port()?;
            match launch(
                ServerProfile {
                    python,
                    model,
                    adapter,
                    steering,
                    context,
                    runtime,
                },
                port,
                deadline,
                interrupt.clone(),
            ) {
                Ok(server) => return Ok(server),
                Err(Failure::PortTaken(error)) => last_race = Some(error),
                Err(Failure::Fatal(error)) => return Err(error),
            }
        }
        Err(last_race.unwrap_or_else(|| anyhow!("no server start attempt was made"))).context(
            format!("model server could not claim a port after {PORT_ATTEMPTS} attempts"),
        )
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn request_model(&self) -> &str {
        if self.vision {
            &self.model
        } else {
            SERVER_MODEL
        }
    }

    /// Whether this server serves exactly `model` with exactly `adapter`.
    pub fn matches(&self, model: &str, adapter: Option<&Path>) -> bool {
        self.model == model && self.adapter.as_deref() == adapter
    }

    /// Whether this server loaded the weights of `model`, with any adapter.
    pub fn serves(&self, model: &str) -> bool {
        self.model == model
    }

    /// False once the server process has exited (crash or external kill).
    pub fn is_running(&self) -> bool {
        !self.process.stopped && matches!(self.process.exited(), Ok(false))
    }

    pub fn ready_for(
        &self,
        model: &str,
        adapter: Option<&Path>,
        steering: Option<&crate::steering::Specification>,
        runtime: &llm::local_runtime::LocalRuntime,
        context: Option<&crate::config::types::Config>,
    ) -> bool {
        self.matches(model, adapter)
            && self.steering.as_ref() == steering
            && self.runtime == *runtime
            && self.tool_grammar == tool_grammar_enabled(context)
            && self
                .gguf_context
                .is_none_or(|launched| launched == requested_context(context))
            && self.is_running()
    }

    /// Kill the whole process group and reap the server. Idempotent.
    pub fn stop(&mut self) {
        self.process.stop();
        self.tracking.take();
    }

    /// Start a new diagnostic window so a failed request cannot quote a previous request's error.
    pub fn begin_request(&mut self) -> Result<()> {
        self.stderr
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .bytes
            .clear();
        self.phase("generating")
    }

    /// A disconnected client is not an engine cancellation acknowledgement. Retire this process
    /// before any retry so abandoned GPU work cannot block the next request.
    pub fn abort_request(&mut self, error: anyhow::Error) -> anyhow::Error {
        self.stop();
        settle_stderr(&self.stderr);
        if error.downcast_ref::<llm::Interrupted>().is_some() {
            return error;
        }
        let text = tail_text(&self.stderr, ERROR_TAIL_BYTES);
        if text.is_empty() {
            error
        } else {
            // Keep the original error in the chain for cancellation/context-overflow classifiers.
            error.context(format!(
                "local model server stderr (last {ERROR_TAIL_BYTES} bytes):\n{text}"
            ))
        }
    }

    pub fn phase(&mut self, phase: &str) -> Result<()> {
        if let Some(record) = &mut self.tracking {
            record.phase(phase)?;
        }
        Ok(())
    }
}

/// Keep `slot` holding a running server for `model`/`adapter`, restarting it
/// when either changed or the process died. The old server is stopped first so
/// two copies of the weights are never resident at once; if the restart fails
/// the slot is left empty.
#[cfg(test)]
pub fn ensure<'a>(
    slot: &'a mut Option<ModelServer>,
    python: &Path,
    model: &str,
    adapter: Option<&Path>,
    startup_timeout_secs: u64,
) -> Result<&'a ModelServer> {
    ensure_controlled(slot, python, model, adapter, startup_timeout_secs, None)
}

#[cfg(test)]
pub fn ensure_controlled<'a>(
    slot: &'a mut Option<ModelServer>,
    python: &Path,
    model: &str,
    adapter: Option<&Path>,
    startup_timeout_secs: u64,
    interrupt: Option<Arc<AtomicBool>>,
) -> Result<&'a ModelServer> {
    ensure_profile_controlled(
        slot,
        python,
        model,
        adapter,
        startup_timeout_secs,
        interrupt,
        None,
    )
}

#[cfg(test)]
pub fn ensure_profile_controlled<'a>(
    slot: &'a mut Option<ModelServer>,
    python: &Path,
    model: &str,
    adapter: Option<&Path>,
    startup_timeout_secs: u64,
    interrupt: Option<Arc<AtomicBool>>,
    steering: Option<&crate::steering::Specification>,
) -> Result<&'a ModelServer> {
    ensure_profile_tracked(
        slot,
        ServerProfile {
            python,
            model,
            adapter,
            steering,
            context: None,
            runtime: &llm::local_runtime::LocalRuntime::default(),
        },
        startup_timeout_secs,
        interrupt,
    )?;
    slot.as_ref().context("model server slot is empty")
}

/// Immutable model identity and optional steering/process-tracking configuration.
#[derive(Clone, Copy)]
pub struct ServerProfile<'a> {
    pub python: &'a Path,
    pub model: &'a str,
    pub adapter: Option<&'a Path>,
    pub steering: Option<&'a crate::steering::Specification>,
    pub context: Option<&'a crate::config::types::Config>,
    pub runtime: &'a llm::local_runtime::LocalRuntime,
}

pub fn ensure_profile_tracked<'a>(
    slot: &'a mut Option<ModelServer>,
    profile: ServerProfile<'_>,
    startup_timeout_secs: u64,
    interrupt: Option<Arc<AtomicBool>>,
) -> Result<&'a mut ModelServer> {
    let ServerProfile {
        python,
        model,
        adapter,
        steering,
        context,
        runtime,
    } = profile;
    let reusable = slot
        .as_ref()
        .is_some_and(|server| server.ready_for(model, adapter, steering, runtime, context));
    if !reusable {
        drop(slot.take());
        *slot = Some(ModelServer::start_profile(
            python,
            model,
            adapter,
            startup_timeout_secs,
            interrupt,
            steering,
            runtime,
            context,
        )?);
    }
    slot.as_mut().context("model server slot is empty")
}

/// Whether the server constrains tool calls with the grammar. Mirrors the
/// `DIAL_TOOL_GRAMMAR` handoff in `launch`; Python defaults to enabled.
pub fn tool_grammar_enabled(context: Option<&crate::config::types::Config>) -> bool {
    context.is_none_or(|config| config.assistance.tool_grammar)
}

/// The context window a GGUF launch sizes `--ctx-size` from. MLX servers size their cache
/// dynamically, so only GGUF servers relaunch when it changes.
fn requested_context(context: Option<&crate::config::types::Config>) -> Option<u64> {
    context
        .and_then(|config| config.requested_context_window())
        .map(u64::from)
}

/// Ask the OS for a free loopback port. The listener is dropped before the
/// server binds, so a race is possible; `launch` reports it as `PortTaken`.
fn free_port() -> Result<u16> {
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("finding a free loopback port")?;
    Ok(listener
        .local_addr()
        .context("reading the chosen port")?
        .port())
}

/// Python sources the MLX server executes; part of the reliability binding.
pub const SERVER_SOURCES: [(&str, &str); 3] = [
    (
        "dial_steering",
        include_str!("../../python/dial_steering.py"),
    ),
    (
        "dial_tool_grammar",
        include_str!("../../python/dial_tool_grammar.py"),
    ),
    ("dial_server", include_str!("../../python/dial_server.py")),
];

/// Register the embedded helper modules, then run `dial_server.py` as `__main__`.
fn launch_script() -> String {
    let (modules, [(_, main)]) = SERVER_SOURCES.split_at(2) else {
        unreachable!("dial_server is the last embedded source")
    };
    let mut script = String::from("import sys, types\n");
    for (name, source) in modules {
        script.push_str(&format!(
            "_m = types.ModuleType({name:?})\nexec({}, _m.__dict__)\nsys.modules[{name:?}] = _m\n",
            serde_json::to_string(source).unwrap()
        ));
    }
    script.push_str(main);
    script
}

/// Restartable spawn spec for a managed process: program, args, cwd, and the variables set
/// explicitly on the command. Inherited variables are not recorded because they may hold secrets;
/// `env_allow` lists the ones a restart re-reads from the environment.
fn spawn_spec(command: &Command) -> anyhow::Result<crate::storage::process_record::SpawnSpec> {
    const ENV_ALLOW: [&str; 6] = ["PATH", "HOME", "USER", "LANG", "LC_ALL", "TMPDIR"];
    let cwd = match command.get_current_dir() {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let env = command
        .get_envs()
        .filter(|(name, _)| name.to_str() != Some("DIAL_PARENT_PID"))
        .filter_map(|(name, value)| Some((name.to_str()?.to_owned(), value?.to_str()?.to_owned())))
        .collect();
    Ok(crate::storage::process_record::SpawnSpec {
        program: std::path::PathBuf::from(command.get_program()),
        args: command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect(),
        cwd,
        env_allow: ENV_ALLOW.iter().map(|name| (*name).to_owned()).collect(),
        env,
    })
}

/// Command line for a Dial-supervised `llama-server` serving one GGUF model.
struct LlamaLaunch<'a> {
    model: &'a Path,
    /// Multimodal projector (`mmproj*.gguf`) found next to the model.
    projector: Option<&'a Path>,
    /// A GGUF LoRA adapter (llama.cpp cannot load MLX adapters).
    adapter: Option<&'a Path>,
    /// Memory-fitted context; `None` leaves llama.cpp's own default.
    context_tokens: Option<u64>,
    port: u16,
}

impl LlamaLaunch<'_> {
    /// Maps `[local_runtime]` onto llama.cpp. Settings without an exact llama.cpp equivalent are
    /// refused rather than approximated, so a launch never silently ignores configuration.
    fn args(
        &self,
        runtime: &llm::local_runtime::LocalRuntime,
    ) -> anyhow::Result<Vec<std::ffi::OsString>> {
        let mut args: Vec<std::ffi::OsString> = vec!["-m".into(), self.model.into()];
        args.extend(
            [
                "--host=127.0.0.1".to_owned(),
                format!("--port={}", self.port),
                "--jinja".to_owned(),
                // One slot keeps the whole context and prompt cache for the single session.
                "--parallel=1".to_owned(),
            ]
            .map(Into::into),
        );
        if let Some(tokens) = self.context_tokens {
            args.push(format!("--ctx-size={tokens}").into());
        }
        if let Some(projector) = self.projector {
            args.extend(["--mmproj".into(), projector.into()]);
        }
        if let Some(adapter) = self.adapter {
            let file = llm::local_format::gguf_file(adapter).ok_or_else(|| {
                anyhow!(
                    "{} is not a GGUF LoRA adapter; llama.cpp loads only GGUF adapters, so train on the model's MLX checkpoint or convert the adapter with llama.cpp's convert_lora_to_gguf.py",
                    adapter.display()
                )
            })?;
            args.extend(["--lora".into(), file.into()]);
        }
        if let Some(draft) = &runtime.draft_model {
            let file = llm::local_format::gguf_file(Path::new(draft)).ok_or_else(|| {
                anyhow!(
                    "draft_model {draft} must be a local GGUF file (or a folder holding one) for a GGUF model; llama.cpp cannot load MLX drafts or download by id here"
                )
            })?;
            args.extend(["--model-draft".into(), file.into()]);
            let count = runtime
                .num_draft_tokens
                .unwrap_or(llm::local_runtime::DEFAULT_NUM_DRAFT_TOKENS);
            args.push(format!("--spec-draft-n-max={count}").into());
        }
        if let Some(bits) = runtime.kv_bits {
            let kind = match bits {
                8 => "q8_0",
                4 => "q4_0",
                other => anyhow::bail!(
                    "kv_bits = {other} has no llama.cpp KV-cache type; GGUF models accept 4 (q4_0) or 8 (q8_0)"
                ),
            };
            anyhow::ensure!(
                runtime.kv_group_size.is_none() && runtime.quantized_kv_start.is_none(),
                "kv_group_size and quantized_kv_start are MLX-only; llama.cpp quantizes the whole KV cache with fixed blocks"
            );
            args.push(format!("--cache-type-k={kind}").into());
            args.push(format!("--cache-type-v={kind}").into());
        }
        Ok(args)
    }
}

fn launch(
    profile: ServerProfile<'_>,
    port: u16,
    deadline: Option<Instant>,
    interrupt: Option<Arc<AtomicBool>>,
) -> std::result::Result<ModelServer, Failure> {
    let ServerProfile {
        python,
        model,
        adapter,
        steering,
        context,
        runtime,
    } = profile;
    let format = llm::local_format::detect(Path::new(model));
    if format == Some(llm::local_format::LocalFormat::Splash) {
        return Err(Failure::Fatal(anyhow!(
            "{model} is a Splash bundle, which only LM Studio can run: load it in LM Studio and select it under the LM Studio provider (Dial lists LM Studio's loaded models automatically)"
        )));
    }
    let gguf = if format == Some(llm::local_format::LocalFormat::Gguf) {
        llm::local_format::gguf_file(Path::new(model))
    } else {
        None
    };
    let vision =
        gguf.is_none() && llm::catalog::local_model_metadata(Path::new(model)).vision == Some(true);
    runtime.validate().map_err(Failure::Fatal)?;
    if gguf.is_none() && vision && !runtime.is_default() {
        return Err(Failure::Fatal(anyhow!(
            "draft-model and KV-cache settings apply to MLX-LM text models only; vision models run through MLX-VLM without them"
        )));
    }
    let mut command = if let Some(file) = &gguf {
        if steering.is_some() {
            return Err(Failure::Fatal(anyhow!(
                "activation steering needs an MLX checkpoint; GGUF models run on llama.cpp without it"
            )));
        }
        let program = llm::local_format::llama_server().ok_or_else(|| {
            Failure::Fatal(anyhow!(
                "GGUF models need llama.cpp's `llama-server`: install llama.cpp (for example `brew install llama.cpp`) or set DIAL_LLAMA_SERVER to its path"
            ))
        })?;
        let requested = requested_context(context);
        let context_tokens = llm::local_fit::gguf_serving_context(file, requested, runtime.kv_bits)
            .map_err(Failure::Fatal)?;
        let projector = llm::local_format::gguf_projector(file);
        let launch = LlamaLaunch {
            model: file,
            projector: projector.as_deref(),
            adapter,
            context_tokens,
            port,
        };
        let mut command = Command::new(program);
        command.args(launch.args(runtime).map_err(Failure::Fatal)?);
        command
    } else {
        let mut command = Command::new(python);
        // `--opt=value` keeps a value that starts with `-` from being read as a flag.
        command
            .arg("-c")
            .arg(launch_script())
            .arg(format!("--model={model}"))
            .arg("--host=127.0.0.1")
            .arg(format!("--port={port}"))
            .arg("--log-level=WARNING");
        if vision {
            command.arg("--vision-cache-size=1").arg("--max-num-seqs=1");
            // Bounded, process-local reuse; Python disables APC if its compatibility check fails.
            command
                .env("APC_ENABLED", "1")
                .env("APC_DISK_ENABLED", "0")
                .env("APC_NUM_BLOCKS", "256")
                .env("APC_MEMORY_MAX_GB", "1")
                .env("APC_MEMORY_RESERVE_GB", "0.25")
                .env("APC_CHECKPOINT_ENTRIES", "1");
        } else if let Some(bytes) = prompt_cache_bytes() {
            command.arg(format!("--prompt-cache-bytes={bytes}"));
        }
        if let Some(adapter) = adapter {
            let mut flag = std::ffi::OsString::from("--adapter-path=");
            flag.push(adapter);
            command.arg(flag);
        }
        for arg in runtime.server_args() {
            command.arg(arg);
        }
        if !tool_grammar_enabled(context) {
            command.env("DIAL_TOOL_GRAMMAR", "0");
        }
        let steering_spec = steering
            .map(serde_json::to_string)
            .transpose()
            .context("encoding steering specification")
            .map_err(Failure::Fatal)?
            .unwrap_or_default();
        command
            .env(
                "DIAL_MODEL_ENGINE",
                if vision { "mlx-vlm" } else { "mlx-lm" },
            )
            .env("DIAL_PARENT_PID", std::process::id().to_string())
            .env("DIAL_STEERING_SPEC", steering_spec)
            .env("HF_HUB_OFFLINE", "1")
            .env("PYTHONUNBUFFERED", "1");
        command
    };
    let mut child = crate::platform::process::spawn_group(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped()),
    )
    .context("starting local model server")
    .map_err(Failure::Fatal)?;
    let stderr = child.stderr.take();
    let mut process = Supervised {
        child,
        stopped: false,
    };
    let mut tracking = context
        .map(|config| {
            crate::storage::process_record::Record::new(
                &config.user_dir,
                process.child.id(),
                "models",
                model,
                &config.project_root,
                &config.state_dir,
                "loading",
            )
        })
        .transpose()
        .map_err(Failure::Fatal)?;
    if let Some(record) = tracking.as_mut() {
        let spec = spawn_spec(&command).map_err(Failure::Fatal)?;
        record.set_spawn_spec(&spec).map_err(Failure::Fatal)?;
    }
    let tail =
        collect_stderr(stderr.ok_or_else(|| Failure::Fatal(anyhow!("missing server stderr")))?);
    let base_url = format!("http://127.0.0.1:{port}");

    let outcome = wait_ready(
        &mut process,
        &base_url,
        &tail,
        deadline,
        interrupt,
        if vision { model } else { SERVER_MODEL },
    );
    match outcome {
        Ok(()) => {
            if let Some(record) = &mut tracking {
                record.phase("loaded").map_err(Failure::Fatal)?;
            }
            Ok(ModelServer {
                process,
                base_url,
                model: model.to_string(),
                vision,
                adapter: adapter.map(Path::to_path_buf),
                steering: steering.cloned(),
                runtime: runtime.clone(),
                tool_grammar: tool_grammar_enabled(context),
                gguf_context: gguf.is_some().then(|| requested_context(context)),
                tracking,
                stderr: tail,
            })
        }
        Err(error) => {
            process.stop();
            settle_stderr(&tail);
            let text = tail_text(&tail, ERROR_TAIL_BYTES);
            let message = if text.is_empty() {
                format!("{error:#} (server wrote nothing to stderr)")
            } else {
                format!("{error:#}; server stderr (last {ERROR_TAIL_BYTES} bytes):\n{text}")
            };
            if text.contains("Address already in use") {
                Err(Failure::PortTaken(anyhow!(message)))
            } else {
                Err(Failure::Fatal(anyhow!(message)))
            }
        }
    }
}

fn wait_ready(
    process: &mut Supervised,
    base_url: &str,
    tail: &Mutex<Tail>,
    deadline: Option<Instant>,
    interrupt: Option<Arc<AtomicBool>>,
    model: &str,
) -> Result<()> {
    let models_url = format!("{base_url}/v1/models");
    let mut last_probe = "no health response yet".to_string();
    loop {
        if interrupt
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
        {
            return Err(llm::Interrupted.into());
        }
        ensure!(
            !process.exited().context("observing server exit")?,
            "model server exited during startup"
        );
        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        ensure!(
            remaining.is_none_or(|remaining| !remaining.is_zero()),
            "model server not ready before the startup timeout; last health probe: {last_probe}"
        );
        match llm::agent(2).get(&models_url).call() {
            Ok(response) if response.status().as_u16() == 200 => break,
            Ok(response) => last_probe = format!("HTTP {}", response.status().as_u16()),
            Err(error) => last_probe = error.to_string(),
        }
        std::thread::sleep(
            remaining.map_or(POLL_INTERVAL, |remaining| POLL_INTERVAL.min(remaining)),
        );
    }
    // The HTTP port opens before the model finishes loading, and a bad
    // model/adapter only fails on first use. One 1-token request blocks until
    // the weights are loaded and proves generation works.
    let remaining = deadline.map_or(0, |deadline| {
        deadline
            .saturating_duration_since(Instant::now())
            .as_secs()
            .max(1)
    });
    let warmup = Chat {
        messages: vec![json!({"role": "user", "content": "ping"})],
        tools: Vec::new(),
        temperature: 0.0,
        max_tokens: 1,
        seed: None,
    };
    if let Err(error) = llm::chat_at_controlled(
        llm::Endpoint {
            headers: &[],
            url: &llm::completions_url(base_url),
            auth: None,
            model,
        },
        &warmup,
        remaining,
        llm::Control {
            interrupt: interrupt.clone(),
            profile: None,
            reasoning_guard: None,
            on_event: &mut |_| true,
        },
    ) {
        if interrupt
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
        {
            return Err(llm::Interrupted.into());
        }
        // A crash during load surfaces as a dropped connection; prefer saying so.
        for _ in 0..20 {
            if process.exited().unwrap_or(false) {
                settle_stderr(tail);
                bail!("model server exited while loading the model");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        return Err(error.context("model server warm-up request failed"));
    }
    ensure!(
        !process.exited().context("observing server exit")?,
        "model server exited right after becoming ready (is the port owned by another process?)"
    );
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    #![cfg(unix)]

    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn strings(args: Vec<std::ffi::OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn gguf_launch_maps_runtime_settings_onto_llama_cpp_flags() {
        let root = tempfile::tempdir().unwrap();
        let model = root.path().join("m.gguf");
        let draft = root.path().join("draft.gguf");
        let lora = root.path().join("lora.gguf");
        for file in [&model, &draft, &lora] {
            fs::write(file, b"GGUF\x03\0\0\0").unwrap();
        }
        let projector = root.path().join("mmproj-f16.gguf");
        let launch = LlamaLaunch {
            model: &model,
            projector: Some(&projector),
            adapter: Some(&lora),
            context_tokens: Some(32768),
            port: 4100,
        };
        let runtime = llm::local_runtime::LocalRuntime {
            draft_model: Some(draft.to_string_lossy().into_owned()),
            num_draft_tokens: Some(5),
            kv_bits: Some(8),
            ..Default::default()
        };
        let args = strings(launch.args(&runtime).unwrap());
        let flag = |name: &str| {
            let at = args.iter().position(|arg| arg == name).unwrap();
            args[at + 1].clone()
        };
        assert_eq!(flag("-m"), model.to_string_lossy());
        assert_eq!(flag("--mmproj"), projector.to_string_lossy());
        assert_eq!(flag("--lora"), lora.to_string_lossy());
        assert_eq!(flag("--model-draft"), draft.to_string_lossy());
        for expected in [
            "--ctx-size=32768",
            "--spec-draft-n-max=5",
            "--cache-type-k=q8_0",
            "--cache-type-v=q8_0",
            "--parallel=1",
            "--jinja",
        ] {
            assert!(
                args.iter().any(|arg| arg == expected),
                "{expected} in {args:?}"
            );
        }

        let plain = strings(
            LlamaLaunch {
                adapter: None,
                projector: None,
                context_tokens: None,
                ..launch
            }
            .args(&Default::default())
            .unwrap(),
        );
        assert!(!plain.iter().any(|arg| arg.starts_with("--ctx-size")
            || arg.starts_with("--cache-type")
            || arg == "--lora"));
    }

    #[test]
    fn gguf_launch_refuses_settings_llama_cpp_cannot_honor() {
        let root = tempfile::tempdir().unwrap();
        let model = root.path().join("m.gguf");
        fs::write(&model, b"GGUF\x03\0\0\0").unwrap();
        let mlx_adapter = root.path().join("adapter");
        fs::create_dir(&mlx_adapter).unwrap();
        fs::write(mlx_adapter.join("adapters.safetensors"), b"x").unwrap();
        let launch = |adapter| LlamaLaunch {
            model: &model,
            projector: None,
            adapter,
            context_tokens: None,
            port: 1,
        };
        let error = launch(Some(&mlx_adapter))
            .args(&Default::default())
            .unwrap_err();
        assert!(
            error.to_string().contains("not a GGUF LoRA adapter"),
            "{error}"
        );
        for (runtime, message) in [
            (
                llm::local_runtime::LocalRuntime {
                    kv_bits: Some(6),
                    ..Default::default()
                },
                "no llama.cpp KV-cache type",
            ),
            (
                llm::local_runtime::LocalRuntime {
                    kv_bits: Some(4),
                    kv_group_size: Some(64),
                    ..Default::default()
                },
                "MLX-only",
            ),
            (
                llm::local_runtime::LocalRuntime {
                    draft_model: Some("mlx-community/draft".into()),
                    ..Default::default()
                },
                "must be a local GGUF file",
            ),
        ] {
            let error = launch(None).args(&runtime).unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
        }
    }

    /// Stands in for `python -m mlx_lm.server`. The `--model` value selects a
    /// behaviour: `exit-early`, `never-ready`, `bad-model`, or a path that
    /// receives `{"pid", "child", "argv"}` once the server is listening.
    const FAKE_SERVER: &str = r#"#!/usr/bin/env python3
import http.server, json, os, socketserver, subprocess, sys
opts = dict(a.split("=", 1) for a in sys.argv[1:] if a.startswith("--") and "=" in a)
mode = opts["--model"]
if mode == "exit-early":
    sys.stderr.write("fatal: cannot load model\n")
    sys.exit(3)
if mode == "never-ready":
    import time
    time.sleep(300)
helper = subprocess.Popen(["sleep", "300"])
marker = os.path.join(mode, "info.json") if os.path.isdir(mode) else mode
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def reply(self, code, payload):
        data = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_GET(self):
        self.reply(200, {"object": "list", "data": []})
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if mode == "bad-model":
            self.reply(500, {"error": "adapter config is broken"})
        else:
            with open(marker) as f: info = json.load(f)
            info.setdefault("requests", []).append(body)
            with open(marker, "w") as f: json.dump(info, f)
            if os.path.basename(mode) == "slow-warmup":
                import time
                time.sleep(300)
                return
            content = body.get("messages", [{}])[-1].get("content")
            if content == "dial-test-fail":
                print("RuntimeError: test inference allocation failed", file=sys.stderr, flush=True)
                self.reply(500, {"error": "inference failed"})
                return
            if content == "dial-test-stall":
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(b'data: {"dial_progress":{"phase":"processing","prompt_tokens":42}}\n\n')
                self.wfile.flush()
                # Deliberately ignore a client disconnect, like an uninterruptible GPU prefill.
                import time
                time.sleep(300)
                return
            self.reply(200, {"choices": [{"finish_reason": "length", "message": {"role": "assistant", "content": "pong"}}],
                             "usage": {"prompt_tokens": 1, "completion_tokens": 1}})
# A fixture has no public hostname. HTTPServer's default server_bind calls
# getfqdn(), which can hang on hosted macOS reverse-DNS resolution before serving.
class FixtureServer(http.server.ThreadingHTTPServer):
    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name = "localhost"
        self.server_port = self.server_address[1]
server = FixtureServer((opts["--host"], int(opts["--port"])), Handler)
if mode != "bad-model":
    open(marker, "w").write(json.dumps({"pid": os.getpid(), "child": helper.pid, "argv": sys.argv[1:], "engine": os.environ.get("DIAL_MODEL_ENGINE"), "apc": os.environ.get("APC_ENABLED"), "apc_config": {key: os.environ.get(key) for key in ("APC_DISK_ENABLED", "APC_NUM_BLOCKS", "APC_MEMORY_MAX_GB", "APC_MEMORY_RESERVE_GB", "APC_CHECKPOINT_ENTRIES")}}))
server.serve_forever()
"#;

    pub(crate) fn fake_python(dir: &Path) -> PathBuf {
        let path = dir.join("fake-python");
        fs::write(&path, FAKE_SERVER).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn tracked_server_exposes_model_phase_and_cleans_up_on_stop() {
        if crate::learn::processes::live_inspection_unavailable() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let python = fake_python(root.path());
        let model_path = root.path().join("model-one");
        let model = model_path.to_str().unwrap();
        let config = crate::config::types::Config {
            user_dir: root.path().join("user"),
            state_dir: root.path().join("state"),
            project_root: root.path().join("project"),
            ..Default::default()
        };
        let mut slot = None;
        let server = ensure_profile_tracked(
            &mut slot,
            ServerProfile {
                python: &python,
                model,
                adapter: None,
                steering: None,
                context: Some(&config),
                runtime: &llm::local_runtime::LocalRuntime::default(),
            },
            30,
            None,
        )
        .unwrap();
        server.phase("generating").unwrap();
        let rows = crate::learn::processes::snapshot(&config.user_dir).unwrap();
        let row = rows.iter().find(|row| row["model"] == model).unwrap();
        assert_eq!(row["phase"], "generating");
        assert_eq!(row["project"], json!(config.project_root));
        let pid = row["pid"].clone();
        server.stop();
        assert!(
            !crate::learn::processes::snapshot(&config.user_dir)
                .unwrap()
                .iter()
                .any(|row| row["pid"] == pid)
        );
    }

    fn alive(pid: i64) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    fn wait_dead(pid: i64) -> bool {
        for _ in 0..200 {
            if !alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn info(path: &Path) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn steering_profile_change_restarts_server_and_clears_cache() {
        let root = tempfile::tempdir().unwrap();
        let python = fake_python(root.path());
        let marker = root.path().join("info.json");
        let model = marker.to_str().unwrap();
        let mut slot = None;
        let mut spec = crate::steering::Specification {
            path: root.path().join("vector.json"),
            id: "a".repeat(64),
            strength: 0.5,
            fingerprint: "first".into(),
        };
        ensure_profile_controlled(&mut slot, &python, model, None, 30, None, Some(&spec)).unwrap();
        let first = info(&marker)["pid"].as_i64().unwrap();
        ensure_profile_controlled(&mut slot, &python, model, None, 30, None, Some(&spec)).unwrap();
        assert_eq!(first, info(&marker)["pid"].as_i64().unwrap());
        spec.strength = 1.;
        ensure_profile_controlled(&mut slot, &python, model, None, 30, None, Some(&spec)).unwrap();
        let second = info(&marker)["pid"].as_i64().unwrap();
        assert_ne!(first, second);
        assert!(wait_dead(first));
        ensure_profile_controlled(&mut slot, &python, model, None, 30, None, None).unwrap();
        assert_ne!(second, info(&marker)["pid"].as_i64().unwrap());
        assert!(wait_dead(second));
    }

    #[test]
    fn ready_then_drop_kills_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let python = fake_python(dir.path());
        let marker = dir.path().join("info.json");
        let model = marker.to_str().unwrap();
        let adapter = dir.path().join("adapter");
        let server = ModelServer::start(&python, model, Some(&adapter), 30).unwrap();
        assert!(server.base_url().starts_with("http://127.0.0.1:"));
        assert!(server.matches(model, Some(&adapter)));
        assert!(!server.matches(model, None));
        assert!(!server.matches("other", Some(&adapter)));
        assert!(server.is_running());
        let started = info(&marker);
        let (pid, helper) = (
            started["pid"].as_i64().unwrap(),
            started["child"].as_i64().unwrap(),
        );
        assert!(alive(pid) && alive(helper));
        let budget = started["argv"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a.as_str()?.strip_prefix("--prompt-cache-bytes="))
            .next()
            .expect("the KV prefix cache must be bounded")
            .parse::<u64>()
            .unwrap();
        assert!(budget > 0, "{budget}");
        drop(server);
        assert!(wait_dead(pid), "server survived drop");
        assert!(wait_dead(helper), "server's child survived drop");
    }

    #[test]
    fn explicit_stop_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let python = fake_python(dir.path());
        let marker = dir.path().join("info.json");
        let mut server = ModelServer::start(&python, marker.to_str().unwrap(), None, 30).unwrap();
        let pid = info(&marker)["pid"].as_i64().unwrap();
        server.stop();
        assert!(!server.is_running());
        assert!(!alive(pid));
        server.stop();
    }

    #[test]
    fn early_exit_reports_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let python = fake_python(dir.path());
        let error = ModelServer::start(&python, "exit-early", None, 30)
            .err()
            .expect("must fail")
            .to_string();
        assert!(error.contains("exited during startup"), "{error}");
        assert!(error.contains("fatal: cannot load model"), "{error}");
    }

    #[test]
    fn cancelled_startup_releases_the_server_before_its_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let python = fake_python(dir.path());
        let cancel = Arc::new(AtomicBool::new(false));
        let mut slot = None;
        let started = Instant::now();
        std::thread::scope(|scope| {
            let flag = cancel.clone();
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                flag.store(true, Ordering::SeqCst);
            });
            let error =
                ensure_controlled(&mut slot, &python, "never-ready", None, 30, Some(cancel))
                    .err()
                    .expect("startup must be cancelled");
            assert!(format!("{error:#}").contains("interrupted"), "{error:#}");
        });
        assert!(slot.is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn unlimited_startup_can_be_cancelled_during_silent_warmup() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("slow-warmup");
        fs::create_dir(&model).unwrap();
        let python = fake_python(dir.path());
        let cancel = Arc::new(AtomicBool::new(false));
        let mut slot = None;
        let started = Instant::now();
        std::thread::scope(|scope| {
            let flag = cancel.clone();
            let marker = model.join("info.json");
            let observer = scope.spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut warmup_started = false;
                while Instant::now() < deadline {
                    if let Some(info) = fs::read(&marker)
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                        && info["requests"]
                            .as_array()
                            .is_some_and(|requests| !requests.is_empty())
                    {
                        warmup_started = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                flag.store(true, Ordering::SeqCst);
                warmup_started
            });
            let error = ensure_controlled(
                &mut slot,
                &python,
                model.to_str().unwrap(),
                None,
                0,
                Some(cancel),
            )
            .err()
            .expect("silent warmup must be cancellable without a startup deadline");
            assert!(
                observer.join().unwrap(),
                "cancellation must occur during warmup"
            );
            assert!(format!("{error:#}").contains("interrupted"), "{error:#}");
        });
        assert!(slot.is_none());
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn startup_timeout_kills_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let python = fake_python(dir.path());
        let started = Instant::now();
        let error = ModelServer::start(&python, "never-ready", None, 1)
            .err()
            .expect("must fail")
            .to_string();
        assert!(error.contains("startup timeout"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn failed_warmup_is_a_start_failure() {
        let dir = tempfile::tempdir().unwrap();
        let python = fake_python(dir.path());
        let error = ModelServer::start(&python, "bad-model", None, 30)
            .err()
            .expect("must fail")
            .to_string();
        assert!(error.contains("warm-up"), "{error}");
        assert!(error.contains("HTTP 500"), "{error}");
    }

    #[test]
    fn ensure_reuses_and_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let python = fake_python(dir.path());
        let first = dir.path().join("a.json");
        let second = dir.path().join("b.json");
        let mut slot = None;
        let url = ensure(&mut slot, &python, first.to_str().unwrap(), None, 30)
            .unwrap()
            .base_url()
            .to_string();
        let pid = info(&first)["pid"].as_i64().unwrap();
        assert_eq!(
            ensure(&mut slot, &python, first.to_str().unwrap(), None, 30)
                .unwrap()
                .base_url(),
            url
        );
        // A different adapter forces a restart and tears the old process down.
        let adapter = dir.path().join("adapter");
        ensure(
            &mut slot,
            &python,
            first.to_str().unwrap(),
            Some(&adapter),
            30,
        )
        .unwrap();
        assert!(wait_dead(pid), "old server not stopped on swap");
        let pid = info(&first)["pid"].as_i64().unwrap();
        // A crashed server is replaced even when the configuration is unchanged.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        for _ in 0..200 {
            if !slot.as_ref().unwrap().is_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        ensure(
            &mut slot,
            &python,
            first.to_str().unwrap(),
            Some(&adapter),
            30,
        )
        .unwrap();
        assert!(slot.as_ref().unwrap().is_running());
        assert_ne!(info(&first)["pid"].as_i64().unwrap(), pid);
        ensure(&mut slot, &python, second.to_str().unwrap(), None, 30).unwrap();
        assert!(
            slot.as_ref()
                .unwrap()
                .matches(second.to_str().unwrap(), None)
        );
    }

    #[test]
    fn free_ports_are_loopback_and_usable() {
        // free_port returns a candidate, not a reservation. Other tests or local
        // processes may claim it before bind; startup already retries that race.
        for _ in 0..PORT_ATTEMPTS {
            let port = free_port().unwrap();
            match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
                Ok(listener) => {
                    assert!(listener.local_addr().unwrap().ip().is_loopback());
                    return;
                }
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => continue,
                Err(error) => panic!("binding the loopback candidate: {error}"),
            }
        }
        panic!("all loopback candidates were claimed before binding");
    }

    #[test]
    fn stderr_tail_is_bounded() {
        let noise = vec![b'x'; STDERR_KEEP_BYTES * 3];
        let tail = collect_stderr(io::Cursor::new(noise));
        settle_stderr(&tail);
        assert_eq!(
            tail.lock().unwrap_or_else(|p| p.into_inner()).bytes.len(),
            STDERR_KEEP_BYTES
        );
        assert_eq!(tail_text(&tail, ERROR_TAIL_BYTES).len(), ERROR_TAIL_BYTES);
    }

    // ---- real mlx_lm.server (run with `cargo test --lib server -- --ignored --nocapture`) ----

    fn real_python() -> PathBuf {
        std::env::var_os("DIAL_TEST_PYTHON")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("python/.venv/bin/python")
            })
    }

    fn snapshot(repo_dir: &str) -> String {
        let home = std::env::var("HOME").unwrap();
        let snapshots = Path::new(&home)
            .join(".cache/huggingface/hub")
            .join(repo_dir)
            .join("snapshots");
        let entry = fs::read_dir(&snapshots)
            .unwrap_or_else(|e| panic!("{}: {e}", snapshots.display()))
            .next()
            .expect("no snapshot")
            .unwrap();
        entry.path().to_str().unwrap().to_string()
    }

    #[test]
    #[ignore = "starts the real mlx_lm.server; needs the SmolLM2 snapshot"]
    fn real_server_chat() {
        let model = snapshot("models--mlx-community--SmolLM2-135M-Instruct-8bit");
        let server = ModelServer::start(&real_python(), &model, None, 180).unwrap();
        let request = Chat {
            messages: vec![json!({"role": "user", "content": "Say hello in five words."})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 24,
            seed: Some(1),
        };
        let reply = llm::chat(server.base_url(), SERVER_MODEL, &request, 120).unwrap();
        println!("REAL REPLY: {reply:?}");
        assert!(!reply.text.is_empty());
        assert!(reply.completion_tokens.is_some());
    }

    #[test]
    #[ignore = "starts the real mlx_lm.server; set DIAL_TOOL_MODEL_DIR to a tool-capable snapshot"]
    fn real_server_tool_calls() {
        let model = std::env::var("DIAL_TOOL_MODEL_DIR").expect("DIAL_TOOL_MODEL_DIR");
        let server = ModelServer::start(&real_python(), &model, None, 180).unwrap();
        let tool = json!({"type": "function", "function": {
            "name": "read_file",
            "description": "Read a text file from the workspace",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}
        }});
        let request = Chat {
            messages: vec![json!({"role": "user", "content": "Please read the file notes.txt"})],
            tools: vec![tool],
            temperature: 0.0,
            max_tokens: 128,
            seed: Some(1),
        };
        let reply = llm::chat(server.base_url(), SERVER_MODEL, &request, 120).unwrap();
        println!("REAL TOOL REPLY: {reply:?}");
        assert!(
            !reply.tool_calls.is_empty(),
            "model produced no parsed tool call"
        );
        assert_eq!(reply.tool_calls[0].name, "read_file");
        assert!(!reply.tool_calls[0].malformed_arguments);
        // Replay the turn with a tool result to prove the history shape is accepted.
        let mut followup = request.clone();
        followup.messages.push(llm::assistant_message(&reply));
        followup.messages.push(llm::tool_message(
            &reply.tool_calls[0].id,
            "hello from notes",
        ));
        let second = llm::chat(server.base_url(), SERVER_MODEL, &followup, 120).unwrap();
        println!("REAL FOLLOWUP: {second:?}");
    }
}
