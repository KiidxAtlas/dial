//! Operator-owned lifecycle hooks: programs run at fixed points of a session, plus the same events
//! delivered to runtime extension plugins (`tools::plugin`).
//!
//! Hooks come from the user's configuration (`[[hooks]]`), which lives outside the project, so the
//! agent cannot add or edit them. Arrays from every configuration layer are appended, so a project
//! layer adds hooks without dropping global ones. A hook receives one JSON object on stdin and may
//! * exit 0: continue. Plain stdout is shown as a notice (on `post_tool_use` it is appended to the
//!   result). A JSON object on stdout is structured: `{"decision":"block","reason":..}` or
//!   `{"block":reason}` blocks, `{"input":{..}}` rewrites a `pre_tool_use` call's arguments,
//!   `{"content":..,"is_error":..}` replaces a `post_tool_use` result, `{"notice":..}` is shown;
//!   `{"payload":..}` rewrites a `before_provider_request` body (handlers run in
//!   extension order and each sees the earlier replacement). A
//!   `before_provider_request` block (`{"block":..}` or exit 2) refuses that model request with an
//!   error instead of sending it;
//! * exit 2: block (`pre_tool_use` refuses the call, `user_prompt_submit` rejects the message,
//!   `stop` makes the agent continue with the reason, `before_compact` skips compaction,
//!   `before_subagent_spawn` refuses the task); stderr is the reason handed to the model or user;
//! * exit anything else, time out or fail to start: a notice and the action proceeds, unless the
//!   hook is fail-closed (`fail_closed`, default true for `pre_tool_use`), which blocks instead.
//!
//! Hooks are trusted programs and run unsandboxed in the project directory, like `[mcp.*]` servers.
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

/// Canonical event names. Alias names (`tool_call`, `tool_result`, `input`,
/// `session_shutdown`, `session_before_compact`, `session_stop`) are accepted as well.
pub const EVENTS: &[&str] = &[
    "session_start",
    "session_end",
    "user_prompt_submit",
    "pre_tool_use",
    "post_tool_use",
    "stop",
    "before_compact",
    "before_subagent_spawn",
    "before_provider_request",
];

/// The Dial name for an event or one of its aliases.
pub fn canonical(event: &str) -> &str {
    crate::tools::plugin::canonical_event(event)
}

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
}

