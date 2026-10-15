//! Structure of similar verified successful tasks from this project, as advisory prompt text: a short
//! excerpt of the prior request, the ordered tool calls with argument key names, and the verifier.
//! Old argument values and outputs never enter the new prompt.
use super::{repair_terms, truncate};
use crate::storage::store::Store;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashSet;

/// Verified live prompts scanned for term overlap; payloads are loaded only for the best `LOADED`.
const SCANNED: usize = 256;
const LOADED: usize = 8;
const MAX_CALLS: usize = 8;
const MAX_EXAMPLES: usize = 2;
const EXCERPT_CHARS: usize = 100;

pub(super) fn verified_examples(
    store: &Store,
    project_root: &str,
    query: &str,
    offered: &[String],
) -> Result<Vec<String>> {
    let query_terms = repair_terms(query);
    if query_terms.is_empty() {
        return Ok(Vec::new());
    }
    let offered: HashSet<&str> = offered.iter().map(String::as_str).collect();
    let threshold = if query_terms.len() <= 3 { 1 } else { 2 };
    let mut candidates: Vec<(usize, i64, String, String)> = store
        .recent_verified_live(SCANNED)?
        .into_iter()
        .filter_map(|(trajectory, prompt, source)| {
            let overlap = query_terms.intersection(&repair_terms(&prompt)).count();
            (overlap >= threshold).then_some((overlap, trajectory, prompt, source))
        })
        .collect();
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut seen = HashSet::new();
    let mut lines = Vec::new();
    for (_, trajectory, prompt, source) in candidates.into_iter().take(LOADED) {
        // A corrupt or vanished snapshot only loses this advisory example.
        let Ok(Some(payload)) = store.trajectory_messages(trajectory) else {
            continue;
        };
        let record = match payload["examples"].as_array() {
            Some(examples) => match examples.last() {
                Some(record) => record,
                None => continue,
            },
            None => &payload,
        };
        if [&payload, record].iter().any(|value| {
            value
                .get("project_root")
                .is_some_and(|root| root.as_str() != Some(project_root))
        }) {
            continue;
        }
        let Some(messages) = record["messages"].as_array() else {
            continue;
        };
        let calls = tool_sequence(messages, &prompt, &offered);
        if calls.is_empty() || !seen.insert(calls.clone()) {
            continue;
        }
        let excerpt: String = prompt
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .filter(|c| !c.is_control())
            .take(EXCERPT_CHARS)
            .collect();
        let source: String = source
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-' | '.'))
            .take(32)
            .collect();
        let source = if source.is_empty() {
            "an external verifier".to_owned()
        } else {
            source
        };
        lines.push(truncate(
            &format!(
                "Verified task \"{excerpt}\" (verified by {source}) used: {}. Re-read and validate current values.",
                calls.join(" -> ")
            ),
            480,
        ));
        if lines.len() == MAX_EXAMPLES {
            break;
        }
    }
    Ok(lines)
}

/// Offered tool calls of the verified turn, in order: those after the user message that carried the
/// trajectory prompt (the last user message when it cannot be located), with argument key names only.
fn tool_sequence(messages: &[Value], prompt: &str, offered: &HashSet<&str>) -> Vec<String> {
    let prompt = prompt.trim();
    let users = || {
        messages
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, message)| message["role"] == "user")
    };
    let start = users()
        .find(|(_, message)| text(&message["content"]).contains(prompt))
        .or_else(|| users().next())
        .map_or(0, |(index, _)| index + 1);
    messages[start..]
        .iter()
        .filter(|message| message["role"] == "assistant")
        .flat_map(|message| message["tool_calls"].as_array().into_iter().flatten())
        .filter_map(|call| {
            let name = call.pointer("/function/name")?.as_str()?;
            offered.contains(name).then(|| {
                format!(
                    "{name}({})",
                    argument_keys(&call["function"]["arguments"]).join(", ")
                )
            })
        })
        .take(MAX_CALLS)
        .collect()
}

