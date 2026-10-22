//! Apple's on-device foundation model (macOS FoundationModels) through Dial's own helper.
//!
//! `runtimes/apple/DialAppleFM.swift` is compiled by `build.rs` when the macOS SDK ships the
//! FoundationModels framework and is embedded in the executable. Each request runs one helper
//! process: Dial writes an OpenAI-shaped request to its stdin and reads JSON-line events from its
//! stdout. Cancellation and timeouts kill the helper's process group. Nothing listens on a port.
use super::{self as llm, Chat, Reply, ToolCall, errors::Classified, errors::ErrorClass};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

/// Provider id in configuration, role assignments and the model picker.
pub const PROVIDER: &str = "apple";
/// The single model the provider exposes: the system language model.
pub const MODEL: &str = "on-device";
pub const LABEL: &str = "Apple on-device model";
/// Wire version shared with `protocolVersion` in the Swift helper.
const PROTOCOL: u64 = 1;
/// Context size used when the helper does not report one (macOS 26 shipped 4096 tokens).
const FALLBACK_CONTEXT: u32 = 4096;
/// Silence limit applied when the configured worker timeout is 0 (unlimited).
const DEFAULT_IDLE_SECS: u64 = 120;
const AVAILABILITY_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);
const STDERR_KEEP_BYTES: usize = 4096;

#[cfg(dial_apple_fm)]
const HELPER: &[u8] = include_bytes!(env!("DIAL_APPLE_FM_HELPER"));
#[cfg(not(dial_apple_fm))]
const HELPER: &[u8] = &[];

/// What the system reports about the on-device model on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Availability {
    pub available: bool,
    /// Why the model cannot run, in user-facing words; `None` when it is available.
    pub reason: Option<String>,
    pub context_size: u32,
}

/// Whether this build carries the helper at all (macOS builds with the FoundationModels SDK).
pub fn supported_build() -> bool {
    !HELPER.is_empty()
}

/// The model's availability, probed once per process. The probe runs the helper, so callers on
/// hot paths should only reach it when the `apple` provider can matter.
pub fn availability() -> &'static Availability {
    static CACHE: OnceLock<Availability> = OnceLock::new();
    CACHE.get_or_init(|| {
        probe().unwrap_or_else(|error| Availability {
            available: false,
            reason: Some(format!("{error:#}")),
            context_size: FALLBACK_CONTEXT,
        })
    })
}

/// Shorthand for the role defaults: the model exists and can answer now.
pub fn available() -> bool {
    supported_build() && availability().available
}

/// The selection Dial uses when it routes a role to the on-device model.
pub fn selection() -> llm::model_config::ModelSelection {
    llm::model_config::ModelSelection::new(PROVIDER, MODEL)
}

/// Capabilities of the system model: text in, text and tool calls out, no reasoning channel.
pub fn metadata() -> llm::catalog::ModelMetadata {
    llm::catalog::ModelMetadata {
        vision: Some(false),
        tools: Some(true),
        reasoning: Some(false),
        kind: llm::catalog::ModelKind::Chat,
        ..Default::default()
    }
}

/// The provider row and its single model for `/models` and `/providers`. Builds without the
/// helper list nothing; an unavailable model stays visible with the system's reason.
pub fn discover() -> Option<(llm::catalog::ProviderStatus, Vec<llm::catalog::ModelEntry>)> {
    if !supported_build() {
        return None;
    }
    let availability = availability();
    let detail = match &availability.reason {
        None => format!(
            "Runs on this Mac through FoundationModels; {} token context, nothing leaves the device",
            availability.context_size
        ),
        Some(reason) => reason.clone(),
    };
    let status = llm::catalog::ProviderStatus {
        id: PROVIDER.into(),
        label: "Apple on-device".into(),
        base_url: None,
        state: if availability.available {
            "cached"
        } else {
            "unavailable"
        }
        .into(),
        detail: detail.clone(),
        models: 1,
    };
    let mut selection = selection();
    selection.context_window = Some(availability.context_size);
    let entry = llm::catalog::ModelEntry {
        selection,
        label: LABEL.into(),
        source: detail,
        available: availability.available,
        metadata: metadata(),
    };
    Some((status, vec![entry]))
}

