//! Operator-owned lifecycle hooks: programs run at fixed points of a session, plus the same events
//! delivered to runtime extension plugins (`crate::tools::integrations::plugin`).
//!
//! Hooks come from the user's configuration (`[[hooks]]`), which lives outside the project, so the
//! agent cannot add or edit them. Arrays from every configuration layer are appended, so a project
//! layer adds hooks without dropping global ones. A hook receives one JSON object on stdin and may
//! * exit 0: continue. Plain stdout is shown as a notice (on `post_tool_use` it is appended to the
//!   result). A JSON object on stdout is structured: `{"decision":"block","reason":..}` or
//!   `{"block":reason}` blocks, `{"input":{..}}` rewrites a `pre_tool_use` call's arguments,
//!   `{"content":..,"is_error":..}` replaces a `post_tool_use` result, `{"notice":..}` is shown;
//!   `{"additional_context":..}` (also `additionalContext`) queues a bounded harness note for the
//!   next model request on `pre_tool_use`, `post_tool_use`, `user_prompt_submit`,
//!   `assistant_message` and `context` (a block discards it);
//!   `{"payload":..}` rewrites a `before_provider_request` body and `{"messages":[..]}` replaces
//!   one request's message list on `context` (handlers run in extension order and each sees the
//!   earlier replacement). A `before_provider_request` or `context` block (`{"block":..}` or
//!   exit 2) refuses that model request with an error instead of sending it;
//! * exit 2: block (`pre_tool_use` refuses the call, `user_prompt_submit` rejects the message,
//!   `stop` makes the agent continue with the reason, `before_compact` skips compaction,
//!   `before_subagent_spawn` refuses the task, `session_before_branch` and
//!   `session_before_switch` refuse the rewind, fork or session change); stderr is the reason
//!   handed to the model or user. `assistant_message` and `after_provider_response` only observe;
//! * exit anything else, time out or fail to start: a notice and the action proceeds, unless the
//!   hook is fail-closed (`fail_closed`, default true for `pre_tool_use`), which blocks instead.
//!
//! Hooks are trusted programs and run unsandboxed in the project directory, like `[mcp.*]` servers.
use crate::config::hooks::canonical;

use crate::config::types::HookConfig;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    process::Command,
    sync::{
        LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Longest hook output kept (bytes).
const MAX_OUTPUT: usize = 8 * 1024;

/// Longest `additional_context` note kept per answer (bytes).
const MAX_CONTEXT: usize = 4 * 1024;

/// Events whose `additional_context` reaches the next request.
const CONTEXT_EVENTS: &[&str] = &[
    "pre_tool_use",
    "post_tool_use",
    "user_prompt_submit",
    "assistant_message",
    "context",
];

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Continue { output: String },
    Block { reason: String },
}

/// One hook's or plugin's structured answer.
#[derive(Debug, Default, PartialEq)]
pub struct Reply {
    pub block: Option<String>,
    pub output: String,
    pub input: Option<Value>,
    pub content: Option<String>,
    pub is_error: Option<bool>,
    /// Replacement request body for `before_provider_request` (`{"payload":..}`).
    pub payload: Option<Value>,
    /// Bounded harness note for the next request (`{"additional_context":..}`).
    pub additional_context: Option<String>,
    /// Replacement message list for a `context` hook (`{"messages":[..]}`), validated by the
    /// caller before use.
    pub messages: Option<Value>,
}

