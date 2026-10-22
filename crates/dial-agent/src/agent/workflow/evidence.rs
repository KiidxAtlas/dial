//! Model-context projection leaves append-only evidence untouched on disk.
use serde_json::{Value, json};
use std::collections::HashSet;
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
}

impl Supersession {
    /// Whether the model still sees this result in full: it was neither replaced nor pruned.
    pub fn holds_full(&self, id: &str) -> bool {
        !self.replaced.contains(id) && !self.pruned.contains(id)
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
///  * a successful observation of a resource+range that a later result repeats becomes a short placeholder,
///    but only while the text after it is small (`warm_tokens`) or the context is under `pressure`. Replacing a
///    read buried under a long warm suffix would force that whole suffix to be prefilled again for little saving.
///    Mutations and errors stay verbatim, and the placeholder text never changes once written.
pub fn project(messages: &mut [Value], memo: &mut Supersession, policy: &Policy) {
    let Policy {
        warm_tokens,
        pressure,
        protect_tokens,
        supersede_reads,
        drop_useless,
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
            if !reminders.insert(key) && (pressure || warm_tokens > 0) {
                message["content"] =
                    json!("[Repeated harness notice; identical guidance retained later]");
            }
        }
    }
    let mut suffix = vec![0usize; messages.len()];
    let mut running = 0usize;
    for i in (0..messages.len()).rev() {
        suffix[i] = running;
        running = running.saturating_add(estimate(&messages[i]));
    }
    let mut latest: HashSet<String> = HashSet::new();
    let mut stale = vec![false; messages.len()];
    for i in (0..messages.len()).rev() {
        // An unsuccessful repeat neither supersedes earlier success nor loses its diagnostics.
        if messages[i]["dial_evidence"]["is_error"] == true {
            continue;
        }
        let Some(key) = messages[i]["dial_evidence"]["observation"]
            .as_str()
            .map(str::to_owned)
        else {
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
        let supersede = memo.replaced.contains(&id)
            || (supersede_reads && (pressure || suffix[i] <= warm_tokens));
        if !supersede || !messages[i]["dial_evidence"]["artifact"].is_string() {
            continue;
        }
        if !id.is_empty() {
            memo.replaced.insert(id);
        }
        let artifact = messages[i]["dial_evidence"]["artifact"]
            .as_str()
            .unwrap_or("unavailable");
        messages[i]["content"] = json!(format!(
            "[Superseded observation: a newer result for the same resource appears later in this conversation. Original evidence: {artifact}]"
        ));
        if let Some(object) = messages[i].as_object_mut() {
            object.remove("dial_images");
        }
    }
    prune(
        messages,
        memo,
        &suffix,
        &stale,
        pressure,
        protect_tokens,
        drop_useless,
    );
    for message in messages {
        crate::llm::messages::canonicalize(message);
    }
}

/// Tools whose results are observations of the project, not changes to it, and whose full text is retrievable from
/// an artifact. Mutation results, todo and memory updates, and `artifact_read` itself are never dropped.
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

/// Under context pressure, replace old large observation results with a pointer to their full text. Done once per
/// result, for everything outside the newest `protect_tokens`, so the cached prefix is rewritten a single time at
/// the moment room is needed rather than shifting on every step.
fn prune(
    messages: &mut [Value],
    memo: &mut Supersession,
    suffix: &[usize],
    stale: &[bool],
    pressure: bool,
    protect_tokens: usize,
    drop_useless: bool,
) {
    let mut tool_of: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for message in messages.iter() {
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            if let (Some(id), Some(name)) = (call["id"].as_str(), call["function"]["name"].as_str())
            {
                tool_of.insert(id.to_owned(), name.to_owned());
            }
        }
    }
    let eligible = |message: &Value, tool_of: &std::collections::HashMap<String, String>| {
        message["role"] == "tool"
            && message["dial_evidence"]["is_error"] != true
            && message["dial_evidence"]["artifact"].is_string()
            && tool_of
                .get(&result_id(message))
                .is_some_and(|tool| OBSERVATIONS.contains(&tool.as_str()))
    };
    let fresh = |i: usize, message: &Value| {
        pressure && estimate(message) >= PRUNE_MIN_TOKENS && suffix[i] > protect_tokens
    };
    let batch: usize = messages
        .iter()
        .enumerate()
        .filter(|(i, message)| {
            (drop_useless || stale[*i])
                && eligible(message, &tool_of)
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
        if messages[i]["dial_evidence"]["is_error"] == true {
            continue;
        }
        let Some(artifact) = messages[i]["dial_evidence"]["artifact"]
            .as_str()
            .map(str::to_owned)
        else {
            continue;
        };
        let Some(tool) = tool_of
            .get(&id)
            .filter(|t| OBSERVATIONS.contains(&t.as_str()))
            .cloned()
        else {
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
