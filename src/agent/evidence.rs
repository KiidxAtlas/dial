//! Model-context projection leaves append-only evidence untouched on disk.
use serde_json::{Value, json};
use std::collections::HashSet;
/// Estimated tokens of one message (bytes/4): enough to compare against a budget, not to bill.
pub(super) fn estimate(message: &Value) -> usize {
    (message["content"].as_str().map_or(0, str::len)
        + message["tool_calls"]
            .as_array()
            .map_or(0, |calls| calls.iter().map(|c| c.to_string().len()).sum()))
        / 4
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
    let mut running = 0;
    for i in (0..messages.len()).rev() {
        suffix[i] = running;
        running += estimate(&messages[i]);
    }
    let mut latest: HashSet<String> = HashSet::new();
    for i in (0..messages.len()).rev() {
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
        let id = result_id(&messages[i]);
        // A result already dropped for space keeps that one placeholder; rewriting it would change cached text.
        if memo.pruned.contains(&id) {
            continue;
        }
        let supersede = memo.replaced.contains(&id)
            || (supersede_reads && (pressure || suffix[i] <= warm_tokens));
        if !supersede {
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
    if drop_useless {
        prune(messages, memo, &suffix, pressure, protect_tokens);
    }
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
    pressure: bool,
    protect_tokens: usize,
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
            eligible(message, &tool_of)
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
        let worth_it = prune_new && fresh(i, &messages[i]);
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
    json!({"type":"function","function":{"name":"working_state","description":"View or replace durable working state. Preserve the user's goal, decisions, constraints and open work across compaction; claims remain unverified unless linked to evidence.","parameters":{"type":"object","properties":{"action":{"type":"string","enum":["view","update"]},"goal":{"type":"string"},"decisions":{"type":"array","items":{"type":"string"}},"constraints":{"type":"array","items":{"type":"string"}},"open_work":{"type":"array","items":{"type":"string"}},"evidence":{"type":"array","items":{"type":"string"}}},"required":["action"],"additionalProperties":false}}})
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_mutations_and_replaces_only_superseded_read() {
        let mut m = vec![
            obs("1", "old", "a:1-5"),
            json!({"role":"tool","tool_call_id":"2","content":"changed file"}),
            obs("3", "new", "a:1-5"),
        ];
        project(&mut m, &mut Supersession::default(), &Policy::calm());
        assert!(m[0]["content"].as_str().unwrap().contains("Superseded"));
        assert_eq!(m[1]["content"], "changed file");
        assert_eq!(m[2]["content"], "new");
    }
    fn obs(id: &str, text: &str, key: &str) -> Value {
        json!({"role":"tool","tool_call_id":id,"content":text,"dial_evidence":{"observation":key,"artifact":format!("artifact://{id}")}})
    }
    #[test]
    fn a_placeholder_never_changes_after_it_is_written() {
        // A moving "newer result at message N" pointer would rewrite every older placeholder on each re-read and
        // invalidate the provider's prompt-prefix cache from the oldest one onward.
        let mut memo = Supersession::default();
        let mut first = vec![obs("1", "v1", "k"), obs("2", "v2", "k")];
        project(&mut first, &mut memo, &Policy::calm());
        let mut later = vec![
            obs("1", "v1", "k"),
            obs("2", "v2", "k"),
            json!({"role":"assistant","content":"x"}),
            obs("3", "v3", "k"),
        ];
        project(&mut later, &mut memo, &Policy::calm());
        assert_eq!(first[0]["content"], later[0]["content"]);
        assert!(
            later[1]["content"]
                .as_str()
                .unwrap()
                .starts_with("[Superseded")
        );
    }
    #[test]
    fn a_read_far_behind_a_large_warm_suffix_stays_until_context_pressure() {
        let big = "x".repeat(40_000);
        let conversation = || {
            vec![
                obs("1", "old", "k"),
                json!({"role":"assistant","content":big.clone()}),
                obs("2", "new", "k"),
            ]
        };
        let mut memo = Supersession::default();
        let policy = |pressure| Policy {
            warm_tokens: 1_000,
            pressure,
            protect_tokens: usize::MAX,
            supersede_reads: true,
            drop_useless: true,
        };
        let mut calm = conversation();
        project(&mut calm, &mut memo, &policy(false));
        assert_eq!(
            calm[0]["content"], "old",
            "superseding would invalidate ~10k cached tokens for little saving"
        );
        let mut pressed = conversation();
        project(&mut pressed, &mut memo, &policy(true));
        assert!(
            pressed[0]["content"]
                .as_str()
                .unwrap()
                .contains("Superseded")
        );
        // And once superseded it must not flip back when pressure eases.
        let mut eased = conversation();
        project(&mut eased, &mut memo, &policy(false));
        assert!(eased[0]["content"].as_str().unwrap().contains("Superseded"));
    }
    #[test]
    fn recall_is_folded_into_its_own_user_message_not_inserted_before_the_conversation() {
        let mut m = vec![
            json!({"role":"user","content":"question","dial_recall":"NOTE: fact"}),
            json!({"role":"assistant","content":"a"}),
            json!({"role":"user","content":"later"}),
        ];
        project(&mut m, &mut Supersession::default(), &Policy::calm());
        assert_eq!(m.len(), 3);
        assert_eq!(m[0]["content"], "question\n\nNOTE: fact");
        assert!(m[0].get("dial_recall").is_none());
        assert_eq!(m[2]["content"], "later");
    }
    fn exchange(id: &str, tool: &str, size: usize) -> Vec<Value> {
        vec![
            json!({"role":"assistant","content":"","tool_calls":[{"id":id,"function":{"name":tool,"arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":id,"content":"y".repeat(size),"dial_evidence":{"artifact":format!("artifact://{id}")}}),
        ]
    }
    fn history() -> Vec<Value> {
        let mut m = vec![json!({"role":"user","content":"task"})];
        for (id, tool, size) in [
            // 10k estimated tokens: on its own enough to clear the prune batch threshold.
            ("old-grep", "grep", 40_000),
            ("old-edit", "edit", 20_000),
            ("old-small", "grep", 400),
            ("old-bash", "bash", 20_000),
            ("new-read", "read", 20_000),
        ] {
            m.extend(exchange(id, tool, size));
        }
        m
    }
    fn content<'a>(m: &'a [Value], id: &str) -> &'a str {
        m.iter().find(|x| x["tool_call_id"] == id).unwrap()["content"]
            .as_str()
            .unwrap()
    }
    #[test]
    fn pressure_drops_old_large_observations_but_never_edits_small_results_or_the_recent_tail() {
        let mut m = history();
        let policy = Policy {
            warm_tokens: 0,
            pressure: true,
            protect_tokens: 4_000,
            supersede_reads: true,
            drop_useless: true,
        };
        project(&mut m, &mut Supersession::default(), &policy);
        assert!(
            content(&m, "old-grep").starts_with("[Older grep result omitted")
                && content(&m, "old-grep").contains("artifact://old-grep")
                && content(&m, "old-grep").contains("40000 bytes")
        );
        assert_eq!(
            content(&m, "old-bash").len(),
            20_000,
            "shell output may be mutation or verification evidence"
        );
        assert_eq!(
            content(&m, "old-edit").len(),
            20_000,
            "a mutation's evidence stays verbatim"
        );
        assert_eq!(
            content(&m, "old-small").len(),
            400,
            "too small to be worth a placeholder"
        );
        assert_eq!(
            content(&m, "new-read").len(),
            20_000,
            "the newest results are inside the protected tail"
        );
    }
    #[test]
    fn pressure_preserves_error_diagnostics_and_artifactless_results() {
        let mut m = exchange("failed", "grep", 20_000);
        m[1]["dial_evidence"]["is_error"] = json!(true);
        m.extend(exchange("unretained", "read", 20_000));
        m[3].as_object_mut().unwrap().remove("dial_evidence");
        m.push(json!({"role":"user","content":"tail"}));
        project(
            &mut m,
            &mut Supersession::default(),
            &Policy {
                warm_tokens: 0,
                pressure: true,
                protect_tokens: 0,
                supersede_reads: true,
                drop_useless: true,
            },
        );
        assert_eq!(content(&m, "failed").len(), 20_000);
        assert_eq!(content(&m, "unretained").len(), 20_000);
    }

    #[test]
    fn nothing_is_dropped_without_pressure() {
        let mut m = history();
        project(
            &mut m,
            &mut Supersession::default(),
            &Policy {
                warm_tokens: 0,
                pressure: false,
                protect_tokens: 4_000,
                supersede_reads: true,
                drop_useless: true,
            },
        );
        assert!(
            m.iter()
                .filter(|x| x["role"] == "tool")
                .all(|x| !x["content"].as_str().unwrap().starts_with("[Older"))
        );
    }
    #[test]
    fn a_dropped_result_stays_dropped_and_byte_identical_after_pressure_eases() {
        let mut memo = Supersession::default();
        let mut pressed = history();
        project(
            &mut pressed,
            &mut memo,
            &Policy {
                warm_tokens: 0,
                pressure: true,
                protect_tokens: 4_000,
                supersede_reads: true,
                drop_useless: true,
            },
        );
        let mut eased = history();
        project(
            &mut eased,
            &mut memo,
            &Policy {
                warm_tokens: 0,
                pressure: false,
                protect_tokens: 4_000,
                supersede_reads: true,
                drop_useless: true,
            },
        );
        assert_eq!(
            content(&pressed, "old-grep"),
            content(&eased, "old-grep"),
            "flipping back would rewrite the cached prefix"
        );
    }
    #[test]
    fn small_prunes_wait_until_together_they_free_a_whole_batch() {
        // Each candidate is 5k estimated tokens, below PRUNE_BATCH_MIN_TOKENS. Pruning one per step
        // as it crossed the protected tail would rewrite the cached prefix on nearly every step.
        let policy = Policy {
            warm_tokens: 0,
            pressure: true,
            protect_tokens: 4_000,
            supersede_reads: true,
            drop_useless: true,
        };
        let mut memo = Supersession::default();
        let mut m = vec![json!({"role":"user","content":"task"})];
        m.extend(exchange("a", "grep", 20_000));
        m.extend(exchange("tail", "read", 20_000));
        let mut first = m.clone();
        project(&mut first, &mut memo, &policy);
        assert_eq!(
            content(&first, "a").len(),
            20_000,
            "one 5k candidate is held until the batch is worth a cache break"
        );

        let mut m = vec![json!({"role":"user","content":"task"})];
        m.extend(exchange("a", "grep", 20_000));
        m.extend(exchange("b", "grep", 20_000));
        m.extend(exchange("tail", "read", 20_000));
        project(&mut m, &mut memo, &policy);
        assert!(content(&m, "a").starts_with("[Older grep result omitted"));
        assert!(content(&m, "b").starts_with("[Older grep result omitted"));
        assert_eq!(
            content(&m, "tail").len(),
            20_000,
            "the protected tail is never part of a batch"
        );
    }
    #[test]
    fn a_dropped_read_that_is_later_repeated_keeps_its_first_placeholder() {
        let mut memo = Supersession::default();
        let mut m = vec![json!({"role":"user","content":"task"})];
        // Large enough (10k estimated tokens) to be pruned as its own batch.
        let mut first = exchange("r1", "read", 40_000);
        first[1]["dial_evidence"]["observation"] = json!("k");
        m.extend(first);
        m.extend(exchange("pad", "grep", 30_000));
        project(
            &mut m.clone(),
            &mut memo,
            &Policy {
                warm_tokens: 0,
                pressure: true,
                protect_tokens: 6_000,
                supersede_reads: true,
                drop_useless: true,
            },
        );
        let before = m.clone();
        let mut projected = before.clone();
        project(
            &mut projected,
            &mut memo,
            &Policy {
                warm_tokens: 0,
                pressure: false,
                protect_tokens: 6_000,
                supersede_reads: true,
                drop_useless: true,
            },
        );
        let first_placeholder = content(&projected, "r1").to_owned();
        assert!(first_placeholder.starts_with("[Older read result omitted"));
        let mut again = before;
        let mut repeat = exchange("r2", "read", 20_000);
        repeat[1]["dial_evidence"]["observation"] = json!("k");
        again.extend(repeat);
        project(
            &mut again,
            &mut memo,
            &Policy {
                warm_tokens: 0,
                pressure: false,
                protect_tokens: 6_000,
                supersede_reads: true,
                drop_useless: true,
            },
        );
        assert_eq!(
            content(&again, "r1"),
            first_placeholder,
            "the repeat must not turn the placeholder into a different one"
        );
    }
}

#[cfg(test)]
mod reminder_tests {
    use super::*;
    #[test]
    fn identical_tagged_notices_shrink_but_user_text_is_untouched() {
        let text = "diagnostic ".repeat(100);
        let user = json!({"role":"user","content":text});
        let notice = json!({"role":"user","content":text,"dial_reminder":"reasoning"});
        let mut messages = vec![user.clone(), notice.clone(), notice.clone()];
        project(
            &mut messages,
            &mut Supersession::default(),
            &Policy {
                warm_tokens: 0,
                pressure: false,
                protect_tokens: 0,
                supersede_reads: true,
                drop_useless: true,
            },
        );
        assert_eq!(
            messages,
            vec![user.clone(), notice.clone(), notice.clone()],
            "calm prefixes remain reusable"
        );
        project(
            &mut messages,
            &mut Supersession::default(),
            &Policy {
                warm_tokens: 0,
                pressure: true,
                protect_tokens: 0,
                supersede_reads: true,
                drop_useless: true,
            },
        );
        assert_eq!(messages[0], user);
        assert!(messages[1]["content"].as_str().unwrap().len() < 100);
        assert_eq!(messages[2], notice);
    }
}