impl Reply {
    /// Interpret a JSON object answer. Accepts Dial's keys and Claude Code's hook output
    /// (`decision`, `reason`, `continue`, `stopReason`, `systemMessage`,
    /// `hookSpecificOutput.updatedInput`). Anything other than an object is not structured.
    pub fn from_json(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let text = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_owned);
        let mut reply = Reply::default();
        if object.get("decision").and_then(Value::as_str) == Some("block")
            || object.get("continue") == Some(&Value::Bool(false))
        {
            reply.block = Some(
                text("reason")
                    .or_else(|| text("stopReason"))
                    .unwrap_or_default(),
            );
        }
        match object.get("block") {
            Some(Value::String(reason)) => reply.block = Some(reason.clone()),
            Some(Value::Bool(true)) => {
                reply.block = Some(text("reason").unwrap_or_default());
            }
            _ => {}
        }
        reply.input = object
            .get("input")
            .or_else(|| {
                object
                    .get("hookSpecificOutput")
                    .and_then(Value::as_object)
                    .and_then(|specific| specific.get("updatedInput"))
            })
            .filter(|input| input.is_object())
            .cloned();
        reply.content = text("content");
        reply.is_error = object.get("is_error").and_then(Value::as_bool);
        reply.payload = object.get("payload").cloned();
        reply.messages = object.get("messages").cloned();
        reply.additional_context = ["additional_context", "additionalContext"]
            .into_iter()
            .find_map(text)
            .or_else(|| {
                object
                    .get("hookSpecificOutput")
                    .and_then(Value::as_object)
                    .and_then(|specific| specific.get("additionalContext"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        reply.output = ["notice", "message", "output", "systemMessage"]
            .into_iter()
            .find_map(text)
            .unwrap_or_default();
        Some(reply)
    }

    fn text(output: String) -> Self {
        Self {
            output,
            ..Self::default()
        }
    }
}

/// Everything one event produced, after every hook and plugin ran.
#[derive(Debug)]
pub struct Outcome {
    pub verdict: Verdict,
    pub notices: Vec<String>,
    /// Rewritten `pre_tool_use` arguments.
    pub input: Option<Value>,
    /// Replacement `post_tool_use` result text.
    pub content: Option<String>,
    pub is_error: Option<bool>,
    /// Rewritten `before_provider_request` body.
    pub request: Option<Value>,
    /// Distinct `additional_context` notes, in handler order, each already bounded.
    pub context: Vec<String>,
    /// Replacement `context` message list, not yet validated.
    pub messages: Option<Value>,
}

impl Outcome {
    /// The notes joined into one bounded harness note; `None` when no hook supplied any.
    pub fn context_note(&self) -> Option<String> {
        (!self.context.is_empty()).then(|| clip(self.context.join("\n")))
    }
}

/// Whether a failure of this hook (crash, timeout, start failure) blocks the action.
pub fn fail_closed(hook: &HookConfig) -> bool {
    hook.fail_closed
        .unwrap_or_else(|| canonical(&hook.event) == "pre_tool_use")
}

/// Matchers compile once per distinct pattern for the life of the process.
static MATCHERS: LazyLock<parking_lot::Mutex<HashMap<String, Option<regex::Regex>>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

fn matches(hook: &HookConfig, tool: Option<&str>) -> bool {
    match (&hook.matcher, tool) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(pattern), Some(tool)) => {
            let mut cache = MATCHERS.lock();
            cache
                .entry(pattern.clone())
                .or_insert_with(|| regex::Regex::new(&format!("^(?:{pattern})$")).ok())
                .as_ref()
                // An invalid matcher never matches; validation rejects it at load.
                .is_some_and(|re| re.is_match(tool))
        }
    }
}

fn clip(text: String) -> String {
    clip_to(text, MAX_OUTPUT)
}

fn clip_to(mut text: String, max: usize) -> String {
    if text.len() > max {
        let end = (0..=max)
            .rev()
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(0);
        text.truncate(end);
        text.push_str(" [cut]");
    }
    text.trim().to_owned()
}

/// A `context` hook's replacement message list: an array of role-bearing objects no larger than
/// `limit` serialized bytes.
fn valid_messages(messages: &Value, limit: usize) -> Result<(), String> {
    let list = messages.as_array().ok_or("messages must be an array")?;
    if !list.iter().all(|message| {
        message
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|role| !role.is_empty())
    }) {
        return Err("every message must be an object with a role".into());
    }
    let bytes = messages.to_string().len();
    if bytes > limit {
        return Err(format!("messages are {bytes} bytes; the limit is {limit}"));
    }
    Ok(())
}

