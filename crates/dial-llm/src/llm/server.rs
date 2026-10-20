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
pub(super) struct Supervised {
    pub(super) child: Child,
    stopped: bool,
}

impl Supervised {
    /// Takes ownership of a child spawned with `platform::process::spawn_group`.
    pub(super) fn new(child: Child) -> Self {
        Self {
            child,
            stopped: false,
        }
    }

    /// Exit status is observed without reaping, so the group identifier stays reserved until
    /// `stop` signals the group.
    fn exited(&self) -> io::Result<bool> {
        crate::platform::process::exited(&self.child)
    }

    pub(super) fn stop(&mut self) {
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
    steering: Option<crate::llm::steering::Specification>,
    runtime: llm::local_runtime::LocalRuntime,
    tool_grammar: bool,
    /// The context window a GGUF server's `--ctx-size` was sized from; `None` for MLX.
    gguf_context: Option<Option<u64>>,
    tracking: Option<crate::platform::process_record::Record>,
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
    #[cfg(any(test, feature = "test-support"))]
    pub fn start(
        python: &Path,
        model: &str,
        adapter: Option<&Path>,
        startup_timeout_secs: u64,
    ) -> Result<ModelServer> {
        Self::start_controlled(python, model, adapter, startup_timeout_secs, None)
    }

    #[cfg(any(test, feature = "test-support"))]
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
        steering: Option<&crate::llm::steering::Specification>,
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
        steering: Option<&crate::llm::steering::Specification>,
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
#[cfg(any(test, feature = "test-support"))]
pub fn ensure<'a>(
    slot: &'a mut Option<ModelServer>,
    python: &Path,
    model: &str,
    adapter: Option<&Path>,
    startup_timeout_secs: u64,
) -> Result<&'a ModelServer> {
    ensure_controlled(slot, python, model, adapter, startup_timeout_secs, None)
}

#[cfg(any(test, feature = "test-support"))]
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

