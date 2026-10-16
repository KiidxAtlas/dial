use crate::config::types::{Config, Version};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(unix)]
use std::io::Write;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const MAX_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_DIAGNOSTICS: usize = 256 * 1024;
#[cfg(not(test))]
const YIELD_GRACE: Duration = Duration::from_secs(2);
#[cfg(test)]
const YIELD_GRACE: Duration = Duration::from_millis(200);

struct ProcessGroup {
    child: Child,
    terminated: bool,
}

impl ProcessGroup {
    fn terminate(&mut self) {
        if !self.terminated {
            let _ = crate::platform::process::kill_group(self.child.id());
            self.terminated = true;
        }
    }

    fn exited(&self) -> io::Result<bool> {
        crate::platform::process::exited(&self.child)
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
        let _ = self.child.wait();
    }
}

fn nonblocking(pipe: &impl crate::platform::io::Pipe) -> io::Result<()> {
    crate::platform::io::set_nonblocking(pipe)
}

// Limit work per pass so a noisy child cannot starve the deadline or stdin writer.
fn drain(
    stream: &mut (impl Read + crate::platform::io::Pipe),
    mut output: Option<&mut Vec<u8>>,
    size: &mut usize,
    limit: usize,
    name: &str,
) -> Result<bool> {
    let mut buffer = [0u8; 8192];
    for _ in 0..8 {
        match crate::platform::io::read_nonblocking(stream, &mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                ensure!(*size + count <= limit, "{name} exceeded {limit} bytes");
                *size += count;
                if let Some(output) = output.as_mut() {
                    output.extend_from_slice(&buffer[..count]);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("reading subprocess pipe"),
        }
    }
    Ok(false)
}

#[cfg(unix)]
type WorkerStdin = std::process::ChildStdin;

/// Execute an operator-approved program without a shell. The process group is
/// killed on success too, so background descendants cannot outlive the request.
/// Stderr and input are never included in errors: tools may echo private prompts.
pub fn run_command(
    program: &Path,
    args: &[String],
    input: &str,
    timeout_secs: u64,
) -> Result<String> {
    run_command_controlled(program, args, input, timeout_secs, None, None)
}

/// [`run_command`] that also stops early when `cancel` is set (the process group is killed).
pub fn run_command_cancellable(
    program: &Path,
    args: &[String],
    input: &str,
    timeout_secs: u64,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<String> {
    run_command_controlled(program, args, input, timeout_secs, Some(cancel), None)
}

#[derive(Debug, thiserror::Error)]
#[error("configured tool process ended after execution started; effects require inspection")]
pub struct ToolEffectsUnknown;

/// Configured tools share process cleanup with learning workers, but expose execution certainty.
pub fn run_tool_command(
    program: &Path,
    args: &[String],
    input: &str,
    timeout_secs: u64,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<String> {
    ensure!(input.len() <= MAX_OUTPUT, "subprocess input exceeds 8 MiB");
    let mut command = Command::new(program);
    command
        .args(args)
        .env("DIAL_PARENT_PID", std::process::id().to_string());
    let mut policy =
        crate::runtime::process::Policy::bounded(Duration::from_secs(timeout_secs), MAX_OUTPUT);
    policy.stderr_cap = MAX_DIAGNOSTICS;
    policy.fail_on_output_limit = true;
    let output = crate::runtime::process::run_with_policy(
        command,
        Some(input.as_bytes().to_vec()),
        policy,
        cancel,
        &mut |_, _| {},
    )
    .map_err(anyhow::Error::msg)?;
    let result = if output.success() && !output.output_incomplete {
        String::from_utf8(output.stdout).context("configured subprocess output is not UTF-8")
    } else if output.cancelled {
        Err(anyhow::anyhow!("configured subprocess cancelled"))
    } else if output.timed_out {
        Err(anyhow::anyhow!("configured subprocess timed out"))
    } else if output.output_limited {
        Err(anyhow::anyhow!(
            "configured subprocess output exceeded its limit"
        ))
    } else {
        Err(anyhow::anyhow!(
            "configured subprocess failed or returned incomplete output"
        ))
    };
    result.map_err(|error| {
        if output.started {
            error.context(ToolEffectsUnknown)
        } else {
            error
        }
    })
}

fn run_command_controlled(
    program: &Path,
    args: &[String],
    input: &str,
    timeout_secs: u64,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    stop: Option<&Path>,
) -> Result<String> {
    run_command_handoff(
        program,
        args,
        input,
        ProcessControl {
            timeout_secs,
            cancel,
            stop,
            tracking: None,
        },
        &mut || Ok(()),
    )
}

/// Limits, cancellation and tracking for a worker process; shared by each handoff path.
struct ProcessControl<'a> {
    timeout_secs: u64,
    cancel: Option<&'a std::sync::atomic::AtomicBool>,
    stop: Option<&'a Path>,
    tracking: Option<&'a Config>,
}

fn run_command_handoff(
    program: &Path,
    args: &[String],
    input: &str,
    control: ProcessControl<'_>,
    on_tick: &mut dyn FnMut() -> Result<()>,
) -> Result<String> {
    let ProcessControl {
        timeout_secs,
        cancel,
        stop,
        tracking,
    } = control;
    ensure!(
        !stop.is_some_and(Path::exists),
        "improvement stopped by operator"
    );
    ensure!(input.len() <= MAX_OUTPUT, "subprocess input exceeds 8 MiB");
    let started = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let mut command = Command::new(program);
    command
        .args(args)
        .env("DIAL_PARENT_PID", std::process::id().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = crate::platform::process::spawn_group(&mut command)
        .context("starting configured subprocess")?;
    let mut group = ProcessGroup {
        child,
        terminated: false,
    };
    let operation = serde_json::from_str::<Value>(input)
        .ok()
        .and_then(|value| value["op"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "working".into());
    let _record = tracking
        .map(|config| {
            super::processes::Record::new(
                &config.user_dir,
                group.child.id(),
                "models",
                &config.model,
                &config.project_root,
                &config.state_dir,
                &operation,
            )
        })
        .transpose()?;
    let stdin = group
        .child
        .stdin
        .take()
        .context("missing subprocess stdin")?;
    let mut stdout = group
        .child
        .stdout
        .take()
        .context("missing subprocess stdout")?;
    let mut stderr = group
        .child
        .stderr
        .take()
        .context("missing subprocess stderr")?;
    #[cfg(unix)]
    nonblocking(&stdin).context("setting stdin nonblocking")?;
    nonblocking(&stdout).context("setting stdout nonblocking")?;
    nonblocking(&stderr).context("setting stderr nonblocking")?;
    #[cfg(unix)]
    let mut stdin: Option<WorkerStdin> = Some(stdin);
    #[cfg(windows)]
    let stdin = crate::platform::io::QueuedWriter::new(stdin, 64 * 1024);
    let mut sent = 0;
    let mut output = Vec::new();
    let mut out_size = 0;
    let mut err_size = 0;
    let mut out_done = false;
    let mut err_done = false;
    let mut status = None;
    let mut exited_at = None;
    let mut tick = Instant::now() - Duration::from_secs(1);
    loop {
        if tick.elapsed() >= Duration::from_millis(80) {
            on_tick()?;
            tick = Instant::now();
        }
        ensure!(
            !stop.is_some_and(Path::exists),
            "improvement stopped by operator"
        );
        ensure!(
            !cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst)),
            "capability verification cancelled"
        );
        ensure!(
            timeout_secs == 0 || started.elapsed() < timeout,
            "subprocess timed out after {timeout_secs}s"
        );
        #[cfg(unix)]
        if let Some(writer) = stdin.as_mut() {
            match writer.write(&input.as_bytes()[sent..]) {
                Ok(count) => sent += count,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error).context("writing subprocess input"),
            }
            if sent == input.len() {
                stdin = None;
            }
        }
        #[cfg(windows)]
        {
            match stdin.try_write(&input.as_bytes()[sent..]) {
                Ok(count) => sent += count,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error).context("writing subprocess input"),
            }
        }
        if !out_done {
            out_done = drain(
                &mut stdout,
                Some(&mut output),
                &mut out_size,
                MAX_OUTPUT,
                "subprocess stdout",
            )?;
        }
        if !err_done {
            err_done = drain(
                &mut stderr,
                None,
                &mut err_size,
                MAX_DIAGNOSTICS,
                "subprocess stderr",
            )?;
        }
        if status.is_none() && group.exited().context("observing subprocess exit")? {
            group.terminate();
            status = Some(group.child.wait().context("reaping subprocess")?);
            exited_at = Some(Instant::now());
        }
        if status.is_some() && out_done && err_done {
            break;
        }
        ensure!(
            !exited_at.is_some_and(|at| at.elapsed() >= Duration::from_millis(300)),
            "subprocess exited but its output pipes did not close; inherited descriptors remained open"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    #[cfg(windows)]
    {
        ensure!(
            stdin.delivered_bytes() == input.len(),
            "subprocess exited before consuming its full request"
        );
        ensure!(
            stdin
                .finish(Duration::from_secs(2))
                .context("finishing subprocess input")?,
            "timed out finishing subprocess input"
        );
    }
    let status = status.context("subprocess has no exit status")?;
    ensure!(
        status.success(),
        "subprocess failed ({status}); {err_size} stderr bytes withheld to protect request data"
    );
    #[cfg(unix)]
    ensure!(
        sent == input.len(),
        "subprocess exited before consuming its full request"
    );
    String::from_utf8(output).context("subprocess stdout is not UTF-8")
}

pub fn call(config: &Config, request: &Value) -> Result<Value> {
    call_controlled(config, request, None)
}
pub fn call_controlled(
    config: &Config,
    request: &Value,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Value> {
    crate::setup::refresh_worker(config)?;
    ensure!(
        config.python.is_absolute() && config.worker.is_absolute(),
        "python and worker paths must be absolute"
    );
    let input = serde_json::to_string(request).context("serializing worker request")?;
    let worker = config.worker.to_str().context("worker path is not UTF-8")?;
    let output = loop {
        let result = run_command_handoff(
            &config.python,
            &[worker.to_owned()],
            &input,
            ProcessControl {
                timeout_secs: config.worker_timeout_secs,
                cancel,
                stop: Some(&config.state_dir.join("improvement.stop")),
                tracking: Some(config),
            },
            &mut || {
                if super::compute::background_active()
                    && (super::compute::paused(&config.user_dir)
                        || (config.compute_locks
                            && super::compute::foreground_pending(&config.user_dir)?))
                {
                    return Err(super::compute::Preempted.into());
                }
                Ok(())
            },
        );
        match result {
            Err(error) if error.downcast_ref::<super::compute::Preempted>().is_some() => {
                super::compute::Background::yield_controlled(config, cancel)?
            }
            result => break result?,
        }
    };
    let value: Value = serde_json::from_str(&output).context("worker returned invalid JSON")?;
    ensure!(value.is_object(), "worker response must be an object");
    Ok(value)
}

#[derive(Deserialize)]
struct Capabilities {
    inference: bool,
    lora: bool,
    reason: String,
    unsupported: Vec<String>,
    resolved_model: Option<PathBuf>,
}

/// Bump when adapter target selection or the verification contract changes.
pub fn current_lora_probe(caps: &Value) -> bool {
    caps["lora_probe_version"].as_u64() == Some(2)
}

/// Capabilities of GGUF and Splash models, answered without the MLX-only Python worker: GGUF serves
/// inference through `llama-server` but cannot be LoRA-trained (training needs MLX weights), and a
/// Splash bundle can only be run by LM Studio. `None` for MLX checkpoints and unresolved models.
fn non_mlx_capabilities(model: &str) -> Option<Value> {
    use crate::llm::local_format::{self, LocalFormat};
    let resolved = crate::llm::catalog::resolve_local_model(model).ok();
    let path = std::path::absolute(resolved.as_deref().unwrap_or(model)).ok()?;
    let unsupported = [
        "expert_growth",
        "routing_calibration",
        "attention_retrofit",
        "learned_step_probe",
    ];
    let (inference, reason) = match local_format::detect(&path)? {
        LocalFormat::Mlx => return None,
        LocalFormat::Gguf if local_format::llama_server().is_some() => (
            true,
            "GGUF model served by llama.cpp: inference and evaluation are supported; LoRA training needs MLX weights".to_owned(),
        ),
        LocalFormat::Gguf => (
            false,
            "GGUF models need llama.cpp's `llama-server`: install llama.cpp or set DIAL_LLAMA_SERVER".to_owned(),
        ),
        LocalFormat::Splash => (
            false,
            "Splash bundles can only be run by LM Studio: select the model under the LM Studio provider".to_owned(),
        ),
    };
    Some(json!({
        "inference": inference,
        "lora": false,
        "lora_probe_version": 2,
        "reason": reason,
        "unsupported": unsupported,
        "resolved_model": path,
    }))
}

pub fn capabilities(config: &Config) -> Result<Value> {
    if let Some(result) = non_mlx_capabilities(&config.model) {
        return validate_capabilities(result);
    }
    let result = call(
        config,
        &json!({"op": "capabilities", "model": config.model}),
    )?;
    validate_capabilities(result)
}

pub fn capabilities_controlled(
    config: &Config,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Value> {
    if let Some(result) = non_mlx_capabilities(&config.model) {
        return validate_capabilities(result);
    }
    crate::setup::refresh_worker(config)?;
    ensure!(
        config.python.is_absolute() && config.worker.is_absolute(),
        "python and worker paths must be absolute"
    );
    let input = json!({"op":"capabilities", "model":config.model}).to_string();
    let output = run_command_handoff(
        &config.python,
        &[config
            .worker
            .to_str()
            .context("worker path is not UTF-8")?
            .to_owned()],
        &input,
        ProcessControl {
            timeout_secs: config.worker_timeout_secs,
            cancel: Some(cancel),
            stop: None,
            tracking: Some(config),
        },
        &mut || Ok(()),
    )?;
    validate_capabilities(serde_json::from_str(&output).context("worker returned invalid JSON")?)
}

fn validate_capabilities(result: Value) -> Result<Value> {
    let parsed: Capabilities =
        serde_json::from_value(result.clone()).context("invalid capability response")?;
    ensure!(
        !parsed.reason.trim().is_empty(),
        "worker omitted capability reason"
    );
    ensure!(
        !parsed.lora || parsed.inference,
        "LoRA capability requires inference support"
    );
    if parsed.inference {
        ensure!(
            parsed
                .resolved_model
                .as_ref()
                .is_some_and(|path| path.is_absolute() && (path.is_dir() || path.is_file())),
            "worker omitted resolved absolute model path"
        );
    }
    for feature in [
        "expert_growth",
        "routing_calibration",
        "attention_retrofit",
        "learned_step_probe",
    ] {
        ensure!(
            parsed.unsupported.iter().any(|item| item == feature),
            "worker omitted unsupported structural capability"
        );
    }
    Ok(result)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Generation {
    text: String,
}

pub fn generate(
    config: &Config,
    model: &str,
    adapter: Option<&Path>,
    messages: &[Value],
    temperature: f64,
    max_tokens: u32,
    seed: u64,
) -> Result<String> {
    ensure!(
        temperature.is_finite() && (0.0..=2.0).contains(&temperature),
        "temperature must be in [0, 2]"
    );
    ensure!(
        (1..=32768).contains(&max_tokens),
        "max_tokens must be in [1, 32768]"
    );
    let result = call(
        config,
        &json!({
            "op": "generate", "model": model, "adapter": adapter,
            "messages": messages, "temperature": temperature, "max_tokens": max_tokens, "seed": seed,
        }),
    )?;
    let generation: Generation =
        serde_json::from_value(result).context("invalid generation response")?;
    Ok(generation.text)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Training {
    adapter: PathBuf,
}

pub fn train(config: &Config, active: &Version, output_dir: &Path) -> Result<PathBuf> {
    ensure!(config.training.enabled, "training is disabled");
    fresh_output(output_dir)?;
    let training = &config.training;
    let mut request = json!({
        "op": "train", "model": config.model, "resume_adapter": active.adapter,
        "train_data": training.train_data, "valid_data": training.valid_data,
        "output_dir": output_dir, "iters": training.iters, "batch_size": training.batch_size,
        "num_layers": training.num_layers, "rank": training.rank, "max_seq_length": training.max_seq_length,
    });
    let response = if super::compute::background_active() {
        crate::setup::refresh_worker(config)?;
        ensure!(
            config.python.is_absolute() && config.worker.is_absolute(),
            "python and worker paths must be absolute"
        );
        let control = tempfile::tempdir()?;
        let pause = control.path().join("pause");
        let progress = control.path().join("progress.json");
        request["pause_file"] = json!(pause);
        request["progress_file"] = json!(progress);
        request["resume_checkpoint"] = json!(false);
        let store = crate::storage::store::Store::open(
            &config.state_dir,
            &config.model,
            &config.use_case,
            &config.harness,
        )?;
        let started = store
            .get_meta("cycle_progress")?
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|value| value["started"].as_u64())
            .unwrap_or(super::improve::now());
        let mut last_progress = String::new();
        loop {
            let input = request.to_string();
            let mut requested = None;
            let output = run_command_handoff(
                &config.python,
                &[config
                    .worker
                    .to_str()
                    .context("worker path is not UTF-8")?
                    .to_owned()],
                &input,
                ProcessControl {
                    timeout_secs: config.worker_timeout_secs,
                    cancel: None,
                    stop: Some(&config.state_dir.join("improvement.stop")),
                    tracking: Some(config),
                },
                &mut || {
                    if super::compute::paused(&config.user_dir)
                        || (config.compute_locks
                            && super::compute::foreground_pending(&config.user_dir)?)
                    {
                        if !pause.exists() {
                            std::fs::write(&pause, "")?;
                        }
                        let since = requested.get_or_insert_with(Instant::now);
                        if since.elapsed() >= YIELD_GRACE {
                            return Err(super::compute::Preempted.into());
                        }
                    }
                    if let Ok(raw) = std::fs::read_to_string(&progress)
                        && raw.len() <= 8192
                        && raw != last_progress
                    {
                        let value: Value = serde_json::from_str(&raw)?;
                        store.set_meta("training_progress", &raw)?;
                        store.set_meta("cycle_progress",&json!({"stage":value["stage"],"at":super::improve::now(),"started":started,"training":value}).to_string())?;
                        last_progress = raw;
                    }
                    Ok(())
                },
            );
            let output = match output {
                Ok(output) => output,
                Err(error) if error.downcast_ref::<super::compute::Preempted>().is_some() => {
                    // The worker is reaped before unlocking compute. Keep only the last atomically
                    // committed step; an unfinished gradient is retried after the coding turn.
                    let saved = output_dir.join("resume.json").is_file();
                    if !saved && output_dir.exists() {
                        std::fs::remove_dir_all(output_dir)?;
                    }
                    store.set_meta("lora_phase", "paused")?;
                    store.set_meta("cycle_progress",&json!({"stage":"training preempted for foreground work; waiting for idle compute","started":started,"at":super::improve::now()}).to_string())?;
                    super::compute::Background::yield_to_coding(config)?;
                    if pause.exists() {
                        std::fs::remove_file(&pause)?;
                    }
                    request["resume_checkpoint"] = json!(saved);
                    store.set_meta("lora_phase", "training")?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let value: Value =
                serde_json::from_str(&output).context("worker returned invalid training JSON")?;
            if value["paused"] != true {
                break value;
            }
            ensure!(
                output_dir.join("resume.json").is_file(),
                "paused training did not commit a checkpoint"
            );
            let checkpoint_progress = std::fs::read_to_string(&progress)
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
            let stage = if let Some(value) = &checkpoint_progress {
                store.set_meta("training_progress", &value.to_string())?;
                format!(
                    "training paused for coding; checkpoint saved ({} {}/{}), waiting for idle compute",
                    value["phase"].as_str().unwrap_or("step"),
                    value["iteration"],
                    value["total"]
                )
            } else {
                "training paused for coding; checkpoint saved, waiting for idle compute".into()
            };
            store.set_meta("lora_phase", "paused")?;
            store.set_meta("cycle_progress",&json!({"stage":stage,"at":super::improve::now(),"started":started,"training":checkpoint_progress}).to_string())?;
            super::compute::Background::yield_to_coding(config)?;
            std::fs::remove_file(&pause)?;
            request["resume_checkpoint"] = json!(true);
            store.set_meta("lora_phase", "training")?;
            store.set_meta("cycle_progress",&json!({"stage":"resuming training from checkpoint","at":super::improve::now(),"started":started}).to_string())?;
        }
    } else {
        call(config, &request)?
    };
    let trained: Training =
        serde_json::from_value(response).context("invalid training response")?;
    produced_adapter(&trained.adapter, output_dir)
}

fn fresh_output(output_dir: &Path) -> Result<()> {
    ensure!(
        output_dir.is_absolute(),
        "adapter output directory must be absolute"
    );
    ensure!(
        !output_dir.exists(),
        "adapter output directory already exists"
    );
    Ok(())
}

/// The worker must report exactly `output_dir`, holding both nonempty adapter artifacts.
fn produced_adapter(reported: &Path, output_dir: &Path) -> Result<PathBuf> {
    ensure!(
        reported.is_absolute(),
        "worker returned a relative adapter path"
    );
    let expected = output_dir
        .canonicalize()
        .context("missing adapter output directory")?;
    ensure!(
        reported == expected,
        "worker returned a different adapter directory"
    );
    for artifact in ["adapter_config.json", "adapters.safetensors"] {
        let metadata =
            std::fs::metadata(expected.join(artifact)).context("missing adapter artifact")?;
        ensure!(
            metadata.is_file() && metadata.len() > 0,
            "empty or invalid adapter artifact"
        );
    }
    Ok(expected)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Score {
    pub examples: usize,
    pub tokens: usize,
    pub mean_logprob: f64,
    /// Per-target-token mean log-probability, in file order.
    pub per_example: Vec<f64>,
    pub pair_examples: usize,
    /// Mean of logp(chosen) − logp(rejected) over preference pairs; None without pairs.
    pub pair_margin: Option<f64>,
    pub per_pair: Vec<f64>,
}

/// Teacher-forced log-probability of the verified targets in `data` under the training loss mask.
pub fn score(
    config: &Config,
    model: &str,
    adapter: Option<&Path>,
    steering: Option<(&Path, f64)>,
    data: &Path,
    max_examples: usize,
) -> Result<Score> {
    ensure!(
        (1..=256).contains(&max_examples),
        "max_examples must be in [1, 256]"
    );
    ensure!(data.is_absolute(), "scoring data path must be absolute");
    let steering = match steering {
        Some((bundle, strength)) => {
            ensure!(
                strength.is_finite() && strength > 0.0,
                "steering strength must be positive"
            );
            json!({"bundle": bundle, "strength": strength})
        }
        None => Value::Null,
    };
    let result = call(
        config,
        &json!({
            "op": "score", "model": model, "adapter": adapter, "steering": steering, "data": data,
            "max_examples": max_examples, "max_seq_length": config.training.max_seq_length,
        }),
    )?;
    let score: Score = serde_json::from_value(result).context("invalid score response")?;
    ensure!(
        (1..=max_examples).contains(&score.examples)
            && score.per_example.len() == score.examples
            && score.tokens >= score.examples
            && score.per_pair.len() == score.pair_examples
            && score.pair_margin.is_some() == (score.pair_examples > 0),
        "worker reported inconsistent score counts"
    );
    ensure!(
        score.mean_logprob.is_finite()
            && score.pair_margin.is_none_or(f64::is_finite)
            && score
                .per_example
                .iter()
                .chain(&score.per_pair)
                .all(|x| x.is_finite()),
        "worker returned a nonfinite score"
    );
    Ok(score)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Combined {
    pub output: PathBuf,
    pub rank: u32,
    pub keys: Vec<String>,
    pub num_layers: u32,
}

/// Exact weighted sum of LoRA adapters (rank concatenation), written as one resumable adapter.
pub fn adapter_combine(
    config: &Config,
    model: &str,
    inputs: &[(&Path, f64)],
    output: &Path,
) -> Result<Combined> {
    ensure!(!inputs.is_empty(), "adapter combination needs inputs");
    ensure!(
        inputs
            .iter()
            .all(|(adapter, weight)| adapter.is_absolute() && weight.is_finite()),
        "combined adapters need absolute paths and finite weights"
    );
    fresh_output(output)?;
    let inputs = inputs
        .iter()
        .map(|(adapter, weight)| json!({"adapter": adapter, "weight": weight}))
        .collect::<Vec<_>>();
    let result = call(
        config,
        &json!({"op": "adapter_combine", "model": model, "inputs": inputs, "output": output}),
    )?;
    let mut combined: Combined =
        serde_json::from_value(result).context("invalid adapter combination response")?;
    combined.output = produced_adapter(&combined.output, output)?;
    ensure!(
        combined.rank > 0 && combined.num_layers > 0 && !combined.keys.is_empty(),
        "worker returned an empty adapter combination"
    );
    Ok(combined)
}

#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fused {
    /// Standalone MLX checkpoint: the base with the adapter merged in.
    pub output: PathBuf,
    pub model_type: Option<String>,
    pub layers: u32,
    /// Full-precision weights (llama.cpp converter input) rather than the base quantization.
    pub dequantized: bool,
}

/// Merge an MLX adapter into `model`, writing a standalone checkpoint that any MLX server (Dial,
/// LM Studio, mlx_lm) loads without the adapter. `dequantize` writes full-precision weights, the
/// input llama.cpp's `convert_hf_to_gguf.py` expects; otherwise the base quantization is kept.
pub fn fuse(
    config: &Config,
    model: &str,
    adapter: &Path,
    output: &Path,
    dequantize: bool,
) -> Result<Fused> {
    let model = crate::llm::catalog::resolve_local_model(model)?;
    ensure!(
        crate::llm::local_format::detect(Path::new(&model))
            == Some(crate::llm::local_format::LocalFormat::Mlx),
        "fusing needs a local MLX base checkpoint"
    );
    ensure!(adapter.is_absolute(), "adapter path must be absolute");
    fresh_output(output)?;
    let result = call(
        config,
        &json!({"op": "fuse", "model": model, "adapter": adapter, "output": output, "dequantize": dequantize}),
    )?;
    let fused: Fused = serde_json::from_value(result).context("invalid fuse response")?;
    ensure!(
        same_dir(&fused.output, output)
            && fused.dequantized == dequantize
            && crate::llm::local_format::detect(output)
                == Some(crate::llm::local_format::LocalFormat::Mlx),
        "worker did not produce the requested checkpoint"
    );
    Ok(fused)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baked {
    pub output: PathBuf,
    pub layer: u32,
    pub module: String,
    pub relative_error: f64,
    pub samples: u32,
}

/// Closed-form rank-1 LoRA reproducing `strength`·vector at the bundle layer (ridge least squares
/// over attention-output inputs collected on `model`+`adapter` from prompts in `data`).
pub fn steering_bake(
    config: &Config,
    model: &str,
    adapter: Option<&Path>,
    (bundle, strength): (&Path, f64),
    data: &Path,
    ridge: f64,
    output: &Path,
) -> Result<Baked> {
    ensure!(
        strength.is_finite() && strength > 0.0,
        "steering strength must be positive"
    );
    ensure!(ridge.is_finite() && ridge > 0.0, "ridge must be positive");
    ensure!(
        bundle.is_absolute() && data.is_absolute(),
        "steering bundle and data paths must be absolute"
    );
    fresh_output(output)?;
    let result = call(
        config,
        &json!({
            "op": "steering_bake", "model": model, "adapter": adapter, "bundle": bundle,
            "strength": strength, "data": data, "ridge": ridge, "output": output,
        }),
    )?;
    let mut baked: Baked =
        serde_json::from_value(result).context("invalid steering bake response")?;
    baked.output = produced_adapter(&baked.output, output)?;
    ensure!(
        baked.samples > 0 && baked.relative_error.is_finite() && baked.relative_error >= 0.0,
        "worker returned an invalid steering bake"
    );
    Ok(baked)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sensitivity {
    layers: Vec<f64>,
    loss: f64,
    samples: u32,
}

/// Per-layer LoRA gradient-norm scores (index 0 = first layer) on up to `samples` verified examples.
pub fn sensitivity(
    config: &Config,
    model: &str,
    train_data: &Path,
    rank: u32,
    samples: u32,
    max_seq_length: u32,
) -> Result<Vec<f64>> {
    ensure!((1..=128).contains(&rank), "rank must be in [1, 128]");
    ensure!((1..=64).contains(&samples), "samples must be in [1, 64]");
    ensure!(
        (32..=8192).contains(&max_seq_length),
        "max_seq_length must be in [32, 8192]"
    );
    ensure!(
        train_data.is_absolute(),
        "training data path must be absolute"
    );
    let result = call(
        config,
        &json!({
            "op": "sensitivity", "model": model, "train_data": train_data,
            "rank": rank, "samples": samples, "max_seq_length": max_seq_length,
        }),
    )?;
    let parsed: Sensitivity =
        serde_json::from_value(result).context("invalid sensitivity response")?;
    ensure!(!parsed.layers.is_empty(), "worker returned no layer scores");
    ensure!(
        parsed
            .layers
            .iter()
            .all(|score| score.is_finite() && *score >= 0.0),
        "worker returned invalid layer scores"
    );
    ensure!(parsed.loss.is_finite(), "worker returned a nonfinite loss");
    ensure!(
        (1..=samples).contains(&parsed.samples),
        "worker reported an invalid sample count"
    );
    Ok(parsed.layers)
}

/// mlx_lm trains the last N layers: the smallest N in `[min, max]` (clamped to the layer count)
/// whose last-N suffix holds at least `coverage` of the total score mass; `min` when no usable mass.
/// Nonempty models always select at least one layer: MLX interprets a zero suffix as all layers.
pub fn choose_layers(scores: &[f64], coverage: f64, min: u32, max: u32) -> u32 {
    let count = u32::try_from(scores.len()).unwrap_or(u32::MAX);
    let upper = max.max(1).min(count);
    let lower = min.max(1).min(upper);
    let usable = |score: &f64| {
        if score.is_finite() && *score > 0.0 {
            *score
        } else {
            0.0
        }
    };
    let total: f64 = scores.iter().map(usable).sum();
    if total <= 0.0 || !total.is_finite() || coverage.is_nan() {
        return lower;
    }
    let target = coverage.clamp(0.0, 1.0) * total;
    let mut mass = 0.0;
    for (taken, score) in (1u32..).zip(scores.iter().rev().map(usable)) {
        if taken > upper {
            break;
        }
        mass += score;
        if taken >= lower && mass >= target {
            return taken;
        }
    }
    upper
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gguf_and_splash_models_answer_capabilities_without_the_mlx_worker() {
        let root = tempfile::tempdir().unwrap();
        let gguf = root.path().join("m.gguf");
        std::fs::write(&gguf, b"GGUF\x03\0\0\0rest").unwrap();
        let caps = non_mlx_capabilities(gguf.to_str().unwrap()).unwrap();
        assert_eq!(caps["lora"], false);
        assert_eq!(
            caps["inference"],
            crate::llm::local_format::llama_server().is_some()
        );
        validate_capabilities(caps).unwrap();

        let splash = root.path().join("splash");
        for dir in ["target", "tokenizer"] {
            std::fs::create_dir_all(splash.join(dir)).unwrap();
        }
        std::fs::write(splash.join("manifest.json"), "{}").unwrap();
        let caps = non_mlx_capabilities(splash.to_str().unwrap()).unwrap();
        assert_eq!(caps["inference"], false);
        assert!(caps["reason"].as_str().unwrap().contains("LM Studio"));
        validate_capabilities(caps).unwrap();

        let mlx = root.path().join("mlx");
        std::fs::create_dir(&mlx).unwrap();
        std::fs::write(mlx.join("config.json"), "{}").unwrap();
        std::fs::write(mlx.join("model.safetensors"), b"x").unwrap();
        assert!(non_mlx_capabilities(mlx.to_str().unwrap()).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_handoff_delivers_stdin_and_drains_output() {
        let output = run_command(
            Path::new("cmd.exe"),
            &["/C".into(), "more".into()],
            "worker input\n",
            10,
        )
        .unwrap();
        assert_eq!(output, "worker input\n");
    }

    #[cfg(unix)]
    #[test]
    fn training_exits_before_coding_and_resumes_after_compute_is_released() {
        handoff_fixture(false);
    }
    #[cfg(unix)]
    #[test]
    fn unresponsive_training_is_preempted_and_retries_its_committed_checkpoint() {
        handoff_fixture(true);
    }
    #[cfg(unix)]
    #[test]
    fn fuse_publishes_a_checkpoint_and_validates_the_worker_reply() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("base");
        std::fs::create_dir(&base).unwrap();
        std::fs::write(base.join("config.json"), "{}").unwrap();
        std::fs::write(base.join("model.safetensors"), "x").unwrap();
        let adapter = root.path().join("adapter");
        let request = root.path().join("request.json");
        let config = Config {
            state_dir: root.path().join("state"),
            user_dir: root.path().join("user"),
            python: PathBuf::from("/usr/bin/python3"),
            worker: root.path().join("worker.py"),
            model: "fixture".into(),
            worker_timeout_secs: 60,
            ..Config::default()
        };
        std::fs::write(
            &config.worker,
            format!(
                r#"
import json,sys
from pathlib import Path
r=json.load(sys.stdin)
Path({request:?}).write_text(json.dumps(r))
out=Path(r['output'])
out.mkdir()
(out/'config.json').write_text('{{}}')
if out.name != 'empty':
    (out/'model.safetensors').write_text('x')
print(json.dumps({{'output':str(out),'model_type':'llama','layers':4,'dequantized':r['dequantize']}}))
"#,
                request = request.to_str().unwrap()
            ),
        )
        .unwrap();
        let base_name = base.to_str().unwrap();

        let output = root.path().join("twin");
        let fused = fuse(&config, base_name, &adapter, &output, false).unwrap();
        assert_eq!(
            fused.output.canonicalize().unwrap(),
            output.canonicalize().unwrap()
        );
        assert_eq!((fused.layers, fused.dequantized), (4, false));
        let sent: Value =
            serde_json::from_str(&std::fs::read_to_string(&request).unwrap()).unwrap();
        assert_eq!(sent["op"], "fuse");
        assert_eq!(sent["dequantize"], false);
        assert_eq!(sent["adapter"], adapter.to_str().unwrap());
        let full = fuse(
            &config,
            base_name,
            &adapter,
            &root.path().join("full"),
            true,
        )
        .unwrap();
        assert!(full.dequantized);

        let error = fuse(&config, base_name, &adapter, &output, false).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        let error = fuse(
            &config,
            base_name,
            &adapter,
            &root.path().join("empty"),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("did not produce"), "{error}");
        let gguf_base = root.path().join("m.gguf");
        std::fs::write(&gguf_base, b"GGUF\x03\0\0\0").unwrap();
        let error = fuse(
            &config,
            gguf_base.to_str().unwrap(),
            &adapter,
            &root.path().join("g"),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("MLX base"), "{error}");
    }

    #[cfg(unix)]
    fn handoff_fixture(unresponsive: bool) {
        use fs2::FileExt;
        let root = tempfile::tempdir().unwrap();
        let mut config = Config {
            state_dir: root.path().join("state"),
            user_dir: root.path().join("user"),
            python: PathBuf::from("/usr/bin/python3"),
            worker: root.path().join("worker.py"),
            model: "fixture".into(),
            use_case: "project".into(),
            worker_timeout_secs: 60,
            ..Config::default()
        };
        config.training.enabled = true;
        let started = root.path().join("started");
        let coded = root.path().join("coded");
        std::fs::write(
            &config.worker,
            format!(
                r#"
import json,sys,time
from pathlib import Path
r=json.load(sys.stdin)
out=Path(r['output_dir'])
out.mkdir(exist_ok=True)
if not r['resume_checkpoint']:
    Path({started:?}).touch()
    if {unresponsive}:
        (out/'resume.json').write_text('{{"completed":2}}')
        time.sleep(30)
    deadline=time.monotonic()+60
    while not Path(r['pause_file']).exists():
        assert time.monotonic()<deadline
        time.sleep(0.01)
    (out/'resume.json').write_text('{{"completed":2}}')
    print(json.dumps({{'paused':True}}))
else:
    assert Path({coded:?}).exists(), 'training resumed before coding completed'
    for name in ['adapter_config.json','adapters.safetensors']:
        (out/name).write_text('fixture')
    print(json.dumps({{'adapter':str(out.resolve())}}))
"#,
                started = started.to_str().unwrap(),
                coded = coded.to_str().unwrap(),
                unresponsive = if unresponsive { "True" } else { "False" },
            ),
        )
        .unwrap();
        let store = crate::storage::store::Store::open(
            &config.state_dir,
            &config.model,
            &config.use_case,
            &config.harness,
        )
        .unwrap();
        let active = store.active().unwrap();
        let _lease = super::super::compute::Background::acquire(&config).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let deadline = Instant::now() + Duration::from_secs(60);
                while !started.exists() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                let request = super::super::compute::Request::foreground(&config.user_dir).unwrap();
                let lock =
                    super::super::auto::lock_file(&config.user_dir.join("compute.lock")).unwrap();
                while fs2::FileExt::try_lock_shared(&lock).is_err() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                drop(request);
                let reader =
                    crate::storage::store::Store::open_read_only(&config.state_dir).unwrap();
                assert_eq!(
                    reader.get_meta("lora_phase").unwrap().as_deref(),
                    Some("paused")
                );
                std::thread::sleep(Duration::from_millis(100));
                std::fs::write(&coded, "").unwrap();
                FileExt::unlock(&lock).unwrap();
            });
            let output = root.path().join("adapter");
            assert_eq!(
                train(&config, &active, &output).unwrap(),
                output.canonicalize().unwrap()
            );
        });
        assert!(
            super::super::compute::can_yield(),
            "the resumed cycle must own compute before evaluating"
        );
        assert_eq!(
            store.active().unwrap().id,
            active.id,
            "checkpointing must not activate a candidate"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stop_request_terminates_running_training_subprocess() {
        let root = tempfile::tempdir().unwrap();
        let stop = root.path().join("improvement.stop");
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                std::fs::write(&stop, "").unwrap();
            });
            let started = Instant::now();
            let error = run_command_controlled(
                Path::new("/bin/sh"),
                &["-c".into(), "sleep 30".into()],
                "",
                10,
                None,
                Some(&stop),
            )
            .unwrap_err();
            assert!(error.to_string().contains("stopped by operator"));
            assert!(started.elapsed() < Duration::from_secs(3));
        });
    }

    #[cfg(unix)]
    #[test]
    fn lora_verification_cancellation_stops_the_worker_promptly() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(40));
                cancel.store(true, Ordering::SeqCst);
            });
            let error = run_command_controlled(
                Path::new("/bin/sh"),
                &["-c".into(), "sleep 30".into()],
                "",
                10,
                Some(&cancel),
                None,
            )
            .unwrap_err();
            assert!(error.to_string().contains("cancelled"));
        });
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn lora_probe_validates_supported_and_unsupported_results() {
        let root = tempfile::tempdir().unwrap();
        let mut caps = json!({"inference":true,"lora":true,"resolved_model":root.path(),"reason":"supported query targets",
            "unsupported":["expert_growth","routing_calibration","attention_retrofit","learned_step_probe"]});
        assert!(validate_capabilities(caps.clone()).is_ok());
        caps["lora"] = json!(false);
        caps["reason"] = json!("fused QKV unsupported");
        assert_eq!(
            validate_capabilities(caps.clone()).unwrap()["reason"],
            "fused QKV unsupported"
        );
        caps["inference"] = json!(false);
        caps["lora"] = json!(true);
        assert!(validate_capabilities(caps).is_err());
    }

    #[test]
    fn choose_layers_takes_smallest_covering_suffix() {
        let scores = [1.0, 1.0, 2.0, 6.0];
        assert_eq!(choose_layers(&scores, 0.5, 1, 4), 1);
        assert_eq!(choose_layers(&scores, 0.6, 1, 4), 1);
        assert_eq!(choose_layers(&scores, 0.61, 1, 4), 2);
        assert_eq!(choose_layers(&scores, 0.8, 1, 4), 2);
        assert_eq!(choose_layers(&scores, 0.81, 1, 4), 3);
        assert_eq!(choose_layers(&scores, 1.0, 1, 4), 4);
    }

    #[test]
    fn choose_layers_is_monotonic_in_coverage() {
        let scores = [3.0, 0.5, 2.0, 0.0, 1.5, 4.0, 0.1];
        let mut previous = 0;
        for step in 0..=100 {
            let chosen = choose_layers(&scores, f64::from(step) / 100.0, 1, 7);
            assert!(
                chosen >= previous,
                "coverage {step}% chose {chosen} after {previous}"
            );
            previous = chosen;
        }
        assert_eq!(previous, 7);
    }

    #[test]
    fn choose_layers_clamps_to_bounds_and_layer_count() {
        let scores = [1.0, 1.0, 1.0, 1.0];
        assert_eq!(choose_layers(&scores, 0.0, 3, 4), 3);
        assert_eq!(choose_layers(&scores, 1.0, 1, 2), 2);
        assert_eq!(choose_layers(&scores, 1.0, 1, 100), 4);
        assert_eq!(choose_layers(&scores, 0.1, 10, 100), 4);
        assert_eq!(choose_layers(&scores, 1.0, 5, 2), 2);
        assert_eq!(choose_layers(&scores, 7.0, 1, 4), 4);
        assert_eq!(choose_layers(&scores, 0.0, 0, 4), 1);
        assert_eq!(choose_layers(&scores, 1.0, 0, 0), 1);
    }

    #[test]
    fn choose_layers_degenerate_input_returns_min() {
        assert_eq!(choose_layers(&[], 0.9, 2, 8), 0);
        assert_eq!(choose_layers(&[0.0, 0.0, 0.0], 0.9, 2, 8), 2);
        assert_eq!(
            choose_layers(&[f64::NAN, f64::INFINITY, -1.0], 0.9, 2, 8),
            2
        );
        assert_eq!(choose_layers(&[0.0, 0.0], 0.9, 5, 8), 2);
        assert_eq!(choose_layers(&[1.0, 2.0], f64::NAN, 1, 2), 1);
    }

    #[test]
    fn choose_layers_ignores_nonfinite_scores_in_mass() {
        assert_eq!(choose_layers(&[5.0, f64::NAN, 5.0], 0.5, 1, 3), 1);
    }

    #[test]
    #[ignore = "needs python/.venv and the cached Qwen2.5-0.5B-Instruct-4bit model"]
    fn real_sensitivity_scores_every_layer() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let python = root.join("python/.venv/bin/python");
        let worker = root.join("python/dial_worker.py");
        let data =
            std::env::temp_dir().join(format!("dial-sensitivity-{}.jsonl", std::process::id()));
        let mut lines = String::new();
        for n in 0..6 {
            let record = json!({"messages": [{"role": "user", "content": format!("What is {n} + 3?")},
                {"role": "assistant", "content": format!("The answer is {}.", n + 3)}],
                "source": "test", "verified": true});
            lines.push_str(&format!("{record}\n"));
        }
        std::fs::write(&data, lines).unwrap();
        let config = Config {
            python,
            worker,
            worker_timeout_secs: 600,
            ..Config::default()
        };
        let result = sensitivity(
            &config,
            "mlx-community/Qwen2.5-0.5B-Instruct-4bit",
            &data,
            8,
            9,
            512,
        );
        let _ = std::fs::remove_file(&data);
        let scores = result.unwrap();
        assert_eq!(scores.len(), 24);
        assert!(
            scores.iter().all(|score| score.is_finite() && *score > 0.0),
            "{scores:?}"
        );
        println!("{scores:?}");
        let chosen = choose_layers(&scores, 0.5, 2, 24);
        println!("chosen suffix: {chosen}");
        assert!((2..=24).contains(&chosen));
    }
}