fn run_one(
    hook: &HookConfig,
    root: &Path,
    payload: &Value,
    interrupt: &AtomicBool,
) -> Result<Reply, String> {
    if interrupt.load(Ordering::Acquire) {
        return Err("cancelled by the user before starting".into());
    }
    // The same deadline covers serialization, stdin delivery and both output streams.
    let started = Instant::now();
    let timeout = Duration::from_secs(hook.timeout_secs);
    let input = payload.to_string();
    let Some(program) = hook.command.first() else {
        return Err("hook has an empty command".into());
    };
    let mut command = Command::new(program);
    command
        .args(&hook.command[1..])
        .current_dir(root)
        .env("DIAL_HOOK_EVENT", &hook.event)
        .env("DIAL_PROJECT_DIR", root);
    let remaining = if timeout.is_zero() {
        timeout
    } else {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(format!(
                "timed out after {}s before starting",
                hook.timeout_secs
            ));
        }
        remaining
    };
    let mut policy = crate::runtime::process::Policy::bounded(remaining, MAX_OUTPUT * 4);
    policy.stderr_cap = MAX_OUTPUT * 4;
    policy.stderr_tail = false;
    policy.allow_unconsumed_stdin = true;
    let output = crate::runtime::process::run_with_policy(
        command,
        Some(input.into_bytes()),
        policy,
        interrupt,
        &mut |_, _| {},
    )
    .map_err(|error| format!("could not start {:?}: {error}", hook.command[0]))?;
    let mut failures = Vec::new();
    if output.cancelled {
        failures.push("cancelled by the user".to_owned());
    }
    if output.timed_out {
        failures.push(format!("timed out after {}s", hook.timeout_secs));
    }
    if let Some(error) = output.io_error {
        failures.push(error);
    }
    let out_overflowed = output.stdout_total > policy.stdout_cap;
    let out = output.stdout;
    let err = output.stderr;
    let status = output.status;
    let out_done = !output.output_incomplete;
    let err_done = out_done;
    if out_overflowed && out.trim_ascii_start().first() == Some(&b'{') {
        failures.push(format!("JSON hook output exceeds {} bytes", MAX_OUTPUT * 4));
    }
    let stdout = clip(String::from_utf8_lossy(&out).into_owned());
    let stderr = clip(String::from_utf8_lossy(&err).into_owned());
    let incomplete = if out_done && err_done {
        ""
    } else {
        "\n[hook output incomplete: inherited pipe remained open after 300ms cleanup grace]"
    };
    if let Some(status) = status
        && failures.is_empty()
    {
        match status.code() {
            Some(0) => {
                // A JSON object on a complete stdout is a structured answer.
                let structured = incomplete
                    .is_empty()
                    .then(|| serde_json::from_slice::<Value>(&out).ok())
                    .flatten()
                    .and_then(|value| Reply::from_json(&value));
                if let Some(reply) = structured {
                    return Ok(reply);
                }
                let mut output = stdout;
                output.push_str(incomplete);
                return Ok(Reply::text(output));
            }
            Some(2) => {
                let mut reason = if stderr.is_empty() { stdout } else { stderr };
                reason.push_str(incomplete);
                return Ok(Reply {
                    block: Some(reason),
                    ..Reply::default()
                });
            }
            other => failures.push(format!(
                "exited with {}",
                other.map_or("a signal".into(), |c| c.to_string()),
            )),
        }
    }
    let mut diagnostics = String::new();
    if !stdout.is_empty() {
        diagnostics.push_str(&format!("\nstdout: {stdout}"));
    }
    if !stderr.is_empty() {
        diagnostics.push_str(&format!("\nstderr: {stderr}"));
    }
    Err(format!(
        "{:?} {}{diagnostics}{incomplete}",
        hook.command[0],
        failures.join("; "),
    ))
}

/// Hooks are synchronous, but the controlling UI must keep pumping its existing interrupt flag.
/// Child agents already execute on a worker and do not need this UI rendezvous.
pub(in crate::agent) fn with_heartbeat<T: Send>(
    ui: &mut dyn crate::agent::Ui,
    run: impl FnOnce() -> T + Send,
) -> T {
    std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = scope.spawn(move || {
            let result = run();
            let _ = tx.send(());
            result
        });
        loop {
            match rx.recv_timeout(Duration::from_millis(80)) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => ui.heartbeat(),
            }
        }
        worker.join().expect("hook worker panicked")
    })
}

