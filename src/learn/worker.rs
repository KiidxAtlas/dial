use crate::config::types::{Config, Version};
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const MAX_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_DIAGNOSTICS: usize = 256 * 1024;

struct ProcessGroup {
    child: Child,
    terminated: bool,
}

impl ProcessGroup {
    fn terminate(&mut self) {
        if !self.terminated {
            // The leader is not reaped until after killpg, so its PID cannot be reused.
            unsafe { libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL) };
            self.terminated = true;
        }
    }

    fn exited(&self) -> io::Result<bool> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error);
        }
        Ok(unsafe { info.si_pid() } != 0)
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
        let _ = self.child.wait();
    }
}

fn nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// Limit work per pass so a noisy child cannot starve the deadline or stdin writer.
fn drain(
    stream: &mut impl Read,
    mut output: Option<&mut Vec<u8>>,
    size: &mut usize,
    limit: usize,
    name: &str,
) -> Result<bool> {
    let mut buffer = [0u8; 8192];
    for _ in 0..8 {
        match stream.read(&mut buffer) {
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

fn run_command_controlled(program: &Path, args: &[String], input: &str, timeout_secs: u64,
    cancel: Option<&std::sync::atomic::AtomicBool>, stop: Option<&Path>) -> Result<String> {
    ensure!(!stop.is_some_and(Path::exists), "improvement stopped by operator");
    ensure!(timeout_secs > 0, "subprocess timeout must be positive");
    ensure!(input.len() <= MAX_OUTPUT, "subprocess input exceeds 8 MiB");
    let started = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .context("starting configured subprocess")?;
    let mut group = ProcessGroup { child, terminated: false };
    let stdin = group.child.stdin.take().context("missing subprocess stdin")?;
    let mut stdout = group.child.stdout.take().context("missing subprocess stdout")?;
    let mut stderr = group.child.stderr.take().context("missing subprocess stderr")?;
    nonblocking(stdin.as_raw_fd()).context("setting stdin nonblocking")?;
    nonblocking(stdout.as_raw_fd()).context("setting stdout nonblocking")?;
    nonblocking(stderr.as_raw_fd()).context("setting stderr nonblocking")?;
    let mut sent = 0;
    let mut stdin = Some(stdin);
    let mut output = Vec::new();
    let mut out_size = 0;
    let mut err_size = 0;
    let mut out_done = false;
    let mut err_done = false;
    let mut status = None;
    loop {
        ensure!(!stop.is_some_and(Path::exists), "improvement stopped by operator");
        ensure!(!cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst)), "capability verification cancelled");
        ensure!(started.elapsed() < timeout, "subprocess timed out after {timeout_secs}s");
        if let Some(writer) = stdin.as_mut() {
            match writer.write(&input.as_bytes()[sent..]) {
                Ok(count) => sent += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::Interrupted => {},
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                    stdin = None;
                }
                Err(error) => return Err(error).context("writing subprocess input"),
            }
            if sent == input.len() {
                stdin = None;
            }
        }
        if !out_done {
            out_done = drain(&mut stdout, Some(&mut output), &mut out_size, MAX_OUTPUT, "subprocess stdout")?;
        }
        if !err_done {
            err_done = drain(&mut stderr, None, &mut err_size, MAX_DIAGNOSTICS, "subprocess stderr")?;
        }
        if status.is_none() && group.exited().context("observing subprocess exit")? {
            group.terminate();
            status = Some(group.child.wait().context("reaping subprocess")?);
        }
        if status.is_some() && out_done && err_done {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let status = status.context("subprocess has no exit status")?;
    ensure!(status.success(), "subprocess failed ({status}); {err_size} stderr bytes withheld to protect request data");
    ensure!(sent == input.len(), "subprocess exited before consuming its full request");
    String::from_utf8(output).context("subprocess stdout is not UTF-8")
}

pub fn call(config: &Config, request: &Value) -> Result<Value> {
    crate::setup::refresh_worker(config)?;
    ensure!(config.python.is_absolute() && config.worker.is_absolute(), "python and worker paths must be absolute");
    let input = serde_json::to_string(request).context("serializing worker request")?;
    let worker = config.worker.to_str().context("worker path is not UTF-8")?;
    let output = run_command_controlled(&config.python, &[worker.to_owned()], &input, config.worker_timeout_secs, None, Some(&config.state_dir.join("improvement.stop")))?;
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

pub fn capabilities(config: &Config) -> Result<Value> {
    let result = call(config, &json!({"op": "capabilities", "model": config.model}))?;
    validate_capabilities(result)
}

pub fn capabilities_controlled(config: &Config, cancel: &std::sync::atomic::AtomicBool) -> Result<Value> {
    crate::setup::refresh_worker(config)?;
    ensure!(config.python.is_absolute() && config.worker.is_absolute(), "python and worker paths must be absolute");
    let input = json!({"op":"capabilities", "model":config.model}).to_string();
    let output = run_command_controlled(&config.python, &[config.worker.to_str().context("worker path is not UTF-8")?.to_owned()], &input, config.worker_timeout_secs, Some(cancel), None)?;
    validate_capabilities(serde_json::from_str(&output).context("worker returned invalid JSON")?)
}

fn validate_capabilities(result: Value) -> Result<Value> {
    let parsed: Capabilities = serde_json::from_value(result.clone()).context("invalid capability response")?;
    ensure!(!parsed.reason.trim().is_empty(), "worker omitted capability reason");
    ensure!(!parsed.lora || parsed.inference, "LoRA capability requires inference support");
    if parsed.inference {
        ensure!(parsed.resolved_model.as_ref().is_some_and(|path| path.is_absolute() && path.is_dir()),
                "worker omitted resolved absolute model directory");
    }
    for feature in ["expert_growth", "routing_calibration", "attention_retrofit", "learned_step_probe"] {
        ensure!(parsed.unsupported.iter().any(|item| item == feature), "worker omitted unsupported structural capability");
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
    ensure!(temperature.is_finite() && (0.0..=2.0).contains(&temperature), "temperature must be in [0, 2]");
    ensure!((1..=32768).contains(&max_tokens), "max_tokens must be in [1, 32768]");
    let result = call(config, &json!({
        "op": "generate", "model": model, "adapter": adapter,
        "messages": messages, "temperature": temperature, "max_tokens": max_tokens, "seed": seed,
    }))?;
    let generation: Generation = serde_json::from_value(result).context("invalid generation response")?;
    Ok(generation.text)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Training {
    adapter: PathBuf,
}

pub fn train(config: &Config, active: &Version, output_dir: &Path) -> Result<PathBuf> {
    ensure!(config.training.enabled, "training is disabled");
    ensure!(output_dir.is_absolute(), "adapter output directory must be absolute");
    ensure!(!output_dir.exists(), "adapter output directory already exists");
    let training = &config.training;
    let response = call(config, &json!({
        "op": "train", "model": config.model, "resume_adapter": active.adapter,
        "train_data": training.train_data, "valid_data": training.valid_data,
        "output_dir": output_dir, "iters": training.iters, "batch_size": training.batch_size,
        "num_layers": training.num_layers, "rank": training.rank, "max_seq_length": training.max_seq_length,
    }))?;
    let trained: Training = serde_json::from_value(response).context("invalid training response")?;
    ensure!(trained.adapter.is_absolute(), "worker returned a relative adapter path");
    let expected = output_dir.canonicalize().context("missing trained adapter directory")?;
    ensure!(trained.adapter == expected, "worker returned a different adapter directory");
    for artifact in ["adapter_config.json", "adapters.safetensors"] {
        let metadata = std::fs::metadata(expected.join(artifact)).context("missing trained adapter artifact")?;
        ensure!(metadata.is_file() && metadata.len() > 0, "empty or invalid trained adapter artifact");
    }
    Ok(expected)
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
    ensure!((32..=8192).contains(&max_seq_length), "max_seq_length must be in [32, 8192]");
    ensure!(train_data.is_absolute(), "training data path must be absolute");
    let result = call(config, &json!({
        "op": "sensitivity", "model": model, "train_data": train_data,
        "rank": rank, "samples": samples, "max_seq_length": max_seq_length,
    }))?;
    let parsed: Sensitivity = serde_json::from_value(result).context("invalid sensitivity response")?;
    ensure!(!parsed.layers.is_empty(), "worker returned no layer scores");
    ensure!(parsed.layers.iter().all(|score| score.is_finite() && *score >= 0.0), "worker returned invalid layer scores");
    ensure!(parsed.loss.is_finite(), "worker returned a nonfinite loss");
    ensure!((1..=samples).contains(&parsed.samples), "worker reported an invalid sample count");
    Ok(parsed.layers)
}

/// mlx_lm trains the last N layers: the smallest N in `[min, max]` (clamped to the layer count)
/// whose last-N suffix holds at least `coverage` of the total score mass; `min` when no usable mass.
/// Nonempty models always select at least one layer: MLX interprets a zero suffix as all layers.
pub fn choose_layers(scores: &[f64], coverage: f64, min: u32, max: u32) -> u32 {
    let count = u32::try_from(scores.len()).unwrap_or(u32::MAX);
    let upper = max.max(1).min(count);
    let lower = min.max(1).min(upper);
    let usable = |score: &f64| if score.is_finite() && *score > 0.0 { *score } else { 0.0 };
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
    fn stop_request_terminates_running_training_subprocess() {
        let root = tempfile::tempdir().unwrap();
        let stop = root.path().join("improvement.stop");
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                std::fs::write(&stop, "").unwrap();
            });
            let started = Instant::now();
            let error = run_command_controlled(Path::new("/bin/sh"), &["-c".into(), "sleep 30".into()], "", 10, None, Some(&stop)).unwrap_err();
            assert!(error.to_string().contains("stopped by operator"));
            assert!(started.elapsed() < Duration::from_secs(3));
        });
    }

    #[test]
    fn lora_verification_cancellation_stops_the_worker_promptly() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| { std::thread::sleep(Duration::from_millis(40)); cancel.store(true, Ordering::SeqCst); });
            let error = run_command_controlled(Path::new("/bin/sh"), &["-c".into(), "sleep 30".into()], "", 10, Some(&cancel), None).unwrap_err();
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
        assert_eq!(validate_capabilities(caps.clone()).unwrap()["reason"], "fused QKV unsupported");
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
            assert!(chosen >= previous, "coverage {step}% chose {chosen} after {previous}");
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
        assert_eq!(choose_layers(&[f64::NAN, f64::INFINITY, -1.0], 0.9, 2, 8), 2);
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
        let data = std::env::temp_dir().join(format!("dial-sensitivity-{}.jsonl", std::process::id()));
        let mut lines = String::new();
        for n in 0..6 {
            let record = json!({"messages": [{"role": "user", "content": format!("What is {n} + 3?")},
                {"role": "assistant", "content": format!("The answer is {}.", n + 3)}],
                "source": "test", "verified": true});
            lines.push_str(&format!("{record}\n"));
        }
        std::fs::write(&data, lines).unwrap();
        let mut config = Config::default();
        config.python = python;
        config.worker = worker;
        config.worker_timeout_secs = 600;
        let result = sensitivity(&config, "mlx-community/Qwen2.5-0.5B-Instruct-4bit", &data, 8, 9, 512);
        let _ = std::fs::remove_file(&data);
        let scores = result.unwrap();
        assert_eq!(scores.len(), 24);
        assert!(scores.iter().all(|score| score.is_finite() && *score > 0.0), "{scores:?}");
        println!("{scores:?}");
        let chosen = choose_layers(&scores, 0.5, 2, 24);
        println!("chosen suffix: {chosen}");
        assert!((2..=24).contains(&chosen));
    }
}