fn probe() -> Result<Availability> {
    if !supported_build() {
        bail!("this Dial build does not include the Apple on-device model helper (macOS 26+ SDK)");
    }
    let helper = helper_path()?;
    let mut child = crate::platform::process::spawn_group(
        Command::new(helper)
            .arg("availability")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
    )
    .context("starting the Apple on-device model helper")?;
    let mut stdout = child.stdout.take().context("helper stdout")?;
    let mut process = super::server::Supervised::new(child);
    let exited = crate::platform::process::wait_exited(&process.child, AVAILABILITY_TIMEOUT)
        .unwrap_or(false);
    if !exited {
        process.stop();
        bail!("the Apple on-device model helper did not answer the availability probe");
    }
    let mut text = String::new();
    stdout.read_to_string(&mut text)?;
    process.stop();
    parse_availability(&text)
}

fn parse_availability(text: &str) -> Result<Availability> {
    let value: Value = serde_json::from_str(text.trim())
        .with_context(|| format!("unreadable availability reply: {}", text.trim()))?;
    if value["protocol"].as_u64() != Some(PROTOCOL) {
        bail!(
            "the Apple helper speaks protocol {}, Dial expects {PROTOCOL}",
            value["protocol"]
        );
    }
    let available = value["available"].as_bool() == Some(true);
    let reason = (!available).then(|| {
        match value["reason"].as_str().unwrap_or("unknown") {
            "device_not_eligible" => "this Mac does not support Apple Intelligence",
            "apple_intelligence_not_enabled" => {
                "Apple Intelligence is turned off in System Settings"
            }
            "model_not_ready" => "the on-device model is still downloading or preparing",
            _ => "the system reports the on-device model unavailable",
        }
        .to_owned()
    });
    let context_size = value["context_size"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
        .unwrap_or(FALLBACK_CONTEXT);
    Ok(Availability {
        available,
        reason,
        context_size,
    })
}

/// Writes the embedded helper once per process into an owner-only directory keyed by its digest,
/// reusing an identical copy left by an earlier run.
fn helper_path() -> Result<PathBuf> {
    static PATH: OnceLock<std::result::Result<PathBuf, String>> = OnceLock::new();
    PATH.get_or_init(|| extract().map_err(|error| format!("{error:#}")))
        .clone()
        .map_err(|error| anyhow!("{error}"))
}

#[cfg(unix)]
fn extract() -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let digest = Sha256::digest(HELPER);
    let tag: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    let uid = unsafe { libc::getuid() };
    let dir = std::env::temp_dir().join(format!("dial-apple-fm-{uid}-{tag}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("creating the helper directory"),
    }
    let meta = std::fs::symlink_metadata(&dir)?;
    anyhow::ensure!(
        meta.is_dir()
            && crate::platform::fs::path_owned_by_current_user(&dir)?
            && meta.permissions().mode() & 0o077 == 0,
        "refusing helper directory {} not owned privately by this user",
        dir.display()
    );
    let path = dir.join("dial-apple-fm");
    if std::fs::read(&path).is_ok_and(|bytes| bytes == HELPER) {
        return Ok(path);
    }
    let staging = dir.join(format!("dial-apple-fm.{}", std::process::id()));
    let _ = std::fs::remove_file(&staging);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&staging)
        .context("writing the helper")?;
    file.write_all(HELPER)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&staging, &path).context("installing the helper")?;
    Ok(path)
}

#[cfg(not(unix))]
fn extract() -> Result<PathBuf> {
    bail!("the Apple on-device model runs only on macOS")
}

/// The request body the helper reads: OpenAI Chat Completions shapes without Dial bookkeeping.
fn request_body(req: &Chat) -> Value {
    let messages: Vec<Value> = req
        .messages
        .iter()
        .map(|message| {
            let mut message = llm::messages::canonical(message);
            if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for call in calls {
                    let arguments = &mut call["function"]["arguments"];
                    if !arguments.is_string() {
                        *arguments = Value::String(arguments.to_string());
                    }
                }
            }
            message
        })
        .collect();
    let mut body = json!({
        "messages": messages,
        "temperature": req.temperature,
    });
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(req.tools.clone());
    }
    if req.max_tokens > 0 {
        body["max_tokens"] = json!(req.max_tokens);
    }
    body
}

