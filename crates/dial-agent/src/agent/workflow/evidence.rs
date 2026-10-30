//! Model-context projection leaves append-only evidence untouched on disk.
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
/// Provider-neutral wire estimate, never billed usage. Include structured text, tool calls,
/// native replay and images just as request sizing does; metadata is not model context.
pub(in crate::agent) fn estimate(message: &Value) -> usize {
    usize::try_from(crate::llm::wire::estimate(message)).unwrap_or(usize::MAX)
}

/// Which results were already replaced, by supersession or by pruning. A result is replaced at most once:
/// deciding again every step from a suffix that only ever grows would flip it back and forth, and each flip
/// rewrites the conversation prefix a provider or local server may have cached.
#[derive(Clone, Default)]
pub struct Supersession {
    replaced: HashSet<String>,
    pruned: HashSet<String>,
    resolved: HashMap<String, String>,
}

impl Supersession {
    /// Whether the model still sees this result in full, without any projection placeholder.
    pub fn holds_full(&self, id: &str) -> bool {
        !self.replaced.contains(id) && !self.pruned.contains(id) && !self.resolved.contains_key(id)
    }
}

/// How aggressively `project` may rewrite older messages.
#[derive(Clone, Copy)]
pub struct Policy {
    /// Replace a superseded read only while no more than this many estimated tokens follow it.
    pub warm_tokens: usize,
    /// The context is filling up: superseding and pruning are allowed regardless of what follows.
    pub pressure: bool,
    /// Results inside the newest this-many tokens are never pruned.
    pub protect_tokens: usize,
    /// `[compaction] supersede_reads`: allow replacing older reads of a resource that a later result repeats.
    pub supersede_reads: bool,
    /// `[compaction] drop_useless`: allow pruning observations that no longer carry needed evidence.
    pub drop_useless: bool,
}

#[cfg(test)]
impl Policy {
    fn calm() -> Self {
        Policy {
            warm_tokens: 10_000,
            pressure: false,
            protect_tokens: usize::MAX,
            supersede_reads: true,
            drop_useless: true,
        }
    }
}