/// Run every hook for `event` whose matcher accepts `tool`, in configuration order. The first block
/// wins. Failures of fail-open hooks are returned as notices; a failing fail-closed hook blocks.
#[cfg(test)]
pub fn fire(
    hooks: &[HookConfig],
    root: &Path,
    event: &str,
    tool: Option<&str>,
    payload: Value,
    interrupt: &AtomicBool,
) -> (Verdict, Vec<String>) {
    let outcome = fire_with(hooks, None, root, event, tool, payload, interrupt);
    (outcome.verdict, outcome.notices)
}

/// Whether any hook or plugin observes `event`; callers skip building payloads otherwise.
pub fn wanted(
    hooks: &[HookConfig],
    plugins: Option<&crate::tools::integrations::plugin::Host>,
    event: &str,
) -> bool {
    let event = canonical(event);
    hooks.iter().any(|hook| canonical(&hook.event) == event)
        || plugins.is_some_and(|host| host.subscribed(event))
}

/// Whether a hook or plugin observes `event` for this specific tool. Shell hooks honour their
/// matcher; plugins subscribe per event, so any subscriber counts. Scheduling uses this so a hook
/// that matches only `bash` no longer serializes reads or disables batch reuse for other tools.
pub fn wanted_for_tool(
    hooks: &[HookConfig],
    plugins: Option<&crate::tools::integrations::plugin::Host>,
    event: &str,
    tool: &str,
) -> bool {
    let event = canonical(event);
    hooks
        .iter()
        .any(|hook| canonical(&hook.event) == event && matches(hook, Some(tool)))
        || plugins.is_some_and(|host| host.subscribed(event))
}

/// Run an observe-only lifecycle event (`session_start`, `session_end`). Failures and output come
/// back as notices; nothing blocks.
pub fn observe(
    hooks: &[HookConfig],
    plugins: Option<&crate::tools::integrations::plugin::Host>,
    root: &Path,
    event: &str,
    payload: Value,
    interrupt: &AtomicBool,
) -> Vec<String> {
    if !wanted(hooks, plugins, event) {
        return Vec::new();
    }
    fire_with(hooks, plugins, root, event, None, payload, interrupt).notices
}

/// Run a `before_*` lifecycle event that may refuse its action. `Err(reason)` refuses; notices and
/// output go to `notice`.
pub fn gate(
    hooks: &[HookConfig],
    plugins: Option<&crate::tools::integrations::plugin::Host>,
    root: &Path,
    event: &str,
    payload: Value,
    interrupt: &AtomicBool,
    notice: &mut dyn FnMut(&str),
) -> Result<(), String> {
    if !wanted(hooks, plugins, event) {
        return Ok(());
    }
    let outcome = fire_with(hooks, plugins, root, event, None, payload, interrupt);
    outcome.notices.iter().for_each(|n| notice(n));
    match outcome.verdict {
        Verdict::Block { reason } => Err(reason),
        Verdict::Continue { output } => {
            if !output.is_empty() {
                notice(&format!("hook: {output}"));
            }
            Ok(())
        }
    }
}

/// Largest replacement message list a `context` hook may return (serialized bytes).
const MAX_CONTEXT_MESSAGES: usize = 256 * 1024;

/// Run the `context` event over the request's message list. The result is a copy for this one
/// request: a hook's `{"messages":[..]}` replaces the list, and its `additional_context` is
/// appended as a transient user note. Persisted history is never touched. An invalid replacement
/// is reported and skipped; a block refuses the request.
fn context_request(
    hooks: &[HookConfig],
    plugins: Option<&crate::tools::integrations::plugin::Host>,
    root: &Path,
    request: &crate::llm::Chat,
    interrupt: &AtomicBool,
    notice: &mut dyn FnMut(&str),
) -> anyhow::Result<Option<crate::llm::Chat>> {
    const EVENT: &str = "context";
    if !wanted(hooks, plugins, EVENT) {
        return Ok(None);
    }
    let payload = json!({"messages": request.messages});
    let outcome = fire_with(hooks, plugins, root, EVENT, None, payload, interrupt);
    outcome.notices.iter().for_each(|n| notice(n));
    match &outcome.verdict {
        Verdict::Block { reason } => {
            anyhow::bail!("request refused by context hook: {reason}")
        }
        Verdict::Continue { output } => {
            if !output.is_empty() {
                notice(&format!("hook: {output}"));
            }
        }
    }
    let mut chat = request.clone();
    let mut changed = false;
    if let Some(messages) = &outcome.messages {
        match valid_messages(messages, MAX_CONTEXT_MESSAGES) {
            Ok(()) => {
                chat.messages = messages.as_array().cloned().unwrap_or_default();
                changed = true;
            }
            Err(why) => notice(&format!("context hook replacement ignored: {why}")),
        }
    }
    if let Some(note) = outcome.context_note() {
        chat.messages
            .push(json!({"role":"user","content":note,"dial_transient":true}));
        changed = true;
    }
    Ok(changed.then_some(chat))
}

