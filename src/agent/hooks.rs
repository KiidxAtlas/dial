//! Operator-owned lifecycle hooks: programs run at fixed points of a turn.
//!
//! Hooks come from the user's configuration (`[[hooks]]`), which lives outside the project, so the
//! agent cannot add or edit them. A hook receives one JSON object on stdin and may
//! * exit 0: continue (stdout is shown as a notice, and on `post_tool_use` appended to the result),
//! * exit 2: block (`pre_tool_use` refuses the call, `user_prompt_submit` rejects the message);
//!   stderr is the reason handed to the model or user,
//! * exit anything else, time out or fail to start: a notice, and the action proceeds.
//!
//! Hooks are trusted programs and run unsandboxed in the project directory, like `[mcp.*]` servers.
use crate::config::types::HookConfig;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Longest hook output kept (bytes).
const MAX_OUTPUT: usize = 8 * 1024;

pub const EVENTS: &[&str] = &[
    "user_prompt_submit",
    "pre_tool_use",
    "post_tool_use",
    "stop",
];

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Continue { output: String },
    Block { reason: String },
}

pub fn validate(hook: &HookConfig) -> Result<(), String> {
    if !EVENTS.contains(&hook.event.as_str()) {
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
    if hook.timeout_secs == 0 || hook.timeout_secs > 600 {
        return Err("hook timeout_secs must be 1..=600".into());
    }
    if let Some(matcher) = &hook.matcher {
        regex::Regex::new(&format!("^(?:{matcher})$"))
            .map_err(|e| format!("invalid hook matcher: {e}"))?;
    }
    Ok(())
}

fn matches(hook: &HookConfig, tool: Option<&str>) -> bool {
    match (&hook.matcher, tool) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(pattern), Some(tool)) => {
            regex::Regex::new(&format!("^(?:{pattern})$")).is_ok_and(|re| re.is_match(tool))
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

fn run_one(hook: &HookConfig, root: &Path, payload: &Value) -> Result<Verdict, String> {
    let mut child = Command::new(&hook.command[0])
        .args(&hook.command[1..])
        .current_dir(root)
        .env("DIAL_HOOK_EVENT", &hook.event)
        .env("DIAL_PROJECT_DIR", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start {:?}: {e}", hook.command[0]))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A hook that ignores stdin may exit first; a broken pipe is not a hook failure.
        let _ = stdin.write_all(payload.to_string().as_bytes());
    }
    let drain = |stream: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut stream) = stream {
                let _ = (&mut stream)
                    .take(MAX_OUTPUT as u64 * 4)
                    .read_to_end(&mut bytes);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let out = drain(child.stdout.take().map(|s| Box::new(s) as _));
    let err = drain(child.stderr.take().map(|s| Box::new(s) as _));
    let deadline = Instant::now() + Duration::from_secs(hook.timeout_secs);
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{:?} timed out after {}s",
                    hook.command[0], hook.timeout_secs
                ));
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let (stdout, stderr) = (
        clip(out.join().unwrap_or_default()),
        clip(err.join().unwrap_or_default()),
    );
    match status.code() {
        Some(0) => Ok(Verdict::Continue { output: stdout }),
        Some(2) => Ok(Verdict::Block {
            reason: if stderr.is_empty() { stdout } else { stderr },
        }),
        other => Err(format!(
            "{:?} exited with {} {}",
            hook.command[0],
            other.map_or("a signal".into(), |c| c.to_string()),
            stderr
        )),
    }
}

/// Run every hook for `event` whose matcher accepts `tool`, in configuration order. The first block
/// wins. Failures of individual hooks are returned as notices and never block.
pub fn fire(
    hooks: &[HookConfig],
    root: &Path,
    event: &str,
    tool: Option<&str>,
    payload: Value,
) -> (Verdict, Vec<String>) {
    let mut payload = payload;
    payload["event"] = json!(event);
    payload["cwd"] = json!(root);
    let mut notices = Vec::new();
    let mut output = Vec::new();
    for hook in hooks
        .iter()
        .filter(|h| h.event == event && matches(h, tool))
    {
        match run_one(hook, root, &payload) {
            Ok(Verdict::Block { reason }) => {
                let reason = if reason.is_empty() {
                    format!("blocked by a {event} hook")
                } else {
                    reason
                };
                return (Verdict::Block { reason }, notices);
            }
            Ok(Verdict::Continue { output: text }) => {
                if !text.is_empty() {
                    output.push(text);
                }
            }
            Err(why) => notices.push(format!("{event} hook failed: {why}")),
        }
    }
    (
        Verdict::Continue {
            output: output.join("\n"),
        },
        notices,
    )
}

/// `pre_tool_use`: `Err(reason)` refuses the call. Notices (failed hooks) are passed to `notice`.
pub fn pre_tool(
    hooks: &[HookConfig],
    root: &Path,
    name: &str,
    args: &Value,
    notice: &mut dyn FnMut(&str),
) -> Result<(), String> {
    if hooks.is_empty() {
        return Ok(());
    }
    let (verdict, notices) = fire(
        hooks,
        root,
        "pre_tool_use",
        Some(name),
        json!({"tool":name,"input":args}),
    );
    notices.iter().for_each(|n| notice(n));
    match verdict {
        Verdict::Block { reason } => Err(format!("blocked by a pre_tool_use hook: {reason}")),
        Verdict::Continue { output } => {
            if !output.is_empty() {
                notice(&format!("hook: {output}"));
            }
            Ok(())
        }
    }
}

/// `post_tool_use`: hook output is appended to the tool result so the model sees it (a formatter's
/// diagnostics, say). A block (exit 2) marks the result as an error with the reason.
pub fn post_tool(
    hooks: &[HookConfig],
    root: &Path,
    name: &str,
    args: &Value,
    out: &mut crate::tools::ToolOutput,
    notice: &mut dyn FnMut(&str),
) {
    if hooks.is_empty() {
        return;
    }
    let (verdict, notices) = fire(
        hooks,
        root,
        "post_tool_use",
        Some(name),
        json!({"tool":name,"input":args,"is_error":out.is_error,"output":clip(out.text.clone())}),
    );
    notices.iter().for_each(|n| notice(n));
    let (text, is_error) = match verdict {
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

    fn hook(event: &str, matcher: Option<&str>, script: &str) -> HookConfig {
        HookConfig {
            event: event.into(),
            matcher: matcher.map(str::to_owned),
            command: vec!["sh".into(), "-c".into(), script.into()],
            timeout_secs: 5,
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
            fire(&hooks, dir.path(), "pre_tool_use", Some("write"), json!({})).0,
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
        let (verdict, notices) = fire(&hooks, dir.path(), "stop", None, json!({}));
        assert!(matches!(verdict, Verdict::Continue { .. }));
        assert_eq!(notices.len(), 3, "{notices:?}");
    }

    #[test]
    fn post_hook_output_reaches_the_model_and_block_marks_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = crate::tools::ToolOutput {
            text: "ok".into(),
            is_error: false,
        };
        post_tool(
            &[hook("post_tool_use", None, "echo formatted")],
            dir.path(),
            "edit",
            &json!({}),
            &mut out,
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
    }
}
