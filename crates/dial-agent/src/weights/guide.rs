//! Help for getting started with weight edits: named goal presets for `weights improve`,
//! generation cases built from a past conversation, and the suggested next step from the status.
use serde_json::{Value, json};

/// Named goals `weights improve PRESET` expands to, as (name, goal).
pub const PRESETS: [(&str, &str); 4] = [
    (
        "loops",
        "stop endless repetition: answers that repeat the same lines or phrases until the token limit",
    ),
    (
        "concise",
        "make answers shorter and more direct without dropping what was asked for",
    ),
    (
        "rambling",
        "stop answers that keep going after the question is answered and hit the token limit",
    ),
    (
        "format",
        "follow requested output formats (JSON, lists, code blocks) exactly",
    ),
];

/// The goal for `goal`: a preset's text when `goal` is exactly a preset name, otherwise `goal`.
pub fn expand_goal(goal: &str) -> String {
    let goal = goal.trim();
    PRESETS
        .iter()
        .find(|(name, _)| *name == goal)
        .map_or_else(|| goal.to_owned(), |(_, text)| (*text).to_owned())
}

/// Most characters kept per message of a case built from a conversation.
const MAX_CASE_TEXT: usize = 8000;

/// The plain text of a message's content: a string, or the text parts of a list.
fn text(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.chars().take(MAX_CASE_TEXT).collect())
}

/// Generation cases from a conversation: one per user turn (the newest `limit`), each holding the
/// user and assistant text before it plus that turn, so the model answers it again. Tool calls,
/// tool results and system messages are left out; `context` bounds the earlier turns kept.
pub fn cases_from_messages(messages: &[Value], limit: usize, context: usize) -> Vec<Value> {
    let turns: Vec<(String, String)> = messages
        .iter()
        .filter(|message| matches!(message["role"].as_str(), Some("user" | "assistant")))
        .filter(|message| message["tool_calls"].is_null())
        .filter_map(|message| {
            Some((
                message["role"].as_str()?.to_owned(),
                text(&message["content"])?,
            ))
        })
        .collect();
    let mut cases: Vec<Value> = turns
        .iter()
        .enumerate()
        .filter(|(_, (role, _))| role == "user")
        .map(|(index, _)| {
            let start = index.saturating_sub(context);
            let messages: Vec<Value> = turns[start..=index]
                .iter()
                .skip_while(|(role, _)| role != "user")
                .map(|(role, content)| json!({"role": role, "content": content}))
                .collect();
            json!({ "messages": messages })
        })
        .collect();
    let excess = cases.len().saturating_sub(limit);
    cases.drain(..excess);
    cases
}

/// What to do next given a weights [`status`](super::status), as commands with a reason.
pub fn next_steps(status: &Value) -> Vec<String> {
    if status["target"]["path"].is_null() {
        return vec![
            "Choose a local MLX model: `dial --provider mlx --model PATH weights next`, or set `weights.model`."
                .into(),
        ];
    }
    if !status["progress"].is_null() {
        return vec!["A weights job is running; `dial weights status` shows its progress.".into()];
    }
    let mut steps = Vec::new();
    let edits = status["edits"].as_array().cloned().unwrap_or_default();
    for edit in &edits {
        let name = edit["name"].as_str().unwrap_or("?");
        match edit["passed"].as_bool() {
            Some(true) => steps.push(format!(
                "Edit {name} passed its evaluation: `dial weights promote {name}` makes it this project's model."
            )),
            None => steps.push(format!(
                "Edit {name} is not evaluated: `dial weights evaluate {name} --cases HELD_OUT.json` (8+ cases the experiment never saw; `dial weights cases --session ID` builds them)."
            )),
            Some(false) => {}
        }
    }
    if !steps.is_empty() {
        return steps;
    }
    if status["analyze"].is_null() {
        steps.push(
            "Start with an atlas of the model: `dial weights analyze` (what each layer, head and expert does)."
                .into(),
        );
    }
    let open = status["experiments"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| e["used"].as_u64() < e["budget"].as_u64());
    match open {
        None => steps.push(
            "Start an experiment: `dial weights experiment NAME --budget 16`, then `dial weights cases --session ID --output cases.json` to collect cases of the problem."
                .into(),
        ),
        Some(experiment) => {
            let name = experiment["experiment"].as_str().unwrap_or("NAME");
            let ran = |op: &str| {
                experiment["runs"]
                    .as_array()
                    .is_some_and(|runs| runs.iter().any(|run| run["op"] == op))
            };
            steps.push(if !ran("generate") {
                format!("Measure the problem: `dial weights generate --experiment {name} --cases cases.json`.")
            } else if !ran("attribute") && !ran("direction") {
                format!("Find what drives it: `dial weights attribute --experiment {name} --cases answered.json --focus repeats`.")
            } else if !ran("try") {
                format!("Score a candidate edit in memory first: `dial weights try --experiment {name} --cases answered.json --changes changes.json --doses 0.5,1`.")
            } else {
                format!("Write the best candidate: `dial weights edit NAME --experiment {name} --changes changes.json`.")
            });
        }
    }
    steps
}

#[cfg(test)]
mod tests;