/// Build the model-facing view of the session without touching the stored evidence:
///  * a user message's persisted `dial_recall` is folded into that same message, so the notes recalled for a turn
///    stay byte-identical on every later request instead of being inserted before the whole conversation;
///  * a successful observation of a resource+range, or an identical read-only call and result, that a later
///    result repeats becomes a short placeholder,
///    but only while the text after it is small (`warm_tokens`) or the context is under `pressure`. Replacing a
///    read buried under a long warm suffix would force that whole suffix to be prefilled again for little saving.
///    Mutations and unresolved errors stay verbatim. Resolved read-only failures keep their identity,
///    outcome and original artifact pointer; placeholders never change once written.
pub fn project(messages: &mut [Value], memo: &mut Supersession, policy: &Policy) {
    let Policy {
        warm_tokens,
        pressure,
        supersede_reads,
        ..
    } = *policy;
    for message in messages.iter_mut() {
        let Some(recall) = message
            .as_object_mut()
            .and_then(|m| m.remove("dial_recall"))
            .and_then(|v| v.as_str().map(str::to_owned))
        else {
            continue;
        };
        match &mut message["content"] {
            Value::String(text) => {
                text.push_str("\n\n");
                text.push_str(&recall);
            }
            Value::Array(parts) => parts.push(json!({"type":"text","text":recall})),
            other => *other = json!(recall),
        }
    }
    // Only exact duplicates of explicitly tagged harness notices are projected.
    let mut reminders = HashSet::new();
    for message in messages.iter_mut().rev() {
        if let Some(kind) = message["dial_reminder"].as_str() {
            let key = (kind.to_owned(), message["content"].to_string());
            const PLACEHOLDER: &str =
                "[Repeated harness notice; identical guidance retained later]";
            if !reminders.insert(key)
                && (pressure || warm_tokens > 0)
                && message["content"]
                    .as_str()
                    .is_some_and(|text| text.len() > PLACEHOLDER.len())
            {
                message["content"] = json!(PLACEHOLDER);
            }
        }
    }
    let mut suffix = vec![0usize; messages.len()];
    let mut running = 0usize;
    for i in (0..messages.len()).rev() {
        suffix[i] = running;
        running = running.saturating_add(estimate(&messages[i]));
    }
    let calls = observation_calls(messages);
    condense_resolved_failures(messages, memo, policy, &suffix, &calls);
    let mut latest: HashSet<String> = HashSet::new();
    let mut stale = vec![false; messages.len()];
    for i in (0..messages.len()).rev() {
        // Failed or unknown-effect results neither supersede earlier success nor lose diagnostics.
        if !successful(&messages[i]) {
            continue;
        }
        let Some(key) = observation_key(&messages[i], &calls) else {
            continue;
        };
        if !latest.contains(&key) {
            latest.insert(key);
            continue;
        }
        stale[i] = true;
        let id = result_id(&messages[i]);
        // A result already dropped for space keeps that one placeholder; rewriting it would change cached text.
        if memo.pruned.contains(&id) {
            continue;
        }
        // Exact successful duplicates lose no unique evidence, even with conservative defaults.
        let exact_duplicate = messages[i]["dial_evidence"]["observation"]
            .as_str()
            .is_none();
        let supersede = memo.replaced.contains(&id)
            || (pressure && exact_duplicate)
            || (supersede_reads && (pressure || suffix[i] <= warm_tokens));
        if !supersede || !messages[i]["dial_evidence"]["artifact"].is_string() {
            continue;
        }
        let artifact = messages[i]["dial_evidence"]["artifact"]
            .as_str()
            .unwrap_or("unavailable");
        let placeholder = format!(
            "[Superseded observation: a newer result for the same resource appears later in this conversation. Original evidence: {artifact}]"
        );
        // A pointer must actually save context. Images may still make a short text result large.
        if !memo.replaced.contains(&id)
            && messages[i]["dial_images"]
                .as_array()
                .is_none_or(Vec::is_empty)
            && messages[i]["content"]
                .as_str()
                .is_none_or(|text| text.len() <= placeholder.len())
        {
            continue;
        }
        if !id.is_empty() {
            memo.replaced.insert(id);
        }
        messages[i]["content"] = json!(placeholder);
        if let Some(object) = messages[i].as_object_mut() {
            object.remove("dial_images");
        }
    }
    prune(messages, memo, &suffix, &stale, policy, &calls);
    for message in messages {
        crate::llm::messages::canonicalize(message);
    }
}

/// Tools whose results are observations of the project, not changes to it, and whose full text is retrievable from
/// an artifact. Mutation results, todo and memory updates, and `artifact_read` itself are never dropped. Bounded
/// read-only investigation tools (data, documents, media, traces, timelines) are observations too: their text is
/// retained like any other result, so under pressure an old one is as recoverable as a `read`. Tools that also
/// record state (`workbench correct`, `evidence`, `research`, `investigate`) are deliberately absent.
const OBSERVATIONS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "find",
    "lsp",
    "ast_search",
    "dep_source",
    "web_fetch",
    "web_search",
    "data",
    "database",
    "document",
    "network_inspect",
    "runtime_inspect",
    "timeline",
    "evidence_search",
    "audio",
    "visual",
    "video",
];
/// A result smaller than this is not worth a placeholder and a retrieval round trip.
const PRUNE_MIN_TOKENS: usize = 1_000;
/// New prunes are applied together only once they free at least this much. Context keeps growing
/// under pressure, so pruning each result the moment it crossed the protected tail rewrote the
/// cached prefix on nearly every step; batching pays that cache break once per batch.
const PRUNE_BATCH_MIN_TOKENS: usize = 8_000;

fn result_id(message: &Value) -> String {
    message["tool_call_id"]
        .as_str()
        .or_else(|| message["dial_evidence"]["artifact"].as_str())
        .unwrap_or("")
        .to_owned()
}