#[cfg(any(test, feature = "test-support"))]
pub fn ensure_profile_controlled<'a>(
    slot: &'a mut Option<ModelServer>,
    python: &Path,
    model: &str,
    adapter: Option<&Path>,
    startup_timeout_secs: u64,
    interrupt: Option<Arc<AtomicBool>>,
    steering: Option<&crate::llm::steering::Specification>,
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
    pub steering: Option<&'a crate::llm::steering::Specification>,
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
        include_str!("../../../../python/dial_steering.py"),
    ),
    (
        "dial_tool_grammar",
        include_str!("../../../../python/dial_tool_grammar.py"),
    ),
    (
        "dial_server",
        include_str!("../../../../python/dial_server.py"),
    ),
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
fn spawn_spec(command: &Command) -> anyhow::Result<crate::platform::process_record::SpawnSpec> {
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
    Ok(crate::platform::process_record::SpawnSpec {
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

/// Slots of a Dial-launched `llama-server` that lists the shared-pool flags: slot 0 keeps the main
/// session's prompt, the other takes side requests (summaries, ranking, repair) so they no longer
/// evict it.
pub const LLAMA_SHARED_SLOTS: u32 = 2;
/// Flags a `llama-server --help` must list before Dial launches the two-slot layout: one unified KV
/// pool, a per-slot context cap so the main slot keeps the whole window, and idle-slot clearing off
/// (with it on, unified-KV builds clear the main slot at every new task and a pinned `id_slot`
/// request never reloads it). Builds without all three keep the single-slot launch.
const SHARED_SLOT_FLAGS: [&str; 3] = [
    "--kv-unified",
    "--kv-unified-per-slot",
    "--no-cache-idle-slots",
];
/// Bound on the offline `llama-server --help` probe; a slower or hung binary counts as old.
const HELP_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const HELP_PROBE_MAX_BYTES: u64 = 1 << 20;

/// Whether `help` lists every shared-slot flag as a whole option name (`--kv-unified` is not
/// matched by `--kv-unified-per-slot`).
fn help_lists_shared_slots(help: &str) -> bool {
    let options: std::collections::HashSet<&str> = help
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|word| word.starts_with("--"))
        .collect();
    SHARED_SLOT_FLAGS.iter().all(|flag| options.contains(flag))
}

/// Runs `program --help` once with a deadline and returns its standard output (capped), or `None`
/// when it cannot start or does not finish in time. Nothing is loaded and no network is used.
fn probe_help(program: &Path, timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.take(HELP_PROBE_MAX_BYTES).read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    reader.join().ok()
}

/// Whether the `llama-server` at `program` supports the shared two-slot layout, probed once per
/// path and process so routing and launches agree for the whole session.
fn shared_slots_supported(program: &Path) -> bool {
    static PROBED: std::sync::LazyLock<Mutex<std::collections::HashMap<PathBuf, bool>>> =
        std::sync::LazyLock::new(Default::default);
    let mut probed = PROBED.lock().unwrap_or_else(|p| p.into_inner());
    *probed.entry(program.to_path_buf()).or_insert_with(|| {
        probe_help(program, HELP_PROBE_TIMEOUT).is_some_and(|help| help_lists_shared_slots(&help))
    })
}

/// Slots a Dial-launched `llama-server` is asked for: [`LLAMA_SHARED_SLOTS`] when the installed
/// binary supports the shared layout, else `None` (one slot, the server chooses). A launch whose
/// reserve does not fit in memory still starts one slot; llama.cpp wraps an out-of-range `id_slot`
/// onto it, which is the single-slot behavior.
pub fn dial_llama_slots() -> Option<u32> {
    llm::local_format::llama_server()
        .is_some_and(|program| shared_slots_supported(&program))
        .then_some(LLAMA_SHARED_SLOTS)
}

/// KV tokens reserved beyond the main window for the side slot: a quarter of the window within
/// 4096..=16384, never more than the window, in 256-token steps.
fn side_reserve(tokens: u64) -> u64 {
    (tokens / 4).clamp(4096, 16384).min(tokens) / 256 * 256
}

/// The shared KV pool for a two-slot launch: the main window plus the side reserve, when the binary
/// supports the layout, the window is known and the larger pool fits in memory. `None` keeps the
/// single-slot launch, so the main window is never reduced to make room.
fn shared_pool(
    context_tokens: Option<u64>,
    supported: bool,
    fits: impl FnOnce(u64) -> bool,
) -> Option<u64> {
    let tokens = context_tokens.filter(|_| supported)?;
    let pool = tokens + side_reserve(tokens);
    fits(pool).then_some(pool)
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
    /// Unified KV pool for the two-slot layout (see [`shared_pool`]); `None` launches one slot.
    shared_pool: Option<u64>,
    port: u16,
}

impl LlamaLaunch<'_> {
    /// Maps `[local_runtime]` onto llama.cpp. Settings without an exact llama.cpp equivalent are
    /// refused rather than approximated, so a launch never silently ignores configuration.
    /// llama.cpp matches option names exactly, so every value is a separate argument.
    fn args(
        &self,
        runtime: &llm::local_runtime::LocalRuntime,
    ) -> anyhow::Result<Vec<std::ffi::OsString>> {
        let mut args: Vec<std::ffi::OsString> = vec!["-m".into(), self.model.into()];
        let mut push = |pairs: &[&str]| args.extend(pairs.iter().map(Into::into));
        push(&[
            "--host",
            "127.0.0.1",
            "--port",
            &self.port.to_string(),
            "--jinja",
        ]);
        match (self.shared_pool, self.context_tokens) {
            (Some(pool), Some(tokens)) => {
                // Two slots over one KV pool; each slot may use the whole window, so the main
                // session keeps it, and the reserve lets a side request run without evicting it.
                push(&[
                    "--parallel",
                    &LLAMA_SHARED_SLOTS.to_string(),
                    "--kv-unified",
                    "--kv-unified-per-slot",
                    &tokens.to_string(),
                    "--no-cache-idle-slots",
                    "--ctx-size",
                    &pool.to_string(),
                ]);
            }
            (_, tokens) => {
                // One slot keeps the whole context and prompt cache for the single session.
                push(&["--parallel", "1"]);
                if let Some(tokens) = tokens {
                    push(&["--ctx-size", &tokens.to_string()]);
                }
            }
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
            args.extend(["--spec-draft-n-max".into(), count.to_string().into()]);
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
            args.extend(["--cache-type-k", kind, "--cache-type-v", kind].map(Into::into));
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
        let shared_pool = shared_pool(context_tokens, shared_slots_supported(&program), |pool| {
            llm::local_fit::gguf_pool_fits(file, pool, runtime.kv_bits)
        });
        let launch = LlamaLaunch {
            model: file,
            projector: projector.as_deref(),
            adapter,
            context_tokens,
            shared_pool,
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
            crate::platform::process_record::Record::new(
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

#[cfg(all(any(test, feature = "test-support"), unix))]
pub mod fixtures;

#[cfg(all(test, unix))]
pub(crate) mod tests;