/// Run `context` and then `before_provider_request` over a built request. `Ok(None)` means the
/// request is sent unchanged; `Ok(Some(body))` is the rewritten request. Hooks and plugins run in
/// order and each sees earlier rewrites; a block refuses the request with its reason.
pub fn provider_request(
    hooks: &[HookConfig],
    plugins: Option<&crate::tools::integrations::plugin::Host>,
    root: &Path,
    request: &crate::llm::Chat,
    interrupt: &AtomicBool,
    notice: &mut dyn FnMut(&str),
) -> anyhow::Result<Option<crate::llm::Chat>> {
    const EVENT: &str = "before_provider_request";
    let contextual = context_request(hooks, plugins, root, request, interrupt, notice)?;
    let request = contextual.as_ref().unwrap_or(request);
    if !wanted(hooks, plugins, EVENT) {
        return Ok(contextual);
    }
    let payload = json!({"request": request.to_payload()});
    let outcome = fire_with(hooks, plugins, root, EVENT, None, payload, interrupt);
    outcome.notices.iter().for_each(|n| notice(n));
    match outcome.verdict {
        Verdict::Block { reason } => {
            anyhow::bail!("request refused by before_provider_request hook: {reason}")
        }
        Verdict::Continue { output } => {
            if !output.is_empty() {
                notice(&format!("hook: {output}"));
            }
            match outcome.request {
                Some(body) => crate::llm::Chat::from_payload(body).map(Some),
                None => Ok(contextual),
            }
        }
    }
}

/// `after_provider_response`: observe one completed model response (model, stop reason, usage,
/// latency). Hooks cannot change or block the response; failures and output become notices.
pub fn provider_response(
    hooks: &[HookConfig],
    plugins: Option<&crate::tools::integrations::plugin::Host>,
    root: &Path,
    model: &str,
    reply: &crate::llm::Reply,
    latency: Duration,
    notice: &mut dyn FnMut(&str),
) {
    const EVENT: &str = "after_provider_response";
    if !wanted(hooks, plugins, EVENT) {
        return;
    }
    let payload = json!({
        "model": model,
        "stop_reason": reply.finish_reason,
        "latency_ms": latency.as_millis() as u64,
        "tool_calls": reply.tool_calls.len(),
        "usage": {
            "prompt_tokens": reply.prompt_tokens,
            "completion_tokens": reply.completion_tokens,
            "reasoning_tokens": reply.reasoning_tokens,
            "cache_read_tokens": reply.cached_prompt_tokens,
            "cache_write_tokens": reply.cache_creation_tokens,
        },
    });
    // A response was already received; an unrelated cancel flag must not skip observers.
    let interrupt = AtomicBool::new(false);
    observe(hooks, plugins, root, EVENT, payload, &interrupt)
        .iter()
        .for_each(|n| notice(n));
}