/// Old sessions may lack execution metadata. Explicit failures and uncertain effects always win.
fn successful(message: &Value) -> bool {
    message["role"] == "tool"
        && message["dial_evidence"]["is_error"] != true
        && message["is_error"] != true
        && message["dial_effects"] != "unknown"
        && message["dial_execution"]["effects"] != "unknown"
        && message["dial_execution_status"]
            .as_str()
            .is_none_or(|status| status == "ok")
        && message["dial_execution"]["status"]
            .as_str()
            .is_none_or(|status| status == "ok")
}

/// LSP shares a tool name for queries and mutations; never classify by name alone.
fn observation_calls(messages: &[Value]) -> HashMap<String, Value> {
    let mut calls = HashMap::new();
    for call in messages
        .iter()
        .flat_map(|message| message["tool_calls"].as_array().into_iter().flatten())
    {
        let (Some(id), Some(name)) = (call["id"].as_str(), call["function"]["name"].as_str())
        else {
            continue;
        };
        if !OBSERVATIONS.contains(&name) {
            continue;
        }
        let arguments = match &call["function"]["arguments"] {
            Value::String(text) => serde_json::from_str::<Value>(text).ok(),
            value @ Value::Object(_) => Some(value.clone()),
            _ => None,
        };
        let Some(arguments) = arguments.filter(Value::is_object) else {
            continue;
        };
        if name == "lsp"
            && !matches!(
                arguments["action"].as_str(),
                Some(
                    "definition"
                        | "references"
                        | "hover"
                        | "implementation"
                        | "type_definition"
                        | "document_symbols"
                        | "workspace_symbols"
                        | "diagnostics"
                        | "rename_preview"
                        | "code_actions"
                        | "code_action_resolve"
                        | "status"
                        | "capabilities"
                )
            )
        {
            continue;
        }
        calls.insert(id.to_owned(), json!([name, arguments]));
    }
    calls
}

fn observation_key(message: &Value, calls: &HashMap<String, Value>) -> Option<String> {
    if let Some(key) = message["dial_evidence"]["observation"].as_str() {
        return Some(json!(["resource", key]).to_string());
    }
    // Untagged results need positive success evidence, a retained original and an exact payload match.
    // Do not guess equivalence from search terms, mutable paths or a hash collision.
    if message["dial_evidence"]["is_error"] != false
        || !message["dial_evidence"]["artifact"].is_string()
        || message.get("dial_images").is_some()
        || message.get("dial_provider_content").is_some()
        || estimate(message) < PRUNE_MIN_TOKENS
    {
        return None;
    }
    let call = calls.get(message["tool_call_id"].as_str()?)?;
    Some(json!(["exact", call, message["content"].as_str()?]).to_string())
}