/// Class and message for a helper `error` event.
fn helper_error(kind: &str, message: &str) -> anyhow::Error {
    let class = match kind {
        "context_exceeded" => ErrorClass::ContextOverflow,
        "rate_limited" | "busy" => ErrorClass::RateLimited { retry_after: None },
        "timeout" => ErrorClass::Transient,
        _ => ErrorClass::Fatal,
    };
    let what = match kind {
        "context_exceeded" => "request exceeds the on-device context window",
        "guardrail" => "blocked by Apple's safety guardrails",
        "refusal" => "the model refused the request",
        "unavailable" => "the on-device model is unavailable",
        "rate_limited" => "rate limited by the system",
        "busy" => "the system model is busy with another request",
        "unsupported_language" => "unsupported language or locale",
        "unsupported" => "the request uses something the on-device model cannot generate",
        "request" => "Dial sent a request the helper could not read",
        _ => "generation failed",
    };
    Classified::new(class, format!("{LABEL}: {what}: {message}")).into()
}

/// Fold helper events into a reply. Returns `Ok(true)` once the run finished.
struct Fold<'t, 'e> {
    reply: Reply,
    on_text: Option<&'t mut dyn FnMut(&str) -> bool>,
    on_event: &'e mut dyn FnMut(&llm::StreamEvent) -> bool,
    generating: bool,
}

impl Fold<'_, '_> {
    fn feed(&mut self, line: &str) -> Result<bool> {
        let event: Value = serde_json::from_str(line)
            .with_context(|| format!("unreadable helper event: {line}"))?;
        match event["type"].as_str() {
            Some("text") => {
                let delta = event["delta"].as_str().unwrap_or_default();
                if !self.generating {
                    self.generating = true;
                    if !(self.on_event)(&llm::StreamEvent::Progress(llm::ModelProgress::new(
                        llm::InferencePhase::Generating,
                    ))) {
                        return Err(llm::Interrupted.into());
                    }
                }
                self.reply.text.push_str(delta);
                if let Some(on_text) = self.on_text.as_mut()
                    && !delta.is_empty()
                    && !on_text(delta)
                {
                    return Err(llm::Interrupted.into());
                }
                Ok(false)
            }
            Some("tool_call") => {
                let raw = event["arguments"].as_str().unwrap_or("{}");
                let parsed = serde_json::from_str::<Value>(raw)
                    .ok()
                    .filter(Value::is_object);
                self.reply.tool_calls.push(ToolCall {
                    id: event["id"].as_str().unwrap_or_default().to_owned(),
                    name: event["name"].as_str().unwrap_or_default().to_owned(),
                    malformed_arguments: parsed.is_none(),
                    arguments: parsed.unwrap_or(Value::Null),
                });
                Ok(false)
            }
            Some("done") => {
                self.reply.finish_reason =
                    event["finish_reason"].as_str().unwrap_or("stop").to_owned();
                let usage = &event["usage"];
                self.reply.prompt_tokens = usage["prompt_tokens"].as_u64();
                self.reply.completion_tokens = usage["completion_tokens"].as_u64();
                self.reply.cached_prompt_tokens = usage["cached_prompt_tokens"].as_u64();
                super::usage::capture_reply(&self.reply);
                Ok(true)
            }
            Some("error") => Err(helper_error(
                event["kind"].as_str().unwrap_or("other"),
                event["message"].as_str().unwrap_or_default(),
            )),
            _ => bail!("unknown helper event: {line}"),
        }
    }
}

