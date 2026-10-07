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

/// Where compaction should cut: everything before the returned index is summarised, everything from it on
/// stays verbatim. Walks back from the newest message keeping about `tail_budget` tokens, so it works inside
/// a single long user turn (one request, many tool steps) where there is no earlier user message to cut at.
/// The cut never lands on a tool result (its assistant call stays with it) and always leaves the newest
/// message in the tail. `None` when there is nothing before the cut worth summarising.
pub(super) fn token_cut(messages: &[Value], tail_budget: usize) -> Option<usize> {
    if messages.len() < 3 {
        return None;
    }
    let (mut kept, mut cut) = (0usize, messages.len());
    for i in (0..messages.len()).rev() {
        let tokens = estimate(&messages[i]);
        if cut < messages.len() && kept + tokens > tail_budget {
            break;
        }
        kept += tokens;
        cut = i;
    }
    while cut > 0 && messages[cut]["role"] == "tool" {
        cut -= 1;
    }
    (cut > 0).then_some(cut)
}

/// Which results were already replaced, by supersession or by pruning. A result is replaced at most once:
/// deciding again every step from a suffix that only ever grows would flip it back and forth, and each flip
/// rewrites the conversation prefix a provider or local server may have cached.
#[derive(Default)]
pub struct Supersession {
    replaced: HashSet<String>,
    pruned: HashSet<String>,
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
}

#[cfg(test)]
impl Policy {
    fn calm() -> Self {
        Policy {
            warm_tokens: 10_000,
            pressure: false,
            protect_tokens: usize::MAX,
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
        let supersede = memo.replaced.contains(&id) || pressure || suffix[i] <= warm_tokens;
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
    prune(messages, memo, &suffix, pressure, protect_tokens);
    for message in messages {
        *message = crate::llm::messages::canonical(message);
    }
}

/// Tools whose results are observations of the project, not changes to it, and whose full text is retrievable from
/// an artifact. Mutation results, todo and memory updates, and `artifact_read` itself are never dropped.
const OBSERVATIONS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "find",
    "bash",
    "lsp",
    "ast_search",
    "web_fetch",
    "web_search",
];
/// A result smaller than this is not worth a placeholder and a retrieval round trip.
const PRUNE_MIN_TOKENS: usize = 1_000;

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
    for i in 0..messages.len() {
        if messages[i]["role"] != "tool" {
            continue;
        }
        let id = result_id(&messages[i]);
        let already = memo.pruned.contains(&id);
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
        let worth_it =
            pressure && estimate(&messages[i]) >= PRUNE_MIN_TOKENS && suffix[i] > protect_tokens;
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
    fn token_cut_works_inside_one_user_turn_and_never_splits_a_tool_pair() {
        // One user request, then a long chain of call/result pairs: the shape that has no earlier user message.
        let mut m = vec![json!({"role":"user","content":"do the task"})];
        for i in 0..20 {
            m.push(json!({"role":"assistant","content":"","tool_calls":[{"id":format!("c{i}"),"function":{"name":"read","arguments":"{}"}}]}));
            m.push(
                json!({"role":"tool","tool_call_id":format!("c{i}"),"content":"x".repeat(4000)}),
            );
        }
        let cut = token_cut(&m, 3_000).expect("a long single turn must be compactable");
        assert!(cut > 1 && cut < m.len() - 1, "cut {cut} of {}", m.len());
        assert_ne!(
            m[cut]["role"], "tool",
            "the tail may not start on an orphaned tool result"
        );
        assert!(
            m[cut..].iter().map(estimate).sum::<usize>() <= 3_000 + 1_100,
            "tail stays near the budget"
        );
        assert_eq!(
            token_cut(&m[..2], 3_000),
            None,
            "nothing worth summarising in a two-message history"
        );
    }
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
            ("old-grep", "grep", 20_000),
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
        };
        project(&mut m, &mut Supersession::default(), &policy);
        assert!(
            content(&m, "old-grep").starts_with("[Older grep result omitted")
                && content(&m, "old-grep").contains("artifact://old-grep")
                && content(&m, "old-grep").contains("20000 bytes")
        );
        assert!(content(&m, "old-bash").starts_with("[Older bash result omitted"));
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
    fn nothing_is_dropped_without_pressure() {
        let mut m = history();
        project(
            &mut m,
            &mut Supersession::default(),
            &Policy {
                warm_tokens: 0,
                pressure: false,
                protect_tokens: 4_000,
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
            },
        );
        assert_eq!(
            content(&pressed, "old-grep"),
            content(&eased, "old-grep"),
            "flipping back would rewrite the cached prefix"
        );
    }
    #[test]
    fn a_dropped_read_that_is_later_repeated_keeps_its_first_placeholder() {
        let mut memo = Supersession::default();
        let mut m = vec![json!({"role":"user","content":"task"})];
        let mut first = exchange("r1", "read", 20_000);
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
            },
        );
        assert_eq!(
            content(&again, "r1"),
            first_placeholder,
            "the repeat must not turn the placeholder into a different one"
        );
    }
}
