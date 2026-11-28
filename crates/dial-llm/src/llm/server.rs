//! Supervision of persistent local MLX-LM, MLX-VLM and llama.cpp servers.
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

/// Exact prefix snapshots kept by the vision runner. Models with sliding-window layers (Gemma 3/4)
/// can only reuse an exact snapshot that is a strict token prefix of the next prompt. The snapshot
/// at a prompt's end includes the template's generation prompt, which the next turn renders
/// differently, so reuse comes from the interval checkpoint taken during prefill; one entry would
/// keep only the unusable final snapshot.
const VISION_PREFIX_CHECKPOINTS: &str = "4";

/// Vision runner prefix-cache budget in GiB: the same share of physical memory as the text
/// runner's prompt cache. A single long-context snapshot can exceed 1 GiB, which would evict
/// the reusable checkpoint before the next turn.
pub(crate) fn vision_prefix_cache_gib() -> String {
    prompt_cache_bytes().map_or_else(
        || "1".to_owned(),
        |bytes| format!("{:.2}", bytes as f64 / f64::from(1u32 << 30)),
    )
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

/// Preserve the provider diagnostic and identify the executable the operator must repair.
fn startup_diagnostic(error: &anyhow::Error, stderr: &str, program: &std::ffi::OsStr) -> String {
    let program = Path::new(program).display();
    let mut message = if stderr.is_empty() {
        format!("{error:#}; server executable: {program} (server wrote nothing to stderr)")
    } else {
        format!(
            "{error:#}; server executable: {program}; server stderr (last {ERROR_TAIL_BYTES} bytes):\n{stderr}"
        )
    };
    if stderr.contains("unknown model architecture") {
        message.push_str("\nThe selected llama.cpp runtime does not support this GGUF architecture. Upgrade to a llama-server build with support for this architecture (the latest package-manager release may still be too old). Set DIAL_LLAMA_SERVER to that executable, or load the model in a compatible LM Studio runtime and select the LM Studio provider. Changing sampling or context settings cannot add architecture support.");
    }
    message
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

    pub(super) fn stop(&mut self) -> Option<std::process::ExitStatus> {
        if !self.stopped {
            let prior = self.child.try_wait().ok().flatten();
            let _ = crate::platform::process::kill_group(self.child.id());
            let waited = self.child.wait().ok();
            crate::platform::process::forget_group(self.child.id());
            self.stopped = true;
            return prior.or(waited);
        }
        None
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
    /// Model id to send in `/v1/chat/completions` when the runtime advertises its own id
    /// (e.g. `mlx-serve`, `vllm-mlx`, `mistralrs-server`, `vllm`, `sglang`, `exllamav3-server`).
    served_model: Option<String>,
    vlm: bool,
    /// Serves only `POST /v1/embeddings` (MLX-VLM embedding checkpoint), never chat.
    embedding: bool,
    adapter: Option<PathBuf>,
    steering: Option<crate::llm::steering::Specification>,
    runtime: llm::local_runtime::LocalRuntime,
    tool_grammar: bool,
    /// The context window a GGUF server's `--ctx-size` was sized from; `None` for MLX.
    gguf_context: Option<Option<u64>>,
    tracking: Option<crate::platform::process_record::Record>,
    stderr: Arc<Mutex<Tail>>,
    /// Monotonic timestamp of the most recent request or warmup, used for idle auto-unload.
    last_used: Instant,
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
            ServerProfile {
                python,
                model,
                adapter,
                steering: None,
                context: None,
                runtime: &llm::local_runtime::LocalRuntime::default(),
            },
            startup_timeout_secs,
            interrupt,
        )
    }

    fn start_profile(
        profile: ServerProfile<'_>,
        startup_timeout_secs: u64,
        interrupt: Option<Arc<AtomicBool>>,
    ) -> Result<ModelServer> {
        ensure!(profile.python.is_absolute(), "python path must be absolute");
        ensure!(!profile.model.is_empty(), "model must not be empty");
        let deadline = (startup_timeout_secs != 0)
            .then(|| Instant::now() + Duration::from_secs(startup_timeout_secs));
        let mut last_race = None;
        for _ in 0..PORT_ATTEMPTS {
            let port = free_port()?;
            match launch(profile, port, deadline, interrupt.clone(), false) {
                Ok(server) => return Ok(server),
                Err(Failure::PortTaken(error)) => last_race = Some(error),
                Err(Failure::Fatal(error)) => return Err(error),
            }
        }
        Err(last_race.unwrap_or_else(|| anyhow!("no server start attempt was made"))).context(
            format!("model server could not claim a port after {PORT_ATTEMPTS} attempts"),
        )
    }

    /// Start an MLX-VLM server that serves `POST /v1/embeddings` for the multimodal embedding
    /// checkpoint `model` (a resolved local directory), and wait until one embedding succeeds.
    pub fn start_embedding(
        python: &Path,
        model: &str,
        startup_timeout_secs: u64,
        interrupt: Option<Arc<AtomicBool>>,
        context: Option<&crate::config::types::Config>,
    ) -> Result<ModelServer> {
        ensure!(python.is_absolute(), "python path must be absolute");
        ensure!(!model.is_empty(), "model must not be empty");
        let deadline = (startup_timeout_secs != 0)
            .then(|| Instant::now() + Duration::from_secs(startup_timeout_secs));
        let runtime = llm::local_runtime::LocalRuntime::default();
        let profile = ServerProfile {
            python,
            model,
            adapter: None,
            steering: None,
            context,
            runtime: &runtime,
        };
        let mut last_race = None;
        for _ in 0..PORT_ATTEMPTS {
            let port = free_port()?;
            match launch(profile, port, deadline, interrupt.clone(), true) {
                Ok(server) => return Ok(server),
                Err(Failure::PortTaken(error)) => last_race = Some(error),
                Err(Failure::Fatal(error)) => return Err(error),
            }
        }
        Err(last_race.unwrap_or_else(|| anyhow!("no server start attempt was made"))).context(
            format!("embedding server could not claim a port after {PORT_ATTEMPTS} attempts"),
        )
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn request_model(&self) -> &str {
        if let Some(served) = &self.served_model {
            served
        } else if self.vlm || self.embedding {
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
    /// Record activity so the idle reaper keeps this server resident.
    pub fn touch(&mut self) {
        self.last_used = Instant::now();
    }

    /// How long this managed server has been idle since its last request or warmup.
    pub fn idle_for(&self) -> Duration {
        self.last_used.elapsed()
    }

    /// Start a new diagnostic window so a failed request cannot quote a previous request's error.
    pub fn begin_request(&mut self) -> Result<()> {
        self.touch();
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
    let reusable = slot.as_ref().is_some_and(|server| {
        server.ready_for(
            profile.model,
            profile.adapter,
            profile.steering,
            profile.runtime,
            profile.context,
        )
    });
    if !reusable {
        drop(slot.take());
        *slot = Some(ModelServer::start_profile(
            profile,
            startup_timeout_secs,
            interrupt,
        )?);
    } else if let Some(server) = slot.as_mut() {
        server.touch();
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
        env_allow: crate::platform::env::cleared_child_allow_list(&ENV_ALLOW, cfg!(windows)),
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
/// Default soft-close reasoning token budget for Dial-managed `llama-server` binaries that support
/// `--reasoning-budget`. Matches the 3/4 * 4096-token soft-close target used on Dial's MLX runtime,
/// letting the server close the thought block cleanly and emit a tool call or answer in-place
/// instead of aborting the stream.
const LLAMA_DEFAULT_REASONING_BUDGET: u32 = 3072;
const LLAMA_REASONING_BUDGET_MESSAGE: &str =
    "I have reasoned enough; I will now execute the next step or answer directly.";
/// Default KV-shift chunk threshold for Dial-managed `llama-server` binaries that support
/// `--cache-reuse`, letting the server salvage matching prompt suffixes when earlier history shifts.
const LLAMA_DEFAULT_CACHE_REUSE: u32 = 256;
/// Bound on the offline `llama-server --help` probe; a slower or hung binary counts as old.
const HELP_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const HELP_PROBE_MAX_BYTES: u64 = 1 << 20;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LlamaHelpCaps {
    shared_slots: bool,
    reasoning_budget: bool,
    reasoning_budget_message: bool,
    cache_reuse: bool,
}

fn help_options(help: &str) -> std::collections::HashSet<&str> {
    help.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|word| word.starts_with("--"))
        .collect()
}

/// Whether `help` lists every shared-slot flag as a whole option name (`--kv-unified` is not
/// matched by `--kv-unified-per-slot`).
#[cfg(all(test, unix))]
fn help_lists_shared_slots(help: &str) -> bool {
    let options = help_options(help);
    SHARED_SLOT_FLAGS.iter().all(|flag| options.contains(flag))
}

fn help_caps_from(help: &str) -> LlamaHelpCaps {
    let options = help_options(help);
    LlamaHelpCaps {
        shared_slots: SHARED_SLOT_FLAGS.iter().all(|flag| options.contains(flag)),
        reasoning_budget: options.contains("--reasoning-budget"),
        reasoning_budget_message: options.contains("--reasoning-budget-message"),
        cache_reuse: options.contains("--cache-reuse"),
    }
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

/// Probes `program --help` once per path and process so routing and launches agree for the whole
/// session without re-spawning the binary.
fn llama_help_caps(program: &Path) -> LlamaHelpCaps {
    static PROBED: std::sync::LazyLock<Mutex<std::collections::HashMap<PathBuf, LlamaHelpCaps>>> =
        std::sync::LazyLock::new(Default::default);
    let mut probed = PROBED.lock().unwrap_or_else(|p| p.into_inner());
    *probed.entry(program.to_path_buf()).or_insert_with(|| {
        probe_help(program, HELP_PROBE_TIMEOUT)
            .map(|help| help_caps_from(&help))
            .unwrap_or_default()
    })
}

/// Whether the `llama-server` at `program` supports the shared two-slot layout.
fn shared_slots_supported(program: &Path) -> bool {
    llama_help_caps(program).shared_slots
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
    /// Soft-close thinking token budget when the `llama-server` binary supports `--reasoning-budget`.
    reasoning_budget: Option<u32>,
    /// Whether the binary also supports `--reasoning-budget-message`.
    reasoning_budget_message: bool,
    /// Minimum KV-shift chunk size when the binary supports `--cache-reuse`.
    cache_reuse: Option<u32>,
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
            args.extend(
                [
                    "--flash-attn",
                    "on",
                    "--cache-type-k",
                    kind,
                    "--cache-type-v",
                    kind,
                ]
                .map(Into::into),
            );
        }
        if let Some(budget) = self.reasoning_budget.filter(|tokens| *tokens > 0) {
            args.extend(["--reasoning-budget".into(), budget.to_string().into()]);
            if self.reasoning_budget_message {
                args.extend([
                    "--reasoning-budget-message".into(),
                    LLAMA_REASONING_BUDGET_MESSAGE.into(),
                ]);
            }
        }
        if let Some(chunk) = self.cache_reuse.filter(|tokens| *tokens > 0) {
            args.extend(["--cache-reuse".into(), chunk.to_string().into()]);
        }
        Ok(args)
    }
}

/// Check that `adapter` is a PEFT directory (`adapter_config.json` + `adapter_model.safetensors`).
fn require_peft_adapter(adapter: &Path, runtime_name: &str) -> anyhow::Result<()> {
    ensure!(
        adapter.join("adapter_config.json").is_file()
            && adapter.join("adapter_model.safetensors").is_file(),
        "{} is not a PEFT LoRA adapter (expected adapter_config.json + adapter_model.safetensors); export it with `dial improve export-adapter --format peft` for {runtime_name}",
        adapter.display()
    );
    Ok(())
}

/// Arguments for `mlx-serve` serving a local MLX chat checkpoint on loopback `port`.
fn mlx_serve_chat_args(model: &str, port: u16) -> Vec<std::ffi::OsString> {
    vec![
        "--model".into(),
        model.into(),
        "--serve".into(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string().into(),
    ]
}

/// Arguments for `vllm-mlx serve` serving a local MLX checkpoint on loopback `port`.
fn vllm_mlx_args(model: &str, port: u16) -> Vec<std::ffi::OsString> {
    vec![
        "serve".into(),
        model.into(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string().into(),
    ]
}

/// Arguments for `mistralrs-server` serving either a GGUF file or a local Safetensors/MLX folder.
fn mistralrs_args(
    model: &str,
    gguf: Option<&Path>,
    port: u16,
) -> anyhow::Result<Vec<std::ffi::OsString>> {
    let mut args: Vec<std::ffi::OsString> = vec!["--port".into(), port.to_string().into()];
    if let Some(file) = gguf {
        let dir = file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let name = file
            .file_name()
            .ok_or_else(|| anyhow!("GGUF path {} has no filename", file.display()))?;
        args.extend([
            "gguf".into(),
            "-m".into(),
            dir.as_os_str().to_owned(),
            "-f".into(),
            name.to_owned(),
        ]);
    } else {
        args.extend(["plain".into(), "-m".into(), model.into()]);
    }
    Ok(args)
}

/// Arguments for `vllm serve` serving a HuggingFace Safetensors checkpoint (+ optional PEFT LoRA).
fn vllm_args(
    model: &str,
    adapter: Option<&Path>,
    port: u16,
) -> anyhow::Result<Vec<std::ffi::OsString>> {
    let mut args: Vec<std::ffi::OsString> = vec![
        "serve".into(),
        model.into(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string().into(),
    ];
    if let Some(adapter) = adapter {
        require_peft_adapter(adapter, "vLLM")?;
        let mut spec = std::ffi::OsString::from("dial=");
        spec.push(adapter);
        args.extend(["--enable-lora".into(), "--lora-modules".into(), spec]);
    }
    Ok(args)
}

/// Arguments for `sglang serve` serving a HuggingFace Safetensors checkpoint (+ optional PEFT LoRA).
fn sglang_args(
    model: &str,
    adapter: Option<&Path>,
    port: u16,
) -> anyhow::Result<Vec<std::ffi::OsString>> {
    let mut args: Vec<std::ffi::OsString> = vec![
        "serve".into(),
        "--model-path".into(),
        model.into(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string().into(),
    ];
    if let Some(adapter) = adapter {
        require_peft_adapter(adapter, "SGLang")?;
        let mut spec = std::ffi::OsString::from("dial=");
        spec.push(adapter);
        args.extend(["--lora-paths".into(), spec]);
    }
    Ok(args)
}

/// Extract the first usable model id from an OpenAI-compatible `GET /v1/models` body.
fn first_served_model_id(body: &serde_json::Value) -> Option<String> {
    body.get("data")?
        .as_array()?
        .iter()
        .filter_map(|row| row.get("id").and_then(serde_json::Value::as_str))
        .find(|id| !id.is_empty())
        .map(str::to_owned)
}

fn launch(
    profile: ServerProfile<'_>,
    port: u16,
    deadline: Option<Instant>,
    interrupt: Option<Arc<AtomicBool>>,
    embedding: bool,
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
    let exl3 = format == Some(llm::local_format::LocalFormat::Exl3);
    if exl3 && cfg!(target_os = "macos") {
        return Err(Failure::Fatal(anyhow!(
            "{model} is an EXL3 (ExLlamaV3) checkpoint, which requires an NVIDIA CUDA GPU on Linux or Windows. Connect to a remote ExLlamaV3/TabbyAPI server via /providers, or use an MLX/GGUF checkpoint on macOS."
        )));
    }
    let gguf = if format == Some(llm::local_format::LocalFormat::Gguf) {
        llm::local_format::gguf_file(Path::new(model))
    } else {
        None
    };
    use llm::local_runtime::{GgufEngines, GgufLoader, LocalEngine};
    if gguf.is_none()
        && runtime.engine != LocalEngine::Auto
        && !format.is_some_and(|f| {
            LocalEngine::choices(f, GgufEngines::Unknown).contains(&runtime.engine)
        })
    {
        return Err(Failure::Fatal(anyhow!(
            "The selected {} engine is not a known loader for this model on this platform. Review the engine choice before loading it.",
            runtime.engine.label()
        )));
    }
    if embedding && gguf.is_some() {
        return Err(Failure::Fatal(anyhow!(
            "{model} is a GGUF file; local image embeddings need an MLX embedding checkpoint, or set an OpenAI-compatible embedding endpoint"
        )));
    }
    let resolved_non_gguf = if !exl3 && gguf.is_none() && !embedding {
        match runtime.engine {
            LocalEngine::Auto => format
                .and_then(|f| LocalEngine::default_for(f, GgufEngines::Unknown))
                .unwrap_or(LocalEngine::Mlx),
            other => other,
        }
    } else {
        LocalEngine::Mlx
    };
    let vlm = !embedding
        && gguf.is_none()
        && resolved_non_gguf == LocalEngine::Mlx
        && llm::catalog::uses_mlx_vlm(Path::new(model));
    runtime.validate().map_err(Failure::Fatal)?;
    if vlm && !runtime.is_default() {
        return Err(Failure::Fatal(anyhow!(
            "draft-model and KV-cache settings apply to MLX-LM only; this model runs through MLX-VLM without them"
        )));
    }
    // External OpenAI-compatible runtimes advertise their own model id at `/v1/models` rather than
    // accepting Dial's `default_model` alias.
    let mut discover_model_id = false;
    let mut explicit_request_model: Option<String> = None;
    let mut command = if exl3 {
        if steering.is_some() {
            return Err(Failure::Fatal(anyhow!(
                "activation steering needs an MLX checkpoint; EXL3 models run without it"
            )));
        }
        let program = llm::local_format::exl3_server().ok_or_else(|| {
            Failure::Fatal(anyhow!(
                "EXL3 model {model} requires an ExLlamaV3 / TabbyAPI server executable (run `/setup exl3`, select ExLlamaV3 in Dial Desktop, or set DIAL_EXL3_SERVER), or configure a TabbyAPI endpoint in /providers"
            ))
        })?;
        let mut command = Command::new(program);
        command
            .arg("--model-dir")
            .arg(model)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string());
        if let Some(adapter) = adapter {
            require_peft_adapter(adapter, "ExLlamaV3").map_err(Failure::Fatal)?;
            command.arg("--lora-dir").arg(adapter);
        }
        discover_model_id = true;
        command
    } else if let Some(file) = &gguf {
        if steering.is_some() {
            return Err(Failure::Fatal(anyhow!(
                "activation steering needs an MLX checkpoint; GGUF models run without it"
            )));
        }
        let loader = GgufLoader::resolve(file, runtime.engine).map_err(Failure::Fatal)?;
        let program = loader.program().map_err(Failure::Fatal)?;
        if loader == GgufLoader::MistralRs {
            if adapter.is_some() {
                return Err(Failure::Fatal(anyhow!(
                    "mistral.rs GGUF launch does not accept a separate LoRA adapter; switch the engine to llama.cpp or export a fused GGUF"
                )));
            }
            if !runtime.is_default() {
                return Err(Failure::Fatal(anyhow!(
                    "draft-model and KV-cache settings are not forwarded to mistral.rs; clear them or switch the engine to llama.cpp"
                )));
            }
            let mut command = Command::new(program);
            command.args(mistralrs_args(model, Some(file), port).map_err(Failure::Fatal)?);
            discover_model_id = true;
            command
        } else {
            let atomic = loader == GgufLoader::Atomic;
            let requested = requested_context(context);
            let context_tokens =
                llm::local_fit::gguf_serving_context(file, requested, runtime.kv_bits)
                    .map_err(Failure::Fatal)?;
            let dspark = (atomic && runtime.is_default() && adapter.is_none())
                .then(|| llm::local_format::inkling_dspark(file, context_tokens))
                .flatten();
            let projector = llm::local_format::gguf_projector(file);
            let caps = llama_help_caps(&program);
            let shared_pool = shared_pool(context_tokens, !atomic && caps.shared_slots, |pool| {
                llm::local_fit::gguf_pool_fits(file, pool, runtime.kv_bits)
            });
            let launch = LlamaLaunch {
                model: file,
                projector: projector.as_deref(),
                adapter,
                context_tokens,
                shared_pool,
                reasoning_budget: caps
                    .reasoning_budget
                    .then_some(LLAMA_DEFAULT_REASONING_BUDGET),
                reasoning_budget_message: caps.reasoning_budget_message,
                cache_reuse: caps.cache_reuse.then_some(LLAMA_DEFAULT_CACHE_REUSE),
                port,
            };
            let mut command = Command::new(program);
            command.args(launch.args(runtime).map_err(Failure::Fatal)?);
            if atomic {
                command.args(["--alias", SERVER_MODEL]);
                if let Some(draft) = dspark {
                    let depth = if file
                        .file_name()
                        .is_some_and(|n| n == "Inkling-2-Mini-AD-IQ2_M.gguf")
                    {
                        "7"
                    } else {
                        "3"
                    };
                    let budget = if llm::local_fit::physical_memory_bytes()
                        .is_some_and(|bytes| bytes >= 24 << 30)
                    {
                        "14436"
                    } else {
                        "8292"
                    };
                    command.arg("-md").arg(draft).args([
                        "--spec-type",
                        "draft-dspark",
                        "--spec-draft-n-max",
                        depth,
                        "--spec-draft-n-ubatch",
                        "32",
                        "--engine-budget-mib",
                        budget,
                        "-fit",
                        "off",
                        "-b",
                        "1024",
                        "-ub",
                        "1024",
                        "-fa",
                        "on",
                        "-ctk",
                        "q4_0",
                        "-ctv",
                        "q4_0",
                        "-ctkd",
                        "q4_0",
                        "-ctvd",
                        "q4_0",
                        "-ngl",
                        "all",
                        "-ngld",
                        "99",
                        "-t",
                        "8",
                        "-tb",
                        "8",
                        "--cache-ram",
                        "0",
                        "--ctx-checkpoints",
                        "0",
                        "--no-warmup",
                    ]);
                }
            }
            command
        }
    } else if resolved_non_gguf != LocalEngine::Mlx {
        if steering.is_some() {
            return Err(Failure::Fatal(anyhow!(
                "activation steering requires Dial's Python MLX runtime; {} runs without it",
                resolved_non_gguf.label()
            )));
        }
        if !runtime.is_default() {
            return Err(Failure::Fatal(anyhow!(
                "draft-model and KV-cache settings apply to Dial's Python MLX-LM runtime only; clear them or switch the engine to MLX"
            )));
        }
        discover_model_id = true;
        match resolved_non_gguf {
            LocalEngine::MlxServe => {
                if adapter.is_some() {
                    return Err(Failure::Fatal(anyhow!(
                        "MLX-Serve does not load Dial LoRA adapters; switch the engine to MLX or fuse the adapter first"
                    )));
                }
                let program = llm::local_format::mlx_serve().ok_or_else(|| {
                    Failure::Fatal(anyhow!(
                        "MLX-Serve is not installed: run `/setup mlx-serve` or select MLX-Serve in Dial Desktop to download it, or set DIAL_MLX_SERVE"
                    ))
                })?;
                let mut command = Command::new(program);
                command.args(mlx_serve_chat_args(model, port));
                command
            }
            LocalEngine::VllmMlx => {
                if adapter.is_some() {
                    return Err(Failure::Fatal(anyhow!(
                        "vLLM-MLX launch does not accept a Dial LoRA adapter; switch the engine to MLX or fuse the adapter first"
                    )));
                }
                let program = llm::local_format::vllm_mlx_server().ok_or_else(|| {
                    Failure::Fatal(anyhow!(
                        "vLLM-MLX is not installed: run `/setup vllm-mlx` or select vLLM-MLX in Dial Desktop to install it, or set DIAL_VLLM_MLX_SERVER"
                    ))
                })?;
                let mut command = Command::new(program);
                command.args(vllm_mlx_args(model, port));
                command
            }
            LocalEngine::MistralRs => {
                if adapter.is_some() {
                    return Err(Failure::Fatal(anyhow!(
                        "mistral.rs plain launch does not accept a separate LoRA adapter; fuse the adapter or switch engines"
                    )));
                }
                let program = llm::local_format::mistralrs_server().ok_or_else(|| {
                    Failure::Fatal(anyhow!(
                        "mistral.rs (`mistralrs-server`) is not installed: run `/setup mistral-rs` or select mistral.rs in Dial Desktop, or set DIAL_MISTRALRS_SERVER"
                    ))
                })?;
                let mut command = Command::new(program);
                command.args(mistralrs_args(model, None, port).map_err(Failure::Fatal)?);
                command
            }
            LocalEngine::Vllm => {
                let program = llm::local_format::vllm_server().ok_or_else(|| {
                    Failure::Fatal(anyhow!(
                        "vLLM is not installed: run `/setup vllm` or select vLLM in Dial Desktop to install it, or set DIAL_VLLM_SERVER"
                    ))
                })?;
                if adapter.is_some() {
                    explicit_request_model = Some("dial".to_owned());
                }
                let mut command = Command::new(program);
                command.args(vllm_args(model, adapter, port).map_err(Failure::Fatal)?);
                command
            }
            LocalEngine::Sglang => {
                let program = llm::local_format::sglang_server().ok_or_else(|| {
                    Failure::Fatal(anyhow!(
                        "SGLang is not installed: run `/setup sglang` or select SGLang in Dial Desktop to install it, or set DIAL_SGLANG_SERVER"
                    ))
                })?;
                if adapter.is_some() {
                    explicit_request_model = Some("dial".to_owned());
                }
                let mut command = Command::new(program);
                command.args(sglang_args(model, adapter, port).map_err(Failure::Fatal)?);
                command
            }
            other => {
                return Err(Failure::Fatal(anyhow!(
                    "Unsupported local runtime {} for model {model}",
                    other.label()
                )));
            }
        }
    } else {
        let mut command = Command::new(python);
        // `--opt=value` keeps a value that starts with `-` from being read as a flag.
        command
            .arg("-c")
            .arg(launch_script())
            .arg(if embedding {
                format!("--embedding-model={model}")
            } else {
                format!("--model={model}")
            })
            .arg("--host=127.0.0.1")
            .arg(format!("--port={port}"))
            .arg("--log-level=WARNING");
        if embedding {
            command.arg("--max-num-seqs=1");
        } else if vlm {
            command.arg("--vision-cache-size=1").arg("--max-num-seqs=1");
            // Bounded, process-local reuse; Python disables APC if its compatibility check fails.
            command
                .env("APC_ENABLED", "1")
                .env("APC_DISK_ENABLED", "0")
                .env("APC_NUM_BLOCKS", "256")
                .env("APC_MEMORY_MAX_GB", vision_prefix_cache_gib())
                .env("APC_MEMORY_RESERVE_GB", "0.25")
                .env("APC_CHECKPOINT_ENTRIES", VISION_PREFIX_CHECKPOINTS);
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
                if embedding {
                    "mlx-vlm-embedding"
                } else if vlm {
                    "mlx-vlm"
                } else {
                    "mlx-lm"
                },
            )
            .env("DIAL_PARENT_PID", std::process::id().to_string())
            .env("DIAL_STEERING_SPEC", steering_spec)
            .env("HF_HUB_OFFLINE", "1")
            .env("PYTHONUNBUFFERED", "1");
        command
    };
    // Free memory a GGUF server left resident when an earlier Dial process died without
    // stopping it; servers owned by a live Dial process are never touched.
    #[cfg(unix)]
    if let Some(config) = context {
        crate::platform::process_record::stop_abandoned(&config.user_dir, "models");
    }
    let mut child = crate::platform::process::spawn_group(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped()),
    )
    .with_context(|| {
        format!(
            "starting local model server {}",
            Path::new(command.get_program()).display()
        )
    })
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

    let fallback_model = if let Some(explicit) = explicit_request_model.as_deref() {
        explicit
    } else if vlm || embedding || discover_model_id {
        model
    } else {
        SERVER_MODEL
    };
    let outcome = wait_ready(
        &mut process,
        &base_url,
        &tail,
        deadline,
        interrupt,
        ReadyModel {
            fallback: fallback_model,
            discover_id: discover_model_id && explicit_request_model.is_none(),
            embedding,
        },
    );
    match outcome {
        Ok(discovered) => {
            if let Some(record) = &mut tracking {
                record.phase("loaded").map_err(Failure::Fatal)?;
            }
            let served_model = explicit_request_model.or(discovered);
            Ok(ModelServer {
                process,
                base_url,
                model: model.to_string(),
                served_model,
                vlm,
                embedding,
                adapter: adapter.map(Path::to_path_buf),
                steering: steering.cloned(),
                runtime: runtime.clone(),
                tool_grammar: tool_grammar_enabled(context),
                gguf_context: gguf.is_some().then(|| requested_context(context)),
                tracking,
                stderr: tail,
                last_used: Instant::now(),
            })
        }
        Err(error) => {
            process.stop();
            settle_stderr(&tail);
            let text = tail_text(&tail, ERROR_TAIL_BYTES);
            let message = startup_diagnostic(&error, &text, command.get_program());
            if text.contains("Address already in use") {
                Err(Failure::PortTaken(anyhow!(message)))
            } else {
                Err(Failure::Fatal(anyhow!(message)))
            }
        }
    }
}

struct ReadyModel<'a> {
    fallback: &'a str,
    discover_id: bool,
    embedding: bool,
}

fn wait_ready(
    process: &mut Supervised,
    base_url: &str,
    tail: &Mutex<Tail>,
    deadline: Option<Instant>,
    interrupt: Option<Arc<AtomicBool>>,
    model: ReadyModel<'_>,
) -> Result<Option<String>> {
    let models_url = format!("{base_url}/v1/models");
    let mut last_probe = "no health response yet".to_string();
    let mut discovered_model: Option<String> = None;
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
        match llm::agent_with(2, false, interrupt.clone(), None)
            .get(&models_url)
            .call()
        {
            Ok(mut response) if response.status().as_u16() == 200 => {
                if model.discover_id
                    && let Ok(text) = response.body_mut().read_to_string()
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
                {
                    discovered_model = first_served_model_id(&value);
                }
                break;
            }
            Ok(response) => last_probe = format!("HTTP {}", response.status().as_u16()),
            Err(error) => last_probe = error.to_string(),
        }
        std::thread::sleep(
            remaining.map_or(POLL_INTERVAL, |remaining| POLL_INTERVAL.min(remaining)),
        );
    }
    let warmup_model = discovered_model.as_deref().unwrap_or(model.fallback);
    // The HTTP port opens before the model finishes loading, and a bad
    // model/adapter only fails on first use. One 1-token request blocks until
    // the weights are loaded and proves generation works.
    let remaining = deadline.map_or(0, |deadline| {
        deadline
            .saturating_duration_since(Instant::now())
            .as_secs()
            .max(1)
    });
    let warmed = if model.embedding {
        warm_embedding(base_url, warmup_model, remaining, interrupt.clone())
    } else {
        let warmup = Chat {
            messages: vec![json!({"role": "user", "content": "ping"})],
            tools: Vec::new(),
            temperature: 0.0,
            max_tokens: 1,
            seed: None,
        };
        llm::chat_at_controlled(
            llm::Endpoint {
                headers: &[],
                url: &llm::completions_url(base_url),
                auth: None,
                model: warmup_model,
            },
            &warmup,
            remaining,
            llm::Control {
                interrupt: interrupt.clone(),
                profile: None,
                reasoning_guard: None,
                on_event: &mut |_| true,
            },
        )
        .map(|_| ())
    };
    if let Err(error) = warmed {
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
    Ok(discovered_model)
}

/// One text embedding blocks until the embedding weights load and proves the route works.
fn warm_embedding(
    base_url: &str,
    model: &str,
    timeout_secs: u64,
    interrupt: Option<Arc<AtomicBool>>,
) -> Result<()> {
    let body = json!({"model": model, "input": ["ping"]});
    let mut response = llm::agent_with(timeout_secs, false, interrupt, None)
        .post(&format!("{base_url}/v1/embeddings"))
        .header("Content-Type", "application/json")
        .send(&serde_json::to_vec(&body)?)
        .context("embedding warm-up request failed")?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .context("reading the embedding warm-up response")?;
    ensure!(
        (200..300).contains(&status),
        "embedding warm-up returned HTTP {status}: {}",
        text.chars().take(300).collect::<String>()
    );
    let value: serde_json::Value =
        serde_json::from_str(&text).context("embedding warm-up response is not JSON")?;
    ensure!(
        value["data"][0]["embedding"]
            .as_array()
            .is_some_and(|vector| !vector.is_empty()),
        "embedding warm-up returned no vector"
    );
    Ok(())
}

#[cfg(all(any(test, feature = "test-support"), unix))]
pub mod fixtures;
pub mod image;

#[cfg(all(test, unix))]
pub(crate) mod tests;
