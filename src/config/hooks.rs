//! Lifecycle hook event names and hook validation.
use crate::config::types::HookConfig;

use anyhow::Result;

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
    crate::config::hooks::canonical_event(event)
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

pub fn canonical_event(event: &str) -> &str {
    match event {
        "input" | "user_prompt_submit" => "user_prompt_submit",
        "tool_call" | "pre_tool_use" => "pre_tool_use",
        "tool_result" | "post_tool_use" => "post_tool_use",
        "session_shutdown" | "session_end" => "session_end",
        "session_before_compact" | "before_compact" => "before_compact",
        "session_stop" | "stop" => "stop",
        other => other,
    }
}
