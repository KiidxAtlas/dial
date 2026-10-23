//! Benchmark child protocol: every turn stays in one process/session, with bounded event capture.
use anyhow::{Context, Result, ensure};
use dial::benchmark::{
    AgentFailure, BenchmarkCase, RunMetrics, ToolTrace, failure_detail, response_sha256,
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = dial::platform::process::kill_group(self.0.id());
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;

// Drain stderr without blocking the child or retaining unbounded provider diagnostics.
fn capture_diagnostics(mut stream: impl Read, captured: &Mutex<Vec<u8>>) {
    let mut buffer = [0; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let mut bytes = captured.lock().expect("diagnostic lock poisoned");
                let keep = count.min(MAX_DIAGNOSTIC_BYTES.saturating_sub(bytes.len()));
                bytes.extend_from_slice(&buffer[..keep]);
            }
        }
    }
}
fn receive(rx: &mpsc::Receiver<Result<Value>>, deadline: Option<Instant>) -> Result<Value> {
    match deadline {
        Some(deadline) => match rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(anyhow::anyhow!("benchmark RPC timed out")),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(anyhow::anyhow!("benchmark RPC closed"))
            }
        },
        None => rx.recv().context("benchmark RPC closed")?,
    }
}
fn request(
    input: &mut impl Write,
    rx: &mpsc::Receiver<Result<Value>>,
    id: u64,
    method: &str,
    params: Value,
    deadline: Option<Instant>,
    metrics: &mut RunMetrics,
) -> Result<Value> {
    serde_json::to_writer(
        &mut *input,
        &json!({"id":id,"method":method,"params":params}),
    )?;
    input.write_all(b"\n")?;
    input.flush()?;
    loop {
        let event = receive(rx, deadline)
            .with_context(|| format!("waiting for benchmark RPC {method} (request {id})"))?;
        observe(metrics, &event);
        if event["id"] == id {
            ensure!(
                event.get("error").is_none(),
                "benchmark RPC failed: {}",
                event["error"]
            );
            return Ok(event["result"].clone());
        }
    }
}
/// The child's snapshots are cumulative, so the last one covers the case. It is complete only when
/// the run succeeded and every send reported a snapshot; with no snapshot the trace stays unknown.
fn tool_trace_for_run(
    snapshot: Option<&Value>,
    sends: u64,
    snapshots: u64,
    succeeded: bool,
) -> Option<ToolTrace> {
    let complete = succeeded && sends > 0 && snapshots == sends;
    ToolTrace::from_snapshot(snapshot?, complete)
}

/// Journey fixtures require an actual history rewrite, not a successful no-op maintenance call.
fn require_compaction(result: &Value) -> Result<()> {
    ensure!(
        result["compacted"] == true,
        "benchmark requested compaction but history was not compacted: {}",
        result["outcome"]
    );
    Ok(())
}

/// Recovery state may describe the previous turn, so retain it separately from send reports.
fn retain_efficiency(metrics: &mut RunMetrics, state: &Value, send: Option<u64>) {
    let Some(report) = state.get("efficiency").filter(|value| !value.is_null()) else {
        return;
    };
    let observations = metrics
        .efficiency
        .get_or_insert_with(|| json!({"turns":[]}));
    if let Some(send) = send {
        observations["turns"]
            .as_array_mut()
            .expect("turn observations")
            .push(json!({"send":send,"report":report}));
    } else {
        observations["recovery_observation"] = report.clone();
    }
}

fn observe(m: &mut RunMetrics, event: &Value) {
    match event["event"].as_str() {
        Some("usage") => {
            if let (Some(prompt), Some(output)) = (
                event["prompt_tokens"].as_u64(),
                event["output_tokens"].as_u64(),
            ) {
                m.token_usage_observed = true;
                m.prompt_tokens = m.prompt_tokens.saturating_add(prompt);
                m.output_tokens = m.output_tokens.saturating_add(output);
            } else {
                m.token_usage_incomplete = true;
            }
        }
        Some("cached_usage") => {
            m.cached_tokens += event["read_tokens"].as_u64().unwrap_or(0);
        }
        Some("reasoning_usage") => m.reasoning_tokens += event["tokens"].as_u64().unwrap_or(0),
        Some("tool_result") => {
            m.tool_calls += 1;
            if event["is_error"] == true {
                m.tool_errors += 1;
            }
            match event["name"].as_str() {
                Some("artifact_read") | Some("artifact_search") => m.artifacts_retrieved += 1,
                Some("tool_load") => m.dynamic_tools_used += 1,
                _ => {}
            }
            if event["text"]
                .as_str()
                .is_some_and(|t| t.starts_with("invalid tool call"))
            {
                m.tool_contract_errors += 1;
            }
        }
        Some("notice")
            if event["message"]
                .as_str()
                .is_some_and(|t| t.contains("output limit") && t.contains("retry")) =>
        {
            m.truncated_recoveries += 1;
        }
        _ => {}
    }
}

