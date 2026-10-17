//! Legacy changelog generation: one model call per changelog target.

use std::path::Path;

use serde_json::{Value as Json, json};

use super::detect::ChangelogTarget;
use super::parse::{CATEGORY_ORDER, normalize_items};
use super::read_unreleased;
use crate::commit::agentic::state::ChangelogTargetProposal;
use crate::commit::agentic::template::{Value, render};
use crate::commit::execute::git_output;
use crate::commit::run::{CommitBackend, CommitError};
use crate::config::types::Config;
use crate::llm::{Chat, Reply};

const SYSTEM_PROMPT: &str = include_str!("prompts/changelog-system.md");
const USER_TEMPLATE: &str = include_str!("prompts/changelog-user.md");
const TOOL_NAME: &str = "create_changelog_entries";
/// Character budget for the staged diff sent to the model.
const DIFF_CHAR_LIMIT: usize = 120_000;
const MAX_TOKENS: u32 = 1200;
const TEMPERATURE: f64 = 0.2;

/// Keep the first `limit` characters and mark the elision, so the model sees how much was cut.
fn cap_chars(text: &str, limit: usize) -> String {
    let total = text.chars().count();
    if total <= limit {
        return text.to_owned();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}\n[…{}ch elided…]", total - limit)
}

fn changelog_tool_schema() -> Json {
    let mut properties = serde_json::Map::new();
    for category in CATEGORY_ORDER {
        properties.insert(
            category.to_owned(),
            json!({"type": "array", "items": {"type": "string"}}),
        );
    }
    json!({
        "type": "function",
        "function": {
            "name": TOOL_NAME,
            "description": "Record changelog entries for the staged changes, grouped by category. Omit categories with no entries.",
            "parameters": {
                "type": "object",
                "properties": {
                    "entries": {
                        "type": "object",
                        "properties": properties,
                        "additionalProperties": false
                    }
                },
                "required": ["entries"],
                "additionalProperties": false
            }
        }
    })
}

/// Entries from the create-entries tool call, or from a JSON object embedded in plain text.
fn extract_entries(reply: &Reply) -> Option<Json> {
    if let Some(call) = reply
        .tool_calls
        .iter()
        .find(|call| call.name == TOOL_NAME && !call.malformed_arguments)
    {
        return call.arguments.get("entries").cloned();
    }
    let start = reply.text.find('{')?;
    let end = reply.text.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str::<Json>(&reply.text[start..=end])
        .ok()?
        .get("entries")
        .cloned()
}

fn render_existing(existing: &[(String, Vec<String>)]) -> String {
    existing
        .iter()
        .map(|(name, items)| {
            let bullets: Vec<String> = items.iter().map(|item| format!("- {item}")).collect();
            format!("### {name}\n{}", bullets.join("\n"))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Generate the entries for one changelog target from the staged diff of its files.
/// Returns `None` when the model produces no entries for it.
pub(crate) fn generate_target(
    cwd: &Path,
    config: &Config,
    backend: &dyn CommitBackend,
    target: &ChangelogTarget,
) -> Result<Option<ChangelogTargetProposal>, CommitError> {
    const CONTEXT: &str = "Changelog generation failed";
    let mut diff_args = vec![
        "diff",
        "--cached",
        "--no-color",
        "--no-ext-diff",
        "--no-renames",
        "--",
    ];
    diff_args.extend(target.files.iter().map(String::as_str));
    let diff = cap_chars(&git_output(cwd, &diff_args, CONTEXT)?, DIFF_CHAR_LIMIT);

    let mut stat_args = vec!["diff", "--cached", "--stat", "--no-color", "--"];
    stat_args.extend(target.files.iter().map(String::as_str));
    let stat = git_output(cwd, &stat_args, CONTEXT)?;

    let existing = read_unreleased(cwd, &target.path).unwrap_or_default();
    let is_package = target.path != "CHANGELOG.md";
    let user = render(
        USER_TEMPLATE,
        &Value::map([
            ("changelog_path", Value::text(&target.path)),
            (
                "is_package_changelog",
                Value::text(if is_package { "true" } else { "" }),
            ),
            ("existing_entries", Value::text(render_existing(&existing))),
            ("stat", Value::text(stat.trim())),
            ("diff", Value::text(diff)),
        ]),
    )
    .map_err(CommitError::Model)?;

    let request = Chat {
        messages: vec![
            json!({"role": "system", "content": SYSTEM_PROMPT}),
            json!({"role": "user", "content": user}),
        ],
        tools: vec![changelog_tool_schema()],
        temperature: TEMPERATURE,
        max_tokens: MAX_TOKENS,
        seed: None,
    };
    let reply = backend
        .complete(config, &request)
        .map_err(CommitError::Model)?
        .reply;
    let entries_value = extract_entries(&reply).ok_or_else(|| {
        CommitError::Model(format!(
            "changelog generation returned no entries for {}",
            target.path
        ))
    })?;

    // Only known categories are kept, in canonical order; unknown categories are dropped silently.
    let mut entries: Vec<(String, Vec<String>)> = Vec::new();
    if let Some(map) = entries_value.as_object() {
        for category in CATEGORY_ORDER {
            let Some(items) = map.get(category).and_then(Json::as_array) else {
                continue;
            };
            let raw: Vec<String> = items
                .iter()
                .filter_map(Json::as_str)
                .map(str::to_owned)
                .collect();
            let items = normalize_items(&raw);
            if !items.is_empty() {
                entries.push((category.to_owned(), items));
            }
        }
    }
    if entries.is_empty() {
        return Ok(None);
    }
    Ok(Some(ChangelogTargetProposal {
        path: target.path.clone(),
        entries,
        deletions: Vec::new(),
    }))
}

#[cfg(test)]
mod tests;