/// Run one chat request on the on-device model. `idle_secs` bounds silence between events.
pub(crate) fn chat(
    req: &Chat,
    idle_secs: u64,
    interrupt: &Arc<AtomicBool>,
    on_text: Option<&mut dyn FnMut(&str) -> bool>,
    on_event: &mut dyn FnMut(&llm::StreamEvent) -> bool,
) -> Result<Reply> {
    let availability = availability();
    if !availability.available {
        return Err(Classified::new(
            ErrorClass::Fatal,
            format!(
                "{LABEL} unavailable: {}",
                availability.reason.as_deref().unwrap_or("unknown reason")
            ),
        )
        .into());
    }
    if !on_event(&llm::StreamEvent::Progress(llm::ModelProgress::new(
        llm::InferencePhase::Processing,
    ))) {
        return Err(llm::Interrupted.into());
    }
    let body = serde_json::to_vec(&request_body(req))?;
    let mut child = crate::platform::process::spawn_group(
        Command::new(helper_path()?)
            .arg("chat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .context("starting the Apple on-device model helper")?;
    let stdin = child.stdin.take().context("helper stdin")?;
    let stdout = child.stdout.take().context("helper stdout")?;
    let stderr = child.stderr.take().context("helper stderr")?;
    let mut process = super::server::Supervised::new(child);

    let (sender, events) = mpsc::channel::<std::io::Result<String>>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if sender.send(line).is_err() {
                return;
            }
        }
    });
    let tail = Arc::new(Mutex::new(Vec::<u8>::new()));
    let reader_tail = Arc::clone(&tail);
    let stderr_reader = std::thread::spawn(move || {
        let mut stderr = stderr;
        let mut chunk = [0u8; 1024];
        while let Ok(n) = stderr.read(&mut chunk) {
            if n == 0 {
                break;
            }
            let mut kept = reader_tail.lock().unwrap_or_else(|p| p.into_inner());
            kept.extend_from_slice(&chunk[..n]);
            let excess = kept.len().saturating_sub(STDERR_KEEP_BYTES);
            kept.drain(..excess);
        }
    });
    // A request larger than the pipe buffer must not block event reading, so it is written on
    // its own thread; closing stdin marks the end of the request.
    super::usage::transport_attempt()?;
    let writer = std::thread::spawn(move || {
        let mut stdin = stdin;
        stdin.write_all(&body)
    });

    let mut fold = Fold {
        reply: Reply::default(),
        on_text,
        on_event,
        generating: false,
    };
    // 0 means the caller set no limit; the helper still needs a bound so a wedged model service
    // cannot hang the turn.
    let idle = Duration::from_secs(if idle_secs == 0 {
        DEFAULT_IDLE_SECS
    } else {
        idle_secs
    });
    let mut last_event = Instant::now();
    let outcome = loop {
        if interrupt.load(Ordering::SeqCst) {
            break Err(llm::Interrupted.into());
        }
        match events.recv_timeout(POLL) {
            Ok(Ok(line)) if line.trim().is_empty() => {}
            Ok(Ok(line)) => {
                last_event = Instant::now();
                match fold.feed(&line) {
                    Ok(true) => break Ok(()),
                    Ok(false) => {}
                    Err(error) => break Err(error),
                }
            }
            Ok(Err(error)) => break Err(anyhow!(error).context("reading the helper output")),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if last_event.elapsed() >= idle {
                    break Err(Classified::new(
                        ErrorClass::Transient,
                        format!("{LABEL}: no output for {}s", idle.as_secs()),
                    )
                    .into());
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Let stderr drain so the helper's last words reach the error.
                process.stop();
                let _ = stderr_reader.join();
                let said = String::from_utf8_lossy(&tail.lock().unwrap_or_else(|p| p.into_inner()))
                    .trim()
                    .to_owned();
                break Err(Classified::new(
                    ErrorClass::Fatal,
                    if said.is_empty() {
                        format!("{LABEL}: the helper exited without a result")
                    } else {
                        format!("{LABEL}: the helper exited without a result: {said}")
                    },
                )
                .into());
            }
        }
    };
    let reply = std::mem::take(&mut fold.reply);
    process.stop();
    let _ = writer.join();
    outcome.map(|()| reply)
}

#[cfg(test)]
mod tests;