/// Runtime-wide usage from the child's `state` snapshot (`usage.total.all`): the main agent,
/// workers and every purpose. Counters are read by name, so additive snapshot fields are ignored.
struct Ledger {
    input: u64,
    output: u64,
    cache_read: u64,
    reasoning: u64,
    requests: u64,
    reported: u64,
    /// No unreported attempts, and every successful request carried usage.
    complete: bool,
    cost: Option<f64>,
    cost_complete: bool,
    groups: Value,
}

fn read_ledger(state: &Value) -> Option<Ledger> {
    let all = state.pointer("/usage/total/all")?;
    let count = |name: &str| all.get(name).and_then(Value::as_u64);
    let successful = count("successful_requests")?;
    let settled = successful.checked_add(count("failed_requests")?)?;
    let reported = count("reported_responses")?;
    let unreported = count("unreported_attempts")?;
    Some(Ledger {
        input: count("input")?,
        output: count("output")?,
        cache_read: count("cache_read")?,
        reasoning: count("reasoning")?,
        requests: count("requests")?,
        reported,
        complete: unreported == 0 && reported >= successful && settled == count("requests")?,
        cost: all.get("observed_cost_usd").and_then(Value::as_f64),
        cost_complete: all.get("cost_coverage").and_then(Value::as_str) == Some("observed_priced"),
        groups: state.pointer("/usage/total")?.clone(),
    })
}

/// Replaces the per-case counters with the runtime ledger, so main-agent usage events are never
/// added to it. A missing ledger leaves coverage unknown instead of keeping a main-only lower bound.
fn apply_ledger(metrics: &mut RunMetrics, ledger: Option<Ledger>) {
    let Some(ledger) = ledger else {
        metrics.token_usage_incomplete = true;
        return;
    };
    metrics.prompt_tokens = ledger.input;
    metrics.output_tokens = ledger.output;
    metrics.cached_tokens = ledger.cache_read;
    metrics.reasoning_tokens = ledger.reasoning;
    metrics.requests = ledger.requests;
    metrics.runtime_usage = Some(ledger.groups);
    metrics.observed_cost_usd = ledger.cost;
    metrics.observed_cost_complete = ledger.cost_complete;
    metrics.token_usage_observed = ledger.reported > 0;
    metrics.token_usage_incomplete = !ledger.complete;
}

fn configure_subscription_auth(command: &mut Command, auth_home: Option<&Path>) -> Result<()> {
    // An ambient override must never opt a benchmark into shared credentials implicitly.
    let key = dial::config::paths::SUBSCRIPTION_AUTH_HOME;
    command.env_remove(key);
    if let Some(home) = auth_home {
        let home = std::fs::canonicalize(home)
            .context("opening benchmark subscription authentication store")?;
        command.env(key, home);
    }
    Ok(())
}

pub fn run(
    executable: &Path,
    user_home: &Path,
    case: &BenchmarkCase,
    workspace: &Path,
    auth_home: Option<&Path>,
) -> Result<RunMetrics> {
    run_configured(executable, user_home, case, workspace, 600, None, auth_home)
}