/// Fold one answer into the event state. Returns the block reason when the answer blocks.
fn apply(
    event: &str,
    reply: Reply,
    payload: &mut Value,
    outcome: &mut Outcome,
    output: &mut Vec<String>,
) -> Option<String> {
    if let Some(reason) = reply.block {
        return Some(reason);
    }
    if !reply.output.is_empty() {
        output.push(reply.output);
    }
    if CONTEXT_EVENTS.contains(&event)
        && let Some(note) = reply.additional_context
    {
        let note = clip_to(note, MAX_CONTEXT);
        // Repeats are compared ignoring surrounding whitespace, like the note itself.
        if !note.is_empty() && !outcome.context.contains(&note) {
            outcome.context.push(note);
        }
    }
    match event {
        "pre_tool_use" => {
            if let Some(input) = reply.input {
                // Later hooks and plugins see the rewritten call.
                payload["input"] = input.clone();
                outcome.input = Some(input);
            }
        }
        "post_tool_use" => {
            if let Some(content) = reply.content {
                payload["output"] = json!(clip(content.clone()));
                outcome.content = Some(content);
            }
            if let Some(is_error) = reply.is_error {
                payload["is_error"] = json!(is_error);
                outcome.is_error = Some(is_error);
            }
        }
        "before_provider_request" => {
            if let Some(request) = reply.payload {
                // Later hooks and plugins see the rewritten body.
                payload["request"] = request.clone();
                outcome.request = Some(request);
            }
        }
        "context" => {
            if let Some(messages) = reply.messages {
                // Later hooks and plugins see the replacement list.
                payload["messages"] = messages.clone();
                outcome.messages = Some(messages);
            }
        }
        _ => {}
    }
    None
}

/// Run the shell hooks and then the subscribed plugins for `event`. Hooks run in configuration
/// order and each sees earlier rewrites; plugins receive the resulting payload together.
pub fn fire_with(
    hooks: &[HookConfig],
    plugins: Option<&crate::tools::integrations::plugin::Host>,
    root: &Path,
    event: &str,
    tool: Option<&str>,
    payload: Value,
    interrupt: &AtomicBool,
) -> Outcome {
    let event = canonical(event);
    let mut payload = payload;
    payload["event"] = json!(event);
    payload["cwd"] = json!(root);
    let mut outcome = Outcome {
        verdict: Verdict::Continue {
            output: String::new(),
        },
        notices: Vec::new(),
        input: None,
        content: None,
        is_error: None,
        request: None,
        context: Vec::new(),
        messages: None,
    };
    let mut output = Vec::new();
    let block = |outcome: &mut Outcome, reason: String, source: &str| {
        let reason = if reason.trim().is_empty() {
            format!("blocked by a {event} {source}")
        } else {
            reason
        };
        outcome.verdict = Verdict::Block { reason };
        // A refusal discards everything the answers before it asked for.
        outcome.context.clear();
    };
    for hook in hooks
        .iter()
        .filter(|h| canonical(&h.event) == event && matches(h, tool))
    {
        match run_one(hook, root, &payload, interrupt) {
            Ok(reply) => {
                if let Some(reason) = apply(event, reply, &mut payload, &mut outcome, &mut output) {
                    block(&mut outcome, reason, "hook");
                    return outcome;
                }
            }
            Err(why) if fail_closed(hook) => {
                block(
                    &mut outcome,
                    format!("{event} hook failed and is fail-closed: {why}"),
                    "hook",
                );
                return outcome;
            }
            Err(why) => outcome.notices.push(format!("{event} hook failed: {why}")),
        }
        if interrupt.load(Ordering::Acquire) {
            break;
        }
    }
    if let Some(host) = plugins
        && host.subscribed(event)
        && !interrupt.load(Ordering::Acquire)
    {
        for answer in host.dispatch(event, &payload, interrupt) {
            match answer.result {
                Ok(value) => {
                    let Some(reply) = Reply::from_json(&value) else {
                        continue;
                    };
                    if let Some(reason) =
                        apply(event, reply, &mut payload, &mut outcome, &mut output)
                    {
                        outcome.notices.extend(host.take_notices());
                        block(
                            &mut outcome,
                            reason,
                            &format!("extension {}", answer.plugin),
                        );
                        return outcome;
                    }
                }
                Err(why) => outcome
                    .notices
                    .push(format!("{event} extension {} failed: {why}", answer.plugin)),
            }
        }
        outcome.notices.extend(host.take_notices());
    }
    outcome.verdict = Verdict::Continue {
        output: output.join("\n"),
    };
    outcome
}