pub fn validate(hook: &HookConfig) -> Result<(), String> {
    if !EVENTS.contains(&canonical(&hook.event)) {
        return Err(format!(
            "unknown hook event {:?}; use one of {}",
            hook.event,
            EVENTS.join(", ")
        ));
    }
    if hook
        .command
        .first()
        .is_none_or(|program| program.trim().is_empty())
    {
        return Err("hook command must name a program".into());
    }
    if hook.timeout_secs > 600 {
        return Err("hook timeout_secs must be 0..=600".into());
    }
    if let Some(matcher) = &hook.matcher {
        regex::Regex::new(&format!("^(?:{matcher})$"))
            .map_err(|e| format!("invalid hook matcher: {e}"))?;
    }
    Ok(())
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

fn clip(mut text: String) -> String {
    if text.len() > MAX_OUTPUT {
        let end = (0..=MAX_OUTPUT)
            .rev()
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(0);
        text.truncate(end);
        text.push_str(" [cut]");
    }
    text.trim().to_owned()
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
pub(super) fn with_heartbeat<T: Send>(ui: &mut dyn super::Ui, run: impl FnOnce() -> T + Send) -> T {
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
    plugins: Option<&crate::tools::plugin::Host>,
    event: &str,
) -> bool {
    let event = canonical(event);
    hooks.iter().any(|hook| canonical(&hook.event) == event)
        || plugins.is_some_and(|host| host.subscribed(event))
}

/// Run an observe-only lifecycle event (`session_start`, `session_end`). Failures and output come
/// back as notices; nothing blocks.
pub fn observe(
    hooks: &[HookConfig],
    plugins: Option<&crate::tools::plugin::Host>,
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
    plugins: Option<&crate::tools::plugin::Host>,
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

/// Run `before_provider_request` over a built request. `Ok(None)` means the request is sent
/// unchanged; `Ok(Some(body))` is the rewritten request. Hooks and plugins run in order and each
/// sees earlier rewrites; a block refuses the request with its reason.
pub fn provider_request(
    hooks: &[HookConfig],
    plugins: Option<&crate::tools::plugin::Host>,
    root: &Path,
    request: &crate::llm::Chat,
    interrupt: &AtomicBool,
    notice: &mut dyn FnMut(&str),
) -> anyhow::Result<Option<crate::llm::Chat>> {
    const EVENT: &str = "before_provider_request";
    if !wanted(hooks, plugins, EVENT) {
        return Ok(None);
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
            outcome
                .request
                .map(crate::llm::Chat::from_payload)
                .transpose()
        }
    }
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
        _ => {}
    }
    None
}

/// Run the shell hooks and then the subscribed plugins for `event`. Hooks run in configuration
/// order and each sees earlier rewrites; plugins receive the resulting payload together.
pub fn fire_with(
    hooks: &[HookConfig],
    plugins: Option<&crate::tools::plugin::Host>,
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
    };
    let mut output = Vec::new();
    let block = |outcome: &mut Outcome, reason: String, source: &str| {
        let reason = if reason.trim().is_empty() {
            format!("blocked by a {event} {source}")
        } else {
            reason
        };
        outcome.verdict = Verdict::Block { reason };
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
    plugins: Option<&crate::tools::plugin::Host>,
    root: &Path,
    name: &str,
    args: &Value,
    interrupt: &AtomicBool,
    notice: &mut dyn FnMut(&str),
) -> Result<Option<Value>, String> {
    if !wanted(hooks, plugins, "pre_tool_use") {
        return Ok(None);
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
    match outcome.verdict {
        Verdict::Block { reason } => Err(format!("blocked by a pre_tool_use hook: {reason}")),
        Verdict::Continue { output } => {
            if !output.is_empty() {
                notice(&format!("hook: {output}"));
            }
            Ok(outcome.input.filter(|input| input != args))
        }
    }
}

/// `post_tool_use`: hook output is appended to the tool result so the model sees it (a formatter's
/// diagnostics, say). A structured `content` replaces the result text; a block (exit 2) marks the
/// result as an error with the reason.
#[cfg(test)]
pub fn post_tool(
    hooks: &[HookConfig],
    root: &Path,
    name: &str,
    args: &Value,
    out: &mut crate::tools::ToolOutput,
    interrupt: &AtomicBool,
    notice: &mut dyn FnMut(&str),
) {
    post_tool_with(hooks, None, root, name, args, out, interrupt, notice)
}

#[allow(clippy::too_many_arguments)]
pub fn post_tool_with(
    hooks: &[HookConfig],
    plugins: Option<&crate::tools::plugin::Host>,
    root: &Path,
    name: &str,
    args: &Value,
    out: &mut crate::tools::ToolOutput,
    interrupt: &AtomicBool,
    notice: &mut dyn FnMut(&str),
) {
    if !wanted(hooks, plugins, "post_tool_use") {
        return;
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
    if text.is_empty() {
        return;
    }
    out.text.push_str(&format!("\n[post_tool_use hook] {text}"));
    out.is_error |= is_error;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn presentation_hooks_cannot_clear_execution_uncertainty_or_committed_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut output = crate::tools::ToolOutput::structured(json!([{"kept":true}]))
            .with_effects(crate::tools::outcome::Effects::Unknown)
            .with_changes([root.path().join("changed")]);
        post_tool(
            &[hook(
                "post_tool_use",
                None,
                "echo '{\"content\":\"replacement\",\"is_error\":false}'",
            )],
            root.path(),
            "edit",
            &json!({}),
            &mut output,
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert_eq!(output.text, "replacement");
        assert!(output.is_error && output.data.is_none());
        assert_eq!(
            output.execution.effects,
            crate::tools::outcome::Effects::Unknown
        );
        assert_eq!(
            output.execution.changed_paths,
            [root.path().join("changed")]
        );
    }

    #[cfg(unix)]
    fn hook(event: &str, matcher: Option<&str>, script: &str) -> HookConfig {
        HookConfig {
            event: event.into(),
            matcher: matcher.map(str::to_owned),
            command: vec!["sh".into(), "-c".into(), script.into()],
            timeout_secs: 5,
            fail_closed: None,
        }
    }

    #[cfg(windows)]
    fn hook(event: &str, matcher: Option<&str>, script: &str) -> HookConfig {
        let script = match script {
            "grep -q '\"tool\":\"edit\"' && echo seen" => {
                "$text = [Console]::In.ReadToEnd(); if ($text.Contains('\"tool\":\"edit\"')) { 'seen' }"
            }
            "echo no >&2; exit 2" => "[Console]::Error.WriteLine('no'); exit 2",
            "sleep 5" => "Start-Sleep -Seconds 5",
            "exit 1" => "exit 1",
            "echo formatted" => "Write-Output 'formatted'",
            "echo lint failed >&2; exit 2" => "[Console]::Error.WriteLine('lint failed'); exit 2",
            "true" => "exit 0",
            r#"printf '{"input":{"command":"ls -la"},"notice":"normalized"}'"# => {
                r#"Write-Output '{"input":{"command":"ls -la"},"notice":"normalized"}'"#
            }
            r#"grep -q 'ls -la' || { echo 'rewrite not visible' >&2; exit 2; }"# => {
                "$text = [Console]::In.ReadToEnd(); if (!$text.Contains('ls -la')) { [Console]::Error.WriteLine('rewrite not visible'); exit 2 }"
            }
            r#"printf '{"decision":"block","reason":"policy"}'"# => {
                r#"Write-Output '{"decision":"block","reason":"policy"}'"#
            }
            r#"printf '{"content":"[redacted]","is_error":false}'"# => {
                r#"Write-Output '{"content":"[redacted]","is_error":false}'"#
            }
            _ => script,
        };
        HookConfig {
            event: event.into(),
            matcher: matcher.map(str::to_owned),
            command: vec![
                "powershell.exe".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script.into(),
            ],
            timeout_secs: 5,
            fail_closed: None,
        }
    }

    #[test]
    fn exit_codes_map_to_verdicts_and_stdin_is_json() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = [hook(
            "pre_tool_use",
            Some("edit|write"),
            "grep -q '\"tool\":\"edit\"' && echo seen",
        )];
        let (verdict, notices) = fire(
            &hooks,
            dir.path(),
            "pre_tool_use",
            Some("edit"),
            json!({"tool":"edit"}),
            &AtomicBool::new(false),
        );
        assert_eq!(
            verdict,
            Verdict::Continue {
                output: "seen".into()
            }
        );
        assert!(notices.is_empty());
        let (verdict, _) = fire(
            &hooks,
            dir.path(),
            "pre_tool_use",
            Some("bash"),
            json!({"tool":"bash"}),
            &AtomicBool::new(false),
        );
        assert_eq!(
            verdict,
            Verdict::Continue {
                output: String::new()
            },
            "matcher is anchored and skips bash"
        );
        let hooks = [hook("pre_tool_use", None, "echo no >&2; exit 2")];
        assert_eq!(
            fire(
                &hooks,
                dir.path(),
                "pre_tool_use",
                Some("write"),
                json!({}),
                &AtomicBool::new(false),
            )
            .0,
            Verdict::Block {
                reason: "no".into()
            }
        );
    }

    #[test]
    fn failures_and_timeouts_never_block() {
        let dir = tempfile::tempdir().unwrap();
        let mut slow = hook("stop", None, "sleep 5");
        slow.timeout_secs = 1;
        let hooks = [
            hook("stop", None, "exit 1"),
            slow,
            HookConfig {
                command: vec!["/no/such/program".into()],
                ..hook("stop", None, "")
            },
        ];
        let (verdict, notices) = fire(
            &hooks,
            dir.path(),
            "stop",
            None,
            json!({}),
            &AtomicBool::new(false),
        );
        assert!(matches!(verdict, Verdict::Continue { .. }));
        assert_eq!(notices.len(), 3, "{notices:?}");
    }

    #[test]
    fn post_hook_output_reaches_the_model_and_block_marks_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = crate::tools::ToolOutput {
            execution: Default::default(),
            data: None,
            text: "ok".into(),
            is_error: false,
        };
        post_tool(
            &[hook("post_tool_use", None, "echo formatted")],
            dir.path(),
            "edit",
            &json!({}),
            &mut out,
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert_eq!(out.text, "ok\n[post_tool_use hook] formatted");
        assert!(!out.is_error);
        post_tool(
            &[hook("post_tool_use", None, "echo lint failed >&2; exit 2")],
            dir.path(),
            "edit",
            &json!({}),
            &mut out,
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert!(out.is_error && out.text.ends_with("lint failed"));
    }

    #[test]
    fn validation_rejects_bad_hooks() {
        assert!(validate(&hook("nope", None, "true")).is_err());
        assert!(validate(&hook("stop", Some("("), "true")).is_err());
        assert!(
            validate(&HookConfig {
                command: vec![],
                ..hook("stop", None, "")
            })
            .is_err()
        );
        assert!(validate(&hook("post_tool_use", Some("edit"), "true")).is_ok());
        assert!(
            validate(&hook("tool_call", None, "true")).is_ok(),
            "alias accepted"
        );
        assert!(validate(&hook("before_subagent_spawn", None, "true")).is_ok());
    }

    #[test]
    fn crashing_fail_closed_pre_tool_hook_blocks_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let interrupt = AtomicBool::new(false);
        let mut slow = hook("pre_tool_use", None, "sleep 5");
        slow.timeout_secs = 1;
        for failing in [
            hook("pre_tool_use", None, "exit 1"),
            slow,
            HookConfig {
                command: vec!["/no/such/program".into()],
                ..hook("pre_tool_use", None, "")
            },
        ] {
            assert!(
                fail_closed(&failing),
                "pre_tool_use defaults to fail-closed"
            );
            let result = pre_tool(
                &[failing],
                dir.path(),
                "bash",
                &json!({"command":"ls"}),
                &interrupt,
                &mut |_| {},
            );
            let reason = result.expect_err("a crashing policy hook must block");
            assert!(reason.contains("fail-closed"), "{reason}");
        }
        // Explicitly fail-open hooks only add a notice.
        let mut open = hook("pre_tool_use", None, "exit 1");
        open.fail_closed = Some(false);
        let mut notices = Vec::new();
        assert_eq!(
            pre_tool(
                &[open],
                dir.path(),
                "bash",
                &json!({}),
                &interrupt,
                &mut |n| notices.push(n.to_owned()),
            ),
            Ok(None)
        );
        assert_eq!(notices.len(), 1);
        // Other events stay fail-open by default; an opted-in stop hook blocks.
        assert!(!fail_closed(&hook("stop", None, "exit 1")));
        let mut strict = hook("stop", None, "exit 1");
        strict.fail_closed = Some(true);
        assert!(matches!(
            fire(&[strict], dir.path(), "stop", None, json!({}), &interrupt).0,
            Verdict::Block { .. }
        ));
    }

    #[test]
    fn structured_output_rewrites_tool_input_and_result() {
        let dir = tempfile::tempdir().unwrap();
        let interrupt = AtomicBool::new(false);
        let rewrite = hook(
            "tool_call",
            Some("bash"),
            r#"printf '{"input":{"command":"ls -la"},"notice":"normalized"}'"#,
        );
        // The second hook observes the first hook's rewrite.
        let check = hook(
            "pre_tool_use",
            None,
            r#"grep -q 'ls -la' || { echo 'rewrite not visible' >&2; exit 2; }"#,
        );
        let mut notices = Vec::new();
        let rewritten = pre_tool(
            &[rewrite, check],
            dir.path(),
            "bash",
            &json!({"command":"ls"}),
            &interrupt,
            &mut |n| notices.push(n.to_owned()),
        )
        .unwrap();
        assert_eq!(rewritten, Some(json!({"command":"ls -la"})));
        assert_eq!(notices, ["hook: normalized"]);
        let blocked = pre_tool(
            &[hook(
                "pre_tool_use",
                None,
                r#"printf '{"decision":"block","reason":"policy"}'"#,
            )],
            dir.path(),
            "bash",
            &json!({}),
            &interrupt,
            &mut |_| {},
        );
        assert_eq!(
            blocked,
            Err("blocked by a pre_tool_use hook: policy".into())
        );
        let mut out = crate::tools::ToolOutput {
            execution: Default::default(),
            data: Some(json!({"raw":true})),
            text: "secret token=abc".into(),
            is_error: false,
        };
        post_tool(
            &[hook(
                "tool_result",
                None,
                r#"printf '{"content":"[redacted]","is_error":false}'"#,
            )],
            dir.path(),
            "bash",
            &json!({}),
            &mut out,
            &interrupt,
            &mut |_| {},
        );
        assert_eq!(out.text, "[redacted]");
        assert!(out.data.is_none() && !out.is_error);
    }

    #[test]
    fn before_provider_request_rewrites_validates_and_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let interrupt = AtomicBool::new(false);
        let request = crate::llm::Chat {
            messages: vec![json!({"role":"user","content":"hi"})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 64,
            seed: None,
        };
        let rewrite = hook(
            "before_provider_request",
            None,
            r#"printf '{"payload":{"messages":[{"role":"user","content":"rewritten"}],"tools":[],"temperature":0.0,"max_tokens":64,"seed":null}}'"#,
        );
        // The second hook sees the first hook's rewritten body in its payload.
        let check = hook(
            "before_provider_request",
            None,
            r#"grep -q rewritten || { echo 'rewrite not visible' >&2; exit 2; }"#,
        );
        let mut notices = Vec::new();
        let sent = provider_request(
            &[rewrite, check],
            None,
            dir.path(),
            &request,
            &interrupt,
            &mut |n| notices.push(n.to_owned()),
        )
        .unwrap()
        .expect("a hook rewrote the request");
        assert_eq!(
            sent.messages,
            vec![json!({"role":"user","content":"rewritten"})]
        );
        assert_eq!(sent.max_tokens, 64);
        assert!(notices.is_empty(), "{notices:?}");

        // A rewrite that is not a valid request is refused, never sent.
        let malformed = hook(
            "before_provider_request",
            None,
            r#"printf '{"payload":{"messages":"nope"}}'"#,
        );
        assert!(
            provider_request(
                &[malformed],
                None,
                dir.path(),
                &request,
                &interrupt,
                &mut |_| {}
            )
            .is_err()
        );

        // A block refuses the request and carries its reason.
        let blocked = hook(
            "before_provider_request",
            None,
            r#"printf '{"decision":"block","reason":"no egress"}'"#,
        );
        let error = provider_request(
            &[blocked],
            None,
            dir.path(),
            &request,
            &interrupt,
            &mut |_| {},
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no egress"), "{error}");

        // Without hooks the request is sent unchanged and nothing is cloned.
        assert!(
            provider_request(&[], None, dir.path(), &request, &interrupt, &mut |_| {})
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn oversized_json_reply_is_reported_not_read_as_text() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"printf '{"notice":"'; head -c 40000 /dev/zero | tr '\0' a; printf '"}'"#;
        let (verdict, notices) = fire(
            &[hook("stop", None, script)],
            dir.path(),
            "stop",
            None,
            json!({}),
            &AtomicBool::new(false),
        );
        assert!(matches!(verdict, Verdict::Continue { .. }));
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("exceeds"), "{notices:?}");
    }

    #[test]
    fn matchers_are_compiled_once() {
        let h = hook("pre_tool_use", Some("edit|write"), "true");
        assert!(matches(&h, Some("edit")));
        assert!(!matches(&h, Some("editor")));
        assert!(MATCHERS.lock().contains_key("edit|write"));
    }
}
