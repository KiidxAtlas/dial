//! Agent loop for the agentic commit flow: drive the model through the commit tools
//! until the required proposals exist, or the turn and reminder budgets run out.

use serde_json::json;

use super::template::{Value, render};
use super::tools::{ToolEnv, execute, tool_schemas};
use crate::commit::commit_types::types_description;
use crate::llm::Chat;

/// Upper bound on model turns; a run that never produces the required proposals falls back to the caller.
const MAX_TURNS: usize = 40;
/// Reminders sent when the model stops calling tools before the required proposals exist.
const MAX_REMINDERS: usize = 3;
const AGENT_TEMPERATURE: f64 = 0.2;
const AGENT_MAX_TOKENS: u32 = 4096;
const SYSTEM_TEMPLATE: &str = include_str!("prompts/system.md");
const USER_TEMPLATE: &str = include_str!("prompts/session-user.md");

/// Existing Unreleased sections of one changelog target: `(section name, items)` pairs.
pub(crate) type ExistingEntries = (String, Vec<(String, Vec<String>)>);

/// Required outputs that are still missing, as described to the model.
fn missing_items(env: &ToolEnv<'_>) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if env.state.proposal.is_none() && env.state.split_proposal.is_none() {
        missing.push("commit proposal (propose_commit or split_commit)");
    }
    if !env.changelog_targets.is_empty() && env.state.changelog_proposal.is_none() {
        missing.push("changelog entries (propose_changelog)");
    }
    missing
}

fn reminder(missing: &[&str], n: usize) -> String {
    format!(
        "<system-reminder>\nCRITICAL: You must call the required tools before finishing.\n\nMissing: {}\nReminder {n} of {MAX_REMINDERS}.\n\nCall the missing tool(s) now.\n</system-reminder>",
        missing.join(", ")
    )
}

fn existing_value(existing: &[ExistingEntries]) -> Value {
    Value::List(
        existing
            .iter()
            .map(|(path, sections)| {
                Value::map([
                    ("path", Value::text(path)),
                    (
                        "sections",
                        Value::List(
                            sections
                                .iter()
                                .map(|(name, items)| {
                                    Value::map([
                                        ("name", Value::text(name)),
                                        (
                                            "items",
                                            Value::List(
                                                items
                                                    .iter()
                                                    .map(|item| Value::text(item))
                                                    .collect(),
                                            ),
                                        ),
                                    ])
                                })
                                .collect(),
                        ),
                    ),
                ])
            })
            .collect(),
    )
}

/// Run the commit agent. Returns `Err` only for backend failures; an incomplete run is left for the caller to
/// detect from `env.state`.
pub(crate) fn run_agent(
    env: &mut ToolEnv<'_>,
    user_context: Option<&str>,
    existing: &[ExistingEntries],
) -> Result<(), String> {
    let system = render(
        SYSTEM_TEMPLATE,
        &Value::map([("types_description", Value::text(types_description()))]),
    )?;
    let user = render(
        USER_TEMPLATE,
        &Value::map([
            ("user_context", Value::text(user_context.unwrap_or(""))),
            (
                "changelog_targets",
                Value::text(env.changelog_targets.join("\n")),
            ),
            ("existing_changelog_entries", existing_value(existing)),
        ]),
    )?;
    let mut messages = vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user", "content": user}),
    ];
    let mut reminders = 0usize;

    for _turn in 0..MAX_TURNS {
        let chat = Chat {
            messages: messages.clone(),
            tools: tool_schemas(),
            temperature: AGENT_TEMPERATURE,
            max_tokens: AGENT_MAX_TOKENS,
            seed: None,
        };
        let reply = env.backend.complete(env.config, &chat)?.reply;

        if reply.tool_calls.is_empty() {
            if missing_items(env).is_empty() || reminders >= MAX_REMINDERS {
                return Ok(());
            }
            reminders += 1;
            messages.push(json!({"role": "assistant", "content": reply.text}));
            messages
                .push(json!({"role": "user", "content": reminder(&missing_items(env), reminders)}));
            continue;
        }

        let calls: Vec<serde_json::Value> = reply
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {"name": call.name, "arguments": call.arguments.to_string()},
                })
            })
            .collect();
        messages.push(json!({"role": "assistant", "content": reply.text, "tool_calls": calls}));
        for call in &reply.tool_calls {
            let result = if call.malformed_arguments {
                format!("Error: malformed arguments for {}", call.name)
            } else {
                execute(env, &call.name, &call.arguments)
            };
            messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": result}));
        }
        if missing_items(env).is_empty() {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::run::CommitBackend;
    use crate::commit::run::Completion;
    use crate::config::types::Config;
    use crate::llm::Reply;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Always answers with plain text, so the loop can only end through reminders.
    struct TextOnly {
        calls: AtomicUsize,
    }

    impl CommitBackend for TextOnly {
        fn complete(&self, _config: &Config, _request: &Chat) -> Result<Completion, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Completion {
                reply: Reply {
                    text: "done".to_owned(),
                    ..Default::default()
                },
                cost_usd: None,
            })
        }
    }

    #[test]
    fn reminders_are_capped_and_leave_state_incomplete() {
        let backend = TextOnly {
            calls: AtomicUsize::new(0),
        };
        let config = Config::default();
        let mut env = ToolEnv {
            root: std::path::Path::new("."),
            config: &config,
            backend: &backend,
            staged: vec!["a.rs".to_owned()],
            numstat: Vec::new(),
            changelog_targets: Vec::new(),
            state: Default::default(),
        };
        run_agent(&mut env, None, &[]).expect("loop ends without error");
        assert_eq!(backend.calls.load(Ordering::SeqCst), MAX_REMINDERS + 1);
        assert!(env.state.proposal.is_none() && env.state.split_proposal.is_none());
    }

    #[test]
    fn reminder_lists_missing_items() {
        let text = reminder(&["commit proposal (propose_commit or split_commit)"], 2);
        assert!(text.contains("Missing: commit proposal (propose_commit or split_commit)"));
        assert!(text.contains("Reminder 2 of 3."));
    }
}