/// Runs one case in a persistent child. A failed run returns [`AgentFailure`], carrying the
/// metrics observed before the failure and a redacted, bounded reason with child diagnostics.
pub fn run_configured(
    executable: &Path,
    user_home: &Path,
    case: &BenchmarkCase,
    workspace: &Path,
    timeout_secs: u64,
    adapter: Option<&Path>,
    auth_home: Option<&Path>,
) -> Result<RunMetrics> {
    ensure!(
        timeout_secs <= 600,
        "benchmark timeout must be 0 (unlimited) or 1..=600 seconds"
    );
    let started = Instant::now();
    let deadline = (timeout_secs != 0).then(|| started + Duration::from_secs(timeout_secs));
    let mut command = Command::new(executable);
    command
        .args(["rpc", "--yes", "--sandbox", "dev"])
        .arg("--dir")
        .arg(workspace)
        .env("DIAL_HOME", user_home)
        .env("HF_HUB_OFFLINE", "1")
        .env("TRANSFORMERS_OFFLINE", "1")
        // Integration benchmarks must not attach to an ambient operator browser via env overrides.
        .env("DIAL_BROWSER_CMUX", "0")
        .env("DIAL_BROWSER_RELAY", "0")
        .env_remove("DIAL_BENCHMARK_ADAPTER")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_subscription_auth(&mut command, auth_home)?;
    if let Some(adapter) = adapter {
        command.env("DIAL_BENCHMARK_ADAPTER", adapter);
    }
    let mut child = ChildGuard(
        dial::platform::process::spawn_group(&mut command)
            .context("starting persistent benchmark child")?,
    );
    let mut input = child
        .0
        .stdin
        .take()
        .context("benchmark stdin unavailable")?;
    let stdout = child
        .0
        .stdout
        .take()
        .context("benchmark stdout unavailable")?;
    let stderr = child
        .0
        .stderr
        .take()
        .context("benchmark stderr unavailable")?;
    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    let captured = diagnostics.clone();
    let diagnostic_reader = thread::spawn(move || capture_diagnostics(stderr, &captured));
    let (tx, rx) = mpsc::sync_channel(16);
    let reader = thread::spawn(move || {
        let mut captured = 0usize;
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    captured += line.len();
                    let result = if captured > 32 * 1024 * 1024 {
                        Err(anyhow::anyhow!("benchmark RPC capture exceeds 32 MiB"))
                    } else {
                        serde_json::from_str(&line).context("invalid benchmark RPC event")
                    };
                    let stop = result.is_err();
                    if tx.send(result).is_err() || stop {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e.into()));
                    break;
                }
            }
        }
    });
    let mut metrics = RunMetrics::default();
    let mut ledger_read = false;
    let mut trace_snapshot: Option<Value> = None;
    let (mut trace_sends, mut trace_snapshots) = (0u64, 0u64);
    let result = (|| -> Result<()> {
        loop {
            let event = receive(&rx, deadline)?;
            ensure!(
                event["event"] != "fatal",
                "benchmark initialization failed: {}",
                event["message"]
            );
            if event["event"] == "ready" {
                break;
            }
        }
        let mut id = 1;
        if !case.context_messages.is_empty() {
            request(
                &mut input,
                &rx,
                id,
                "seed_context",
                json!({"messages":case.context_messages}),
                deadline,
                &mut metrics,
            )?;
            id += 1;
        }
        for (prompt, compact) in std::iter::once((case.prompt.as_str(), false)).chain(
            case.turns
                .iter()
                .map(|t| (t.prompt.as_str(), t.compact_before)),
        ) {
            if compact {
                let compacted = request(
                    &mut input,
                    &rx,
                    id,
                    "compact",
                    json!({"keep_recent_tokens":0}),
                    deadline,
                    &mut metrics,
                )?;
                require_compaction(&compacted)?;
                id += 1;
            }
            trace_sends += 1;
            let report = request(
                &mut input,
                &rx,
                id,
                "send",
                json!({"text":prompt}),
                deadline,
                &mut metrics,
            )?;
            retain_efficiency(&mut metrics, &report, Some(trace_sends));
            id += 1;
            ensure!(
                report["interrupted"] != true
                    && report["text"]
                        .as_str()
                        .is_some_and(|t| !t.trim().is_empty()),
                "benchmark turn interrupted or empty"
            );
            let completed = report
                .pointer("/metrics/turns_completed")
                .and_then(Value::as_u64)
                .context("benchmark completion counter unavailable")?;
            ensure!(
                completed == metrics.turns_completed + 1,
                "agent stopped without completing the benchmark turn: {}",
                report["text"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(512)
                    .collect::<String>()
            );
            metrics.turns_completed = completed;
            metrics.response_sha256 = report["text"].as_str().map(response_sha256);
            if let Some(speculation) = report.get("speculation") {
                for (name, target) in [
                    ("started", &mut metrics.speculative_preparations),
                    ("reused", &mut metrics.speculative_reuses),
                    ("discarded", &mut metrics.speculative_discards),
                    ("preparation_ms", &mut metrics.speculative_preparation_ms),
                ] {
                    if let Some(value) = speculation[name].as_u64() {
                        *target = value;
                    }
                }
            }
            if let Some(v) = report.get("metrics") {
                for (name, target) in [
                    ("tool_contract_errors", &mut metrics.tool_contract_errors),
                    ("compactions", &mut metrics.compactions),
                    ("artifacts_retrieved", &mut metrics.artifacts_retrieved),
                    ("dynamic_tools_used", &mut metrics.dynamic_tools_used),
                    ("truncated_recoveries", &mut metrics.truncated_recoveries),
                    ("reasoning_tokens", &mut metrics.reasoning_tokens),
                    ("cached_tokens", &mut metrics.cached_tokens),
                ] {
                    if let Some(n) = v[name].as_u64() {
                        *target = n;
                    }
                }
            }
            if let Some(n) = report
                .pointer("/metrics/repair_attempts")
                .and_then(Value::as_u64)
            {
                metrics.repair_attempts = Some(n);
            }
            if let Some(snapshot) = report.pointer("/metrics/tool_trace") {
                trace_snapshots += 1;
                trace_snapshot = Some(snapshot.clone());
            }
        }
        // Runtime-wide ledger (every purpose and worker), read once after the last turn. It
        // replaces the main-agent usage events above, so the two are never summed.
        let state = request(
            &mut input,
            &rx,
            id,
            "state",
            json!({}),
            deadline,
            &mut metrics,
        )
        .context("benchmark runtime usage snapshot unavailable")?;
        ledger_read = true;
        apply_ledger(&mut metrics, read_ledger(&state));
        id += 1;
        request(
            &mut input,
            &rx,
            id,
            "shutdown",
            json!({}),
            deadline,
            &mut metrics,
        )?;
        Ok(())
    })();
    // A turn can fail after paying for inference. When the child still speaks RPC, retain its
    // settled ledger rather than throwing away worker/auxiliary usage on the error path.
    // A dead or busy child gets only one bounded recovery read, never another model request.
    let active_before_snapshot = trace_sends > 0
        || metrics.token_usage_observed
        || metrics.tool_calls > 0
        || metrics.turns_completed > 0
        || metrics.compactions > 0;
    if result.is_err()
        && !ledger_read
        && active_before_snapshot
        && let Ok(state) = request(
            &mut input,
            &rx,
            u64::MAX,
            "state",
            json!({}),
            Some(Instant::now() + Duration::from_secs(2)),
            &mut metrics,
        )
    {
        ledger_read = true;
        apply_ledger(&mut metrics, read_ledger(&state));
        retain_efficiency(&mut metrics, &state, None);
    }
    metrics.elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    metrics.agent_succeeded = result.is_ok();
    metrics.agent_error = result.is_err();
    // Snapshots are cumulative for the child runtime; the last one describes the whole case.
    metrics.tool_trace = tool_trace_for_run(
        trace_snapshot.as_ref(),
        trace_sends,
        trace_snapshots,
        result.is_ok(),
    );
    // A failure before the ledger read leaves runtime usage unknown once the model was active.
    let active = metrics.token_usage_observed
        || metrics.tool_calls > 0
        || metrics.turns_completed > 0
        || metrics.compactions > 0;
    if result.is_err() && !ledger_read && active {
        metrics.token_usage_incomplete = true;
    }
    drop(input);
    drop(child);
    drop(rx);
    // Not joined: a grandchild that escaped the process group can keep stdout open after the
    // child is killed, leaving the reader blocked in read_line. The thread exits on its own once
    // the pipe closes or its send fails (rx is dropped above), and it owns nothing else.
    drop(reader);
    drop(diagnostic_reader);
    if let Err(e) = result {
        let bytes = diagnostics.lock().expect("diagnostic lock poisoned");
        let detail = failure_detail(&format!("{e:#}"), &String::from_utf8_lossy(&bytes));
        eprintln!("benchmark case {}: {detail}", case.id);
        return Err(AgentFailure { metrics, detail }.into());
    }
    Ok(metrics)
}

#[cfg(test)]
mod tests;