/// `pre_tool_use`: `Err(reason)` refuses the call; `Ok(Some(args))` replaces its arguments (the
/// caller must execute the call with them, still through schemas and approvals). Notices (failed
/// fail-open hooks) are passed to `notice`.
#[cfg(test)]
pub fn pre_tool(
    hooks: &[HookConfig],
    root: &Path,
    name: &str,
    args: &Value,
    interrupt: &AtomicBool,
    notice: &mut dyn FnMut(&str),
) -> Result<Option<Value>, String> {
    pre_tool_with(hooks, None, root, name, args, interrupt, notice)
}

pub fn pre_tool_with(
    hooks: &[HookConfig],
    plugins: Option<&crate::tools::integrations::plugin::Host>,
    root: &Path,
    name: &str,
    args: &Value,
    interrupt: &AtomicBool,
    notice: &mut dyn FnMut(&str),
) -> Result<Option<Value>, String> {
    pre_tool_noted(hooks, plugins, root, name, args, interrupt, notice).0
}

/// `pre_tool_with` that also returns the bounded `additional_context` note the hooks queued for
/// the next request (`None` when refused or when no hook supplied one).
pub fn pre_tool_noted(
    hooks: &[HookConfig],
    plugins: Option<&crate::tools::integrations::plugin::Host>,
    root: &Path,
    name: &str,
    args: &Value,
    interrupt: &AtomicBool,
    notice: &mut dyn FnMut(&str),
) -> (Result<Option<Value>, String>, Option<String>) {
    if !wanted(hooks, plugins, "pre_tool_use") {
        return (Ok(None), None);
    }
    let outcome = fire_with(
        hooks,
        plugins,
        root,
        "pre_tool_use",
        Some(name),
        json!({"tool":name,"input":args}),
        interrupt,
    );
    outcome.notices.iter().for_each(|n| notice(n));
    let context = outcome.context_note();
    match outcome.verdict {
        Verdict::Block { reason } => (
            Err(format!("blocked by a pre_tool_use hook: {reason}")),
            None,
        ),
        Verdict::Continue { output } => {
            if !output.is_empty() {
                notice(&format!("hook: {output}"));
            }
            (Ok(outcome.input.filter(|input| input != args)), context)
        }
    }
}

/// `post_tool_use`: hook output is appended to the tool result so the model sees it (a formatter's
/// diagnostics, say). A structured `content` replaces the result text; a block (exit 2) marks the
/// result as an error with the reason. The return value is the bounded `additional_context` note
/// for the next request, delivered outside the tool result.
#[cfg(test)]
pub fn post_tool(
    hooks: &[HookConfig],
    root: &Path,
    name: &str,
    args: &Value,
    out: &mut crate::runtime::tool_output::ToolOutput,
    interrupt: &AtomicBool,
    notice: &mut dyn FnMut(&str),
) {
    let _ = post_tool_with(hooks, None, root, name, args, out, interrupt, notice);
}

#[allow(clippy::too_many_arguments)]
pub fn post_tool_with(
    hooks: &[HookConfig],
    plugins: Option<&crate::tools::integrations::plugin::Host>,
    root: &Path,
    name: &str,
    args: &Value,
    out: &mut crate::runtime::tool_output::ToolOutput,
    interrupt: &AtomicBool,
    notice: &mut dyn FnMut(&str),
) -> Option<String> {
    if !wanted(hooks, plugins, "post_tool_use") {
        return None;
    }
    let outcome = fire_with(
        hooks,
        plugins,
        root,
        "post_tool_use",
        Some(name),
        json!({"tool":name,"input":args,"is_error":out.is_error,"output":clip(out.text.clone())}),
        interrupt,
    );
    outcome.notices.iter().for_each(|n| notice(n));
    let context = outcome.context_note();
    if let Some(content) = outcome.content {
        out.text = content;
        // Structured data described the replaced text; keep the model and clients consistent.
        out.data = None;
    }
    if let Some(is_error) = outcome.is_error {
        // A presentation hook may add a failure, but cannot clear an execution failure or uncertainty.
        out.is_error |= is_error;
    }
    let (text, is_error) = match outcome.verdict {
        Verdict::Continue { output } => (output, false),
        Verdict::Block { reason } => (reason, true),
    };
    if !text.is_empty() {
        out.text.push_str(&format!("\n[post_tool_use hook] {text}"));
        out.is_error |= is_error;
    }
    context
}

#[cfg(test)]
mod tests;