/// Only exact read-only retries establish resolution here. A later arbitrary pass, shell output,
/// observation key, worker claim or unknown-effect result cannot discharge a diagnostic.
fn condense_resolved_failures(
    messages: &mut [Value],
    memo: &mut Supersession,
    policy: &Policy,
    suffix: &[usize],
    calls: &HashMap<String, Value>,
) {
    let mut successes: HashMap<String, String> = HashMap::new();
    for i in (0..messages.len()).rev() {
        let id = result_id(&messages[i]);
        let Some(call) = calls.get(&id) else { continue };
        let key = call.to_string();
        let explicit_success = messages[i]["dial_execution_status"] == "ok"
            || messages[i]["dial_execution"]["status"] == "ok";
        let known_effects = |message: &Value| {
            ["none", "known"].iter().any(|effect| {
                message["dial_effects"] == *effect
                    || message["dial_execution"]["effects"] == *effect
            })
        };
        if successful(&messages[i])
            && messages[i]["dial_evidence"]["is_error"] == false
            && explicit_success
            && known_effects(&messages[i])
        {
            successes.insert(key, id);
            continue;
        }
        let message = &mut messages[i];
        if message["role"] != "tool"
            || message["dial_evidence"]["is_error"] != true
            || !known_effects(message)
            || message["dial_effects"] == "unknown"
            || message["dial_execution"]["effects"] == "unknown"
            || message.get("dial_images").is_some()
            || message.get("dial_provider_content").is_some()
        {
            continue;
        }
        let Some(artifact) = message["dial_evidence"]["artifact"].as_str() else {
            continue;
        };
        let placeholder = memo.resolved.get(&id).cloned().or_else(|| {
            if !policy.drop_useless
                || !(policy.pressure || suffix[i] <= policy.warm_tokens)
                || suffix[i] <= policy.protect_tokens
            {
                return None;
            }
            let success = successes.get(&key)?;
            Some(format!("[Historical failed read-only attempt {id}; exact retry succeeded in tool result {success}. This does not verify other work. Original diagnostic: {artifact}; retrieve with artifact_read]"))
        });
        let Some(placeholder) = placeholder else {
            continue;
        };
        if !memo.resolved.contains_key(&id)
            && message["content"]
                .as_str()
                .is_none_or(|text| text.len() <= placeholder.len())
        {
            continue;
        }
        memo.resolved.insert(id, placeholder.clone());
        message["content"] = json!(placeholder);
    }
}

/// Under context pressure, replace old large observation results with a pointer to their full text. Done once per
/// result, for everything outside the newest `protect_tokens`, so the cached prefix is rewritten a single time at
/// the moment room is needed rather than shifting on every step.
fn prune(
    messages: &mut [Value],
    memo: &mut Supersession,
    suffix: &[usize],
    stale: &[bool],
    policy: &Policy,
    calls: &HashMap<String, Value>,
) {
    let Policy {
        pressure,
        protect_tokens,
        drop_useless,
        ..
    } = *policy;
    let eligible = |message: &Value| {
        successful(message)
            && message["dial_evidence"]["artifact"].is_string()
            && calls.contains_key(&result_id(message))
    };
    let fresh = |i: usize, message: &Value| {
        pressure && estimate(message) >= PRUNE_MIN_TOKENS && suffix[i] > protect_tokens
    };
    let batch: usize = messages
        .iter()
        .enumerate()
        .filter(|(i, message)| {
            (drop_useless || stale[*i])
                && eligible(message)
                && !memo.pruned.contains(&result_id(message))
                && fresh(*i, message)
        })
        .map(|(_, message)| estimate(message))
        .sum();
    let prune_new = batch >= PRUNE_BATCH_MIN_TOKENS;
    for i in 0..messages.len() {
        if messages[i]["role"] != "tool" {
            continue;
        }
        let id = result_id(&messages[i]);
        let already = memo.pruned.contains(&id);
        // Preserve failures even for read-only tools.
        if !successful(&messages[i]) {
            continue;
        }
        let Some(artifact) = messages[i]["dial_evidence"]["artifact"]
            .as_str()
            .map(str::to_owned)
        else {
            continue;
        };
        let Some(tool) = calls.get(&id).and_then(|call| call[0].as_str()) else {
            continue;
        };
        let bytes = messages[i]["content"].as_str().map_or(0, str::len);
        let worth_it = prune_new && fresh(i, &messages[i]) && (drop_useless || stale[i]);
        if !already && !worth_it {
            continue;
        }
        if !id.is_empty() {
            memo.pruned.insert(id);
        }
        messages[i]["content"] = json!(format!(
            "[Older {tool} result omitted to save context ({bytes} bytes). Full output: {artifact}; retrieve with artifact_read]"
        ));
        if let Some(object) = messages[i].as_object_mut() {
            object.remove("dial_images");
        }
    }
}
pub fn working_schema() -> Value {
    super::working_state::schema()
}
#[cfg(test)]
mod tests;

#[cfg(test)]
mod reminder_tests;

#[cfg(test)]
mod resolved_tests;