fn argument_keys(arguments: &Value) -> Vec<String> {
    let parsed = arguments
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let mut keys = parsed
        .as_ref()
        .unwrap_or(arguments)
        .as_object()
        .map(|object| {
            object
                .keys()
                .filter(|key| {
                    key.len() <= 32
                        && key
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    keys.sort();
    keys.truncate(5);
    keys
}

fn text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::Outcome;
    use serde_json::json;

    fn call(name: &str, arguments: Value) -> Value {
        json!({"role":"assistant","content":null,"tool_calls":[{
            "id":format!("call-{name}"),"type":"function",
            "function":{"name":name,"arguments":arguments.to_string()}
        }]})
    }

    fn record(store: &Store, domain: &str, task: &str, prompt: &str, payload: Value, score: f64) {
        let version = store.active().unwrap().id;
        let id = store
            .begin_trajectory(version, domain, task, prompt)
            .unwrap();
        store.save_messages(id, &payload).unwrap();
        store.finish_trajectory(id, Some("done"), None).unwrap();
        store
            .outcome(&Outcome {
                trajectory: id,
                score,
                source: "user".into(),
                reference: None,
            })
            .unwrap();
    }

    fn turn(prompt: &str, tool: &str) -> Value {
        json!({"examples":[{"messages":[
            {"role":"user","content":prompt},
            call(tool, json!({"path":"private/secret.txt"})),
            {"role":"tool","tool_call_id":format!("call-{tool}"),"content":"SECRET_OUTPUT"},
            {"role":"assistant","content":"done"}
        ],"tools":[]}]})
    }

    #[test]
    fn examples_are_live_project_scoped_relevant_offered_and_omit_old_values() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            "test-model",
            "test-usecase",
            &Default::default(),
        )
        .unwrap();
        let prompt = "Refactor the tokenizer module for unicode identifiers";
        record(
            &store,
            "coding",
            "session:a",
            prompt,
            json!({"examples":[{"messages":[
            {"role":"system","content":"rules"},
            {"role":"user","content":"Earlier unrelated request"},
            call("glob", json!({"pattern":"src/**"})),
            {"role":"assistant","content":"found"},
            {"role":"user","content":format!("{prompt}\n\nAdvisory notes")},
            call("read", json!({"path":"private/secret.txt"})),
            {"role":"tool","tool_call_id":"call-read","content":"SECRET_OUTPUT"},
            call("edit", json!({"path":"private/secret.txt","input":"SECRET_VALUE"})),
            {"role":"tool","tool_call_id":"call-edit","content":"ok"},
            call("bash", json!({"command":"cargo test SECRET_COMMAND"})),
            {"role":"tool","tool_call_id":"call-bash","content":"ok"},
            {"role":"assistant","content":"done"}
        ],"tools":[]}]}),
            100.0,
        );
        record(
            &store,
            "selfplay:files",
            "selfplay",
            "Rename the scheduler queue",
            turn("Rename the scheduler queue", "read"),
            100.0,
        );
        record(
            &store,
            "coding",
            "session:b",
            "Migrate the billing ledger",
            json!({"project_root":"/workspace/b","messages":[
                {"role":"user","content":"Migrate the billing ledger"},
                call("read", json!({"path":"ledger.rs"}))
            ]}),
            100.0,
        );
        record(
            &store,
            "coding",
            "session:c",
            "Document the cache eviction",
            turn("Document the cache eviction", "read"),
            90.0,
        );
        let offered: Vec<String> = ["read", "edit", "bash", "glob"].map(String::from).to_vec();
        let lookup = |query: &str, offered: &[String]| {
            verified_examples(&store, "/workspace/a", query, offered).unwrap()
        };

        let examples = lookup("please refactor the tokenizer for unicode", &offered);
        assert_eq!(examples.len(), 1);
        assert!(examples[0].contains(prompt) && examples[0].contains("by user"));
        assert!(
            examples[0].contains("used: read(path) -> edit(input, path) -> bash(command)."),
            "only this turn's calls, in order: {}",
            examples[0]
        );
        for leaked in [
            "secret.txt",
            "SECRET_VALUE",
            "SECRET_COMMAND",
            "SECRET_OUTPUT",
            "Advisory",
        ] {
            assert!(
                !examples[0].contains(leaked),
                "examples must not replay old values: {leaked}"
            );
        }

        // Relevance: four or more query terms need two shared terms.
        assert!(lookup("weather forecast paris", &offered).is_empty());
        assert!(lookup("tokenizer weather forecast paris london", &offered).is_empty());
        assert_eq!(
            lookup("unicode tokenizer weather forecast", &offered).len(),
            1
        );

        assert_eq!(
            lookup("refactor the tokenizer", &["read".into()]),
            [format!(
                "Verified task \"{prompt}\" (verified by user) used: read(path). Re-read and validate current values."
            )]
        );
        assert!(lookup("refactor the tokenizer", &["write".into()]).is_empty());
        assert!(
            lookup("rename the scheduler queue", &offered).is_empty(),
            "self-play is excluded"
        );
        assert!(
            lookup("migrate the billing ledger", &offered).is_empty(),
            "other projects are excluded"
        );
        assert!(
            lookup("document the cache eviction", &offered).is_empty(),
            "only perfect scores count"
        );
    }
}
