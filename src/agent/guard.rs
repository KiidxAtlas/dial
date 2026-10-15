//! Loop-level safeguards that keep a weak or confused model productive: how tool results enter
//! the conversation, what to do when a generation runs out of output tokens, and noticing a model
//! that repeats a failing call.

use crate::llm::{Reply, ToolCall};

/// Ignore only top-level presentation metadata, never nested tool payload fields.
/// Bash timeouts affect waiting, not the identity of the requested command.
pub(super) fn normalized_arguments(call: &ToolCall) -> serde_json::Value {
    let mut args = call.arguments.clone();
    if let Some(object) = args.as_object_mut() {
        object.remove("i");
        object.remove("_intent");
        if call.name == "bash" {
            object.remove("timeout");
            if let Some(command) = object.get("command").and_then(|v| v.as_str())
                // Expansions and escapes depend on quoting; keep them exact.
                // This fingerprint never changes the executed command.
                && !command.contains(['*', '?', '[', ']', '~', '#', '!', '\\'])
                && let Some(words) = super::approval::shell_words(command)
            {
                object.insert("command".into(), serde_json::json!(words));
            }
        }
    }
    args
}

/// A tool call repeated with identical arguments across consecutive assistant turns.
/// Stateless: the streak is recounted from the request's own history each
/// turn, so nothing persists across sessions and a redirect never has to be stored.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct RepeatedToolCall {
    pub tool_name: String,
    pub count: usize,
    pub arguments_summary: String,
    pub result_summary: String,
}

/// Argument identity stays stable by dropping the intent field (`i`, legacy `__intent`) at
/// every depth and sorting keys; the result is the fingerprint of one call.
fn canonical_arguments(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonical_arguments).collect()),
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<&String, &Value> = map.iter().collect();
            sorted
                .into_iter()
                .filter(|(key, _)| key.as_str() != "i" && key.as_str() != "__intent")
                .map(|(key, value)| (key.clone(), canonical_arguments(value)))
                .collect::<serde_json::Map<String, Value>>()
                .into()
        }
        other => other.clone(),
    }
}

fn call_arguments(function: &serde_json::Value) -> serde_json::Value {
    match function["arguments"].as_str() {
        Some(raw) => {
            serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_owned()))
        }
        None => function["arguments"].clone(),
    }
}

/// Collapses whitespace and truncates on a char boundary.
fn summarize_text(text: &str, limit: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > limit {
        format!("{}…", collapsed.chars().take(limit).collect::<String>())
    } else {
        collapsed
    }
}

impl RepeatedToolCall {
    /// Walks assistant turns from the newest backwards. A turn with no calls, or one whose calls are
    /// all exempt, ends the streak. Identical canonical call sets extend it; the first difference ends it.
    pub(super) fn detect(
        messages: &[serde_json::Value],
        threshold: usize,
        exempt: &[&str],
    ) -> Option<Self> {
        let mut streak_key: Option<String> = None;
        let mut count = 0usize;
        let mut newest: Vec<(String, String, serde_json::Value)> = Vec::new();
        for message in messages.iter().rev() {
            if message["role"].as_str() != Some("assistant") {
                continue;
            }
            let Some(calls) = message["tool_calls"]
                .as_array()
                .filter(|calls| !calls.is_empty())
            else {
                break;
            };
            let parsed: Vec<(String, String, serde_json::Value)> = calls
                .iter()
                .map(|call| {
                    let name = call["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    let id = call["id"].as_str().unwrap_or_default().to_owned();
                    (id, name, call_arguments(&call["function"]))
                })
                .collect();
            if parsed
                .iter()
                .all(|(_, name, _)| exempt.contains(&name.as_str()))
            {
                break;
            }
            let mut parts: Vec<String> = parsed
                .iter()
                .map(|(_, name, args)| {
                    serde_json::json!([name, canonical_arguments(args)]).to_string()
                })
                .collect();
            parts.sort();
            let key = serde_json::to_string(&parts).unwrap_or_default();
            match &streak_key {
                Some(existing) if *existing != key => break,
                Some(_) => count += 1,
                None => {
                    streak_key = Some(key);
                    count = 1;
                    newest = parsed;
                }
            }
        }
        if count < threshold || count == 0 {
            return None;
        }
        let (id, name, args) = newest
            .iter()
            .find(|(_, name, _)| !exempt.contains(&name.as_str()))
            .unwrap_or(&newest[0])
            .clone();
        let result_summary = messages
            .iter()
            .rev()
            .find(|message| {
                message["role"].as_str() == Some("tool")
                    && message["tool_call_id"].as_str() == Some(id.as_str())
            })
            .map(|message| match &message["content"] {
                serde_json::Value::String(text) => summarize_text(text, 200),
                serde_json::Value::Null => String::new(),
                other => summarize_text(&other.to_string(), 200),
            })
            .unwrap_or_default();
        Some(Self {
            tool_name: name,
            count,
            arguments_summary: summarize_text(&canonical_arguments(&args).to_string(), 400),
            result_summary,
        })
    }
}

#[cfg(test)]
mod repeated_tool_call_tests {
    use super::*;
    use serde_json::json;

    fn turn(id: &str, name: &str, args: &str) -> Vec<serde_json::Value> {
        vec![
            json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":args}}]}),
            json!({"role":"tool","tool_call_id":id,"content":"same   result"}),
        ]
    }

    #[test]
    fn five_identical_turns_trip_with_the_intent_field_ignored() {
        let mut messages = Vec::new();
        for n in 0..5 {
            messages.extend(turn(
                &format!("c{n}"),
                "read",
                &format!("{{\"path\":\"a\",\"i\":\"step {n}\"}}"),
            ));
        }
        let found =
            RepeatedToolCall::detect(&messages, 5, &["wait"]).expect("five identical turns trip");
        assert_eq!(found.tool_name, "read");
        assert_eq!(found.count, 5);
        assert_eq!(found.result_summary, "same result");
        assert!(RepeatedToolCall::detect(&messages, 6, &["wait"]).is_none());
    }

    #[test]
    fn a_different_turn_or_an_exempt_turn_breaks_the_streak() {
        let mut messages = Vec::new();
        for n in 0..4 {
            messages.extend(turn(&format!("c{n}"), "read", "{\"path\":\"a\"}"));
        }
        messages.extend(turn("x", "read", "{\"path\":\"b\"}"));
        assert!(RepeatedToolCall::detect(&messages, 5, &["wait"]).is_none());

        let mut waits = Vec::new();
        for n in 0..6 {
            waits.extend(turn(&format!("w{n}"), "wait", "{}"));
        }
        assert!(RepeatedToolCall::detect(&waits, 5, &["wait"]).is_none());
    }
}

/// Repeated narration can disguise a loop even when the model varies its calls.
/// This is feedback, not a stop condition: repeated wording alone does not prove
/// the tools failed to make progress. Result/failure guards enforce those stops.
#[derive(Default)]
pub(super) struct NarrationGuard {
    seen: std::collections::VecDeque<(u64, u32)>,
    promises: std::collections::HashMap<&'static str, u32>,
}

impl NarrationGuard {
    pub fn observe(&mut self, reply: &Reply) -> Option<String> {
        let lower = reply
            .text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        let promise = ["i'll", "i will", "next", "now", "going to"]
            .iter()
            .any(|marker| {
                lower.starts_with(&format!("{marker} ")) || lower.contains(&format!(" {marker} "))
            });
        if promise && !reply.tool_calls.is_empty() {
            let words: Vec<_> = lower.split(|c: char| !c.is_alphabetic()).collect();
            let category = if ["verify", "verification", "check", "test"]
                .iter()
                .any(|word| words.contains(word))
            {
                Some("verification")
            } else if ["inspect", "read", "search"]
                .iter()
                .any(|word| words.contains(word))
            {
                Some("inspection")
            } else {
                None
            };
            if let Some(category) = category {
                let count = self.promises.entry(category).or_default();
                *count = count.saturating_add(1);
                if *count >= 3 && (*count).is_multiple_of(3) {
                    return Some(format!(
                        "[Loop check from Dial] You have promised {category} {count} times without a confirmed edit or task closure. Rewording the promise is not progress. Reuse returned evidence and take the concrete next action, or report the blocker. Do not repeat the promise to check again."
                    ));
                }
            }
        }
        use std::hash::{Hash, Hasher};
        let text = reply.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if reply.tool_calls.is_empty() || text.len() < 64 {
            return None;
        }
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hash);
        let key = hash.finish();
        if let Some((_, count)) = self.seen.iter_mut().find(|(seen, _)| *seen == key) {
            *count = count.saturating_add(1);
            return (*count >= 3 && (*count).is_multiple_of(3)).then(|| format!("[Loop check from Dial] You have repeated the same explanation {count} times this turn, even if the tool arguments differ. Reuse current evidence and choose a different concrete action or report the blocker. If the repeated action is necessary, identify the changed state or uncertainty it checks."));
        }
        if self.seen.len() == 32 {
            self.seen.pop_front();
        }
        self.seen.push_back((key, 1));
        None
    }

    pub fn progress(&mut self) {
        self.seen.clear();
        self.promises.clear();
    }
}

/// A tool call that failed this many times with identical arguments earns a warning.
const REPEAT_LIMIT: u32 = 3;
const FAILURE_LIMIT: u32 = 4;
/// Length stops answered with a nudge (after the budget cannot grow) before the turn gives up.
pub const MAX_LENGTH_NUDGES: u32 = 2;
/// Hard ceiling for a raised per-call output budget.
const MAX_RAISED_BUDGET: u32 = 32768;
/// Context kept free for the system prompt growth and the next tool result when raising the budget.
const CONTEXT_MARGIN: u64 = 1024;

/// Keep the beginning and the end of an oversized tool result and say how much was dropped. The end
/// matters: build and test verdicts, exit codes and "continue with ..." footers are printed last.
pub fn clip_middle(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut head_end = max / 3;
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - (max - max / 3);
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}\n[... {} of {} bytes omitted from the middle; fetch what you need with a narrower call (read a line range, grep, or pipe through head/tail) ...]\n{}",
        &text[..head_end],
        tail_start - head_end,
        text.len(),
        &text[tail_start..]
    )
}

/// Clip a tool result for the model. Like [`clip_middle`], but diagnostic lines (errors, panics,
/// failed tests; see [`crate::tools::diagnostics`]) from the omitted middle are kept between the two
/// ends, within a quarter of `max`, so a long build log still shows the first error and its location.
pub fn clip_evidence(text: &str, max: usize) -> String {
    use crate::tools::diagnostics;
    if text.len() <= max {
        return text.to_owned();
    }
    let floor = |mut at: usize| {
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    let head_end = floor(max / 4);
    let tail_start = floor(text.len() - max / 2);
    let budget = max - max / 4 - max / 2;
    let middle = &text[head_end..tail_start];
    let (mut kept, mut lines, mut overflow) = (String::new(), 0usize, 0usize);
    for line in middle
        .lines()
        .filter(|line| diagnostics::is_diagnostic(line))
    {
        let line = diagnostics::shorten(line);
        if kept.len() + line.len() + 1 > budget {
            overflow += 1;
            continue;
        }
        kept.push_str(&line);
        kept.push('\n');
        lines += 1;
    }
    if lines == 0 {
        return clip_middle(text, max);
    }
    let more = if overflow > 0 {
        format!(" ({overflow} more not shown)")
    } else {
        String::new()
    };
    format!(
        "{}\n[... {} of {} bytes omitted from the middle; {lines} diagnostic line(s) from it follow{more}. Fetch other parts with a narrower call ...]\n{kept}[... end of kept diagnostics ...]\n{}",
        &text[..head_end],
        tail_start - head_end,
        text.len(),
        &text[tail_start..]
    )
}

/// Detects the same tool call failing again and again, which a confused model does until its step
/// budget is gone.
#[derive(Default)]
pub struct RepeatGuard {
    failures: std::collections::HashMap<String, u32>,
}

impl RepeatGuard {
    fn key(call: &ToolCall) -> String {
        // Rephrasing a completion claim is not recovery from a rejected step closure.
        if call.name == "todo" && call.arguments["op"] == "done" {
            return format!("todo\u{0}done\u{0}{}", call.arguments["index"]);
        }
        // Rewording task descriptions cannot fix a plan containing precompleted investigations.
        if call.name == "todo"
            && matches!(call.arguments["op"].as_str(), Some("set" | "append"))
            && call.arguments["tasks"].as_array().is_some_and(|tasks| {
                tasks
                    .iter()
                    .any(|task| task["investigation"] == true && task["status"] == "completed")
            })
        {
            return "todo\u{0}precompleted-investigation".into();
        }
        format!("{}\u{0}{}", call.name, normalized_arguments(call))
    }
    /// Keep failures across intervening reads. Only success for this exact call resets its count.
    pub fn observe(&mut self, call: &ToolCall, failed: bool) -> Option<String> {
        let key = Self::key(call);
        if !failed {
            self.failures.remove(&key);
            return None;
        }
        let count = self.failures.entry(key).or_default();
        *count += 1;
        if call.name == "read" {
            return Some(format!(
                "[Read recovery: this exact call has now failed {count} times with the same arguments. Use the diagnostic to correct the path or numeric selector (offset/limit is also supported), or locate the file once with a scoped glob/grep. Reuse context already returned and request only missing lines. Do not retry unchanged, even after other reads; continue the task with the corrected call.]"
            ));
        }
        (*count >= REPEAT_LIMIT).then(|| {
            format!(
                "[This exact call has now failed {count} times with the same arguments this turn. Do not issue it again unchanged: change the arguments or the approach. Reading another file does not reset this count; the turn stops after {FAILURE_LIMIT} identical failures.]"
            )
        })
    }

    pub fn stop_message(&self, call: &ToolCall) -> Option<String> {
        let count = *self.failures.get(&Self::key(call))?;
        if call.name == "todo" && call.arguments["op"] == "done" && count >= FAILURE_LIMIT {
            return Some(format!(
                "Stopped: `todo done` for the same target failed {count} times this turn. Rewording the completion claim is not recovery. Deliver the report, satisfy the reported check, or block the step with a concrete reason before retrying."
            ));
        }
        (count >= FAILURE_LIMIT).then(|| format!(
            "Stopped: `{}` with the same arguments failed {count} times this turn. The model did not correct the call after feedback. Change the call or editing mode, or use a different model before retrying.", call.name
        ))
    }
}
/// Steps a turn may spend. A step is a model call that did work; when a reply is unusable (cut off, unreadable
/// call, a tool call written as plain text) the retry is refunded, up to `free` times, so a few bad generations do
/// not eat the budget meant for real work. Evaluation runs get no refunds: their budget is what they measure.
pub struct StepBudget {
    max: u32,
    spent: u32,
    free: u32,
}

impl StepBudget {
    /// Refunds a normal turn may use. Bounded so a model that can only produce unusable replies still stops.
    pub const FREE_RETRIES: u32 = 8;

    pub fn new(max: u32, free: u32) -> Self {
        StepBudget {
            max,
            spent: 0,
            free,
        }
    }

    /// Start a step; false once a finite budget is used up. Zero means unlimited.
    pub fn begin(&mut self) -> bool {
        if self.max != 0 && self.spent >= self.max {
            return false;
        }
        self.spent = self.spent.saturating_add(1);
        true
    }

    /// Give back the current step if a free retry remains.
    pub fn refund(&mut self) -> bool {
        if self.free == 0 || self.spent == 0 {
            return false;
        }
        self.free -= 1;
        self.spent -= 1;
        true
    }
    /// Give the step back unconditionally. For retries that were not the model's doing (compacting after the
    /// server rejected the prompt size); the caller bounds how often that can happen.
    pub fn give_back(&mut self) {
        self.spent = self.spent.saturating_sub(1);
    }
}

/// Tools whose failure always means the call itself was wrong (a bad path, text that is not in the file). A
/// failing `bash` or `kernel` result is ordinary task feedback, so those are never counted as a stalled call.
const CALL_TOOLS: &[&str] = &[
    "read",
    "edit",
    "write",
    "glob",
    "grep",
    "ast_edit",
    "ast_search",
    "artifact_read",
];
/// The same tool failing this many calls in a row, whatever its arguments, earns a warning and then a stop.
const STREAK_WARN: u32 = 5;
const STREAK_STOP: u32 = 10;
/// Rounds of tool calls in which every call failed.
const ROUND_WARN: u32 = 4;
const ROUND_STOP: u32 = 8;

pub enum Stall {
    None,
    Warn(String),
    Stop(String),
}

/// `RepeatGuard` only sees the same call failing again. A model that is lost changes its guess each time, so these
/// count failures regardless of arguments: one tool failing call after call (reading a file in between does not
/// help), and whole rounds in which nothing worked.
#[derive(Default)]
pub struct StallGuard {
    streaks: std::collections::HashMap<String, u32>,
    failed_rounds: u32,
    /// Last failure cause per tool, recorded by `note_error`; named in the stop text.
    causes: std::collections::HashMap<String, String>,
}

impl StallGuard {
    /// Consecutive failures of `tool` so far, before the current call is counted.
    pub fn streak(&self, tool: &str) -> u32 {
        self.streaks.get(tool).copied().unwrap_or(0)
    }
    /// Record why the latest failing call of `tool` failed. The first non-empty line is kept
    /// and clipped, so the stop text can say what went wrong without repeating the whole output.
    pub fn note_error(&mut self, tool: &str, error: &str) {
        if let Some(line) = error.lines().map(str::trim).find(|line| !line.is_empty()) {
            self.causes.insert(tool.to_owned(), clip_middle(line, 160));
        }
    }
    pub fn call(&mut self, call: &ToolCall, failed: bool) -> Stall {
        if !CALL_TOOLS.contains(&call.name.as_str()) {
            return Stall::None;
        }
        if !failed {
            self.streaks.remove(&call.name);
            self.causes.remove(&call.name);
            return Stall::None;
        }
        let count = self.streaks.entry(call.name.clone()).or_default();
        *count += 1;
        let (count, name) = (*count, &call.name);
        let cause = self
            .causes
            .get(name)
            .map_or(String::new(), |cause| format!(" Last error: {cause}."));
        if count >= STREAK_STOP {
            Stall::Stop(format!(
                "Stopped: `{name}` failed {count} times in a row without one success.{cause} The model is not recovering from the errors it is shown. Try a smaller request, a different edit mode, or a different model."
            ))
        } else if count == STREAK_WARN {
            Stall::Warn(format!(
                "[`{name}` has now failed {count} times in a row, with different arguments each time. Guessing again will not help: each error says what is wrong or what the file really contains. Re-read the file and quote it exactly, take a different approach, or tell the user you are blocked. The turn stops after {STREAK_STOP} in a row.]"
            ))
        } else {
            Stall::None
        }
    }

    /// One round of tool calls finished; `all_failed` is true when every call in it errored.
    pub fn round(&mut self, all_failed: bool) -> Stall {
        if !all_failed {
            self.failed_rounds = 0;
            return Stall::None;
        }
        self.failed_rounds += 1;
        let rounds = self.failed_rounds;
        if rounds >= ROUND_STOP {
            Stall::Stop(format!(
                "Stopped: every tool call failed for {rounds} rounds in a row. The model is not making progress. Try a smaller request, check that the project and tools are reachable, or use a different model."
            ))
        } else if rounds == ROUND_WARN {
            Stall::Warn(format!(
                "{rounds} rounds of tool calls in a row have failed completely. Stop and reconsider: read the errors, check the current state of the files, and change your approach, or tell the user what is blocking you. The turn stops after {ROUND_STOP} such rounds."
            ))
        } else {
            Stall::None
        }
    }
}

/// What to do after a generation stopped because it hit the output token limit.
#[derive(Debug, PartialEq, Eq)]
pub enum LengthAction {
    /// Discard the reply and ask again with a larger output budget.
    Retry { max_tokens: u32 },
    /// Keep going: record `partial` (if any) as the assistant turn, then send `message` as the user.
    Nudge {
        partial: Option<String>,
        message: String,
    },
    /// Stop the turn and tell the user why.
    GiveUp(String),
}

/// Double the output budget, bounded by what the context window can still spare for the answer.
fn larger_budget(current: u32, window: u32, prompt_tokens: u64) -> Option<u32> {
    let spare = u64::from(window)
        .saturating_sub(prompt_tokens)
        .saturating_sub(CONTEXT_MARGIN);
    let next = u64::from(current.saturating_mul(2).min(MAX_RAISED_BUDGET)).min(spare) as u32;
    (next > current).then_some(next)
}

/// A reasoning model can spend its whole budget thinking and return no answer, or cut a tool call off
/// mid-argument. Treating either as a finished reply ends the turn with nothing useful.
pub fn recover_from_length(
    current: u32,
    window: u32,
    prompt_tokens: u64,
    reply: &Reply,
    nudges: u32,
) -> LengthAction {
    if let Some(max_tokens) = larger_budget(current, window, prompt_tokens) {
        return LengthAction::Retry { max_tokens };
    }
    if nudges >= MAX_LENGTH_NUDGES {
        return LengthAction::GiveUp(format!(
            "Stopped: the model reached its output limit ({current} tokens) without finishing, repeatedly. Raise `harness.max_tokens`, or use a model or setting that spends fewer tokens on reasoning."
        ));
    }
    if !reply.tool_calls.is_empty() {
        return LengthAction::Nudge {
            partial: None,
            message: "Your last tool call was cut off by the output limit, so nothing ran. Make smaller calls (for a large file, write a first part and extend it with edit) and keep your reasoning short.".into(),
        };
    }
    if reply.text.trim().is_empty() {
        return LengthAction::Nudge {
            partial: None,
            message: "You used the whole output limit on thinking and produced no answer. Think less: take the next action or answer directly.".into(),
        };
    }
    LengthAction::Nudge {
        partial: Some(reply.text.clone()),
        message: "Your reply was cut off by the output limit. Continue exactly where you stopped."
            .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments: args,
            malformed_arguments: false,
        }
    }

    fn reply(text: &str, calls: Vec<ToolCall>) -> Reply {
        Reply {
            text: text.into(),
            tool_calls: calls,
            finish_reason: "length".into(),
            prompt_tokens: None,
            completion_tokens: None,
            ..Default::default()
        }
    }

    #[test]
    fn equivalent_simple_shell_spellings_share_a_fingerprint() {
        let first = call(
            "bash",
            json!({"command":"cargo test --offline", "timeout":10}),
        );
        let second = call(
            "bash",
            json!({"command":"  cargo   'test'   --offline  ", "timeout":30}),
        );
        assert_eq!(normalized_arguments(&first), normalized_arguments(&second));
        assert_eq!(first.arguments["command"], "cargo test --offline");
        for command in ["echo *", "echo '$HOME'", "echo a && echo b", "echo \\*"] {
            let raw = call("bash", json!({"command":command}));
            assert_eq!(normalized_arguments(&raw)["command"], command);
        }
        let different = call("bash", json!({"command":"cargo test --offline other_test"}));
        assert_ne!(
            normalized_arguments(&first),
            normalized_arguments(&different)
        );
    }

    #[test]
    fn paraphrased_verification_promises_get_feedback_without_a_wording_only_stop() {
        let mut guard = NarrationGuard::default();
        for (index, text) in [
            "I'll verify the result.",
            "Next I will run the check.",
            "Now I will test the change.",
        ]
        .iter()
        .enumerate()
        {
            let result = guard.observe(&reply(
                text,
                vec![call("read", json!({"path":index.to_string()}))],
            ));
            assert_eq!(result.is_some(), index == 2);
        }
        guard.progress();
        assert!(
            guard
                .observe(&reply(
                    "I'll check a changed source.",
                    vec![call("read", json!({"path":"new"}))]
                ))
                .is_none()
        );
    }

    #[test]
    fn narration_recovery_recurs_until_real_progress() {
        let mut guard = NarrationGuard::default();
        let response = reply(
            "I'll verify the result.",
            vec![call("read", json!({"path":"a"}))],
        );
        for round in 1..=9 {
            assert_eq!(guard.observe(&response).is_some(), round % 3 == 0);
        }
        guard.progress();
        assert!(guard.observe(&response).is_none());
    }

    #[test]
    fn cosmetic_arguments_do_not_disguise_failures_but_payloads_do() {
        let mut guard = RepeatGuard::default();
        for n in 0..FAILURE_LIMIT {
            let retry = call(
                "bash",
                json!({"command":"false", "timeout":n+1, "i":n.to_string(), "_intent":n.to_string()}),
            );
            guard.observe(&retry, true);
            assert_eq!(guard.stop_message(&retry).is_some(), n + 1 == FAILURE_LIMIT);
        }
        let original = call("write", json!({"path":"a", "content":{"i":"first"}}));
        let changed = call("write", json!({"path":"a", "content":{"i":"different"}}));
        assert_ne!(
            normalized_arguments(&original),
            normalized_arguments(&changed)
        );
        let changed = call("bash", json!({"command":"true"}));
        assert!(guard.stop_message(&changed).is_none());
    }

    #[test]
    fn clipping_keeps_both_ends_and_reports_what_was_dropped() {
        let body: String = (0..2000).map(|i| format!("line {i}\n")).collect();
        let out = clip_middle(&body, 4000);
        assert!(
            out.starts_with("line 0\n") && out.ends_with("line 1999\n"),
            "{out}"
        );
        assert!(
            out.contains("bytes omitted from the middle")
                && out.contains(&format!("of {} bytes", body.len()))
        );
        assert!(out.len() < 4300, "{}", out.len());
        assert_eq!(clip_middle("short", 4000), "short");
        // Multi-byte text is never split mid-character.
        let wide = "é".repeat(5000);
        assert!(clip_middle(&wide, 1001).is_char_boundary(0));
    }

    #[test]
    fn evidence_clipping_keeps_middle_diagnostics_and_stays_bounded() {
        let noise = |n: usize| {
            (0..n)
                .map(|i| format!("   Compiling crate-{i} v0.1.0\n"))
                .collect::<String>()
        };
        let log = format!(
            "{}error[E0425]: cannot find value `x`\n  --> src/main.rs:3:5\n{}error: could not compile\n",
            noise(400),
            noise(400)
        );
        let out = clip_evidence(&log, 4000);
        assert!(
            out.contains("2 diagnostic line(s)")
                && out.contains("error[E0425]: cannot find value `x`\n  --> src/main.rs:3:5\n"),
            "{out}"
        );
        assert!(
            out.ends_with("error: could not compile\n") && out.len() < 4300,
            "{}",
            out.len()
        );
        // Without diagnostics it is plain head-and-tail clipping.
        let plain = noise(800);
        assert_eq!(clip_evidence(&plain, 4000), clip_middle(&plain, 4000));
        assert_eq!(clip_evidence("short", 4000), "short");
    }

    #[test]
    fn the_same_failing_call_is_flagged_on_its_third_repeat_only() {
        let mut guard = RepeatGuard::default();
        let same = call(
            "edit",
            json!({"path": "a", "old_string": "x", "new_string": "y"}),
        );
        assert!(guard.observe(&same, true).is_none());
        assert!(guard.observe(&same, true).is_none());
        let advice = guard.observe(&same, true).expect("third identical failure");
        assert!(advice.contains("failed 3 times") && advice.contains("same arguments"));
        assert!(guard.observe(&same, true).unwrap().contains("4 times"));
        // Other calls do not erase a failure history; success for the exact call does.
        let changed = call(
            "edit",
            json!({"path": "a", "old_string": "x2", "new_string": "y"}),
        );
        assert!(guard.observe(&changed, true).is_none());
        assert!(guard.observe(&same, true).unwrap().contains("5 times"));
        assert!(guard.stop_message(&same).is_some());
        assert!(guard.observe(&same, false).is_none());
        assert!(guard.observe(&same, true).is_none() && guard.observe(&same, true).is_none());
    }

    #[test]
    fn failed_reads_warn_immediately_and_allow_corrected_calls() {
        let mut guard = RepeatGuard::default();
        let malformed = call("read", json!({"path":"src/a.rs:25- fifty"}));
        assert!(
            guard
                .observe(&malformed, true)
                .unwrap()
                .contains("Read recovery")
        );
        assert!(guard.stop_message(&malformed).is_none());
        let other = call("read", json!({"path":"src/b.rs:1-20"}));
        assert!(guard.observe(&other, false).is_none());
        guard.observe(&malformed, true);
        assert!(guard.stop_message(&malformed).is_none());
        let corrected = call("read", json!({"path":"src/a.rs:25-50"}));
        assert!(guard.stop_message(&corrected).is_none());
        guard.observe(&malformed, false);
        assert!(guard.stop_message(&malformed).is_none());
    }

    #[test]
    fn repeated_narration_warns_periodically_despite_different_calls_and_whitespace() {
        let mut guard = NarrationGuard::default();
        let text = "I can see the current version. Let me verify the current state and check the UI header integration.";
        for n in 0..6 {
            let text = if n % 2 == 0 {
                text.to_owned()
            } else {
                text.replace(' ', "\n")
            };
            let r = reply(
                &text,
                vec![call("read", json!({"path":format!("source-{n}")}))],
            );
            assert_eq!(guard.observe(&r).is_some(), n % 3 == 2);
        }
        assert!(guard.observe(&reply(text, vec![])).is_none());
        assert!(
            guard
                .observe(&reply("Checking.", vec![call("read", json!({}))]))
                .is_none()
        );
        for n in 0..100 {
            let r = reply(
                &format!("{text} Unique explanation {n}."),
                vec![call("read", json!({}))],
            );
            assert!(guard.observe(&r).is_none());
        }
        assert_eq!(guard.seen.len(), 32);
    }

    #[test]
    fn a_length_stop_first_asks_for_a_bigger_budget_that_fits_the_context() {
        let r = reply("", vec![]);
        assert_eq!(
            recover_from_length(4096, 32768, 2000, &r, 0),
            LengthAction::Retry { max_tokens: 8192 }
        );
        assert_eq!(
            recover_from_length(8192, 32768, 2000, &r, 0),
            LengthAction::Retry { max_tokens: 16384 }
        );
        // Never more than the window can spare after the prompt.
        assert_eq!(
            recover_from_length(8192, 32768, 20000, &r, 0),
            LengthAction::Retry { max_tokens: 11744 }
        );
        assert_eq!(
            recover_from_length(20000, 65536, 100, &r, 0),
            LengthAction::Retry { max_tokens: 32768 }
        );
    }

    #[test]
    fn without_room_to_grow_the_model_is_nudged_then_the_turn_stops() {
        let spent = recover_from_length(8192, 8192, 6000, &reply("", vec![]), 0);
        assert!(
            matches!(&spent, LengthAction::Nudge { partial: None, message } if message.contains("Think less")),
            "{spent:?}"
        );
        let partial = recover_from_length(8192, 8192, 6000, &reply("The answer is", vec![]), 0);
        assert!(
            matches!(&partial, LengthAction::Nudge { partial: Some(p), .. } if p == "The answer is"),
            "{partial:?}"
        );
        let cut_call = recover_from_length(
            8192,
            8192,
            6000,
            &reply("", vec![call("write", json!({}))]),
            1,
        );
        assert!(
            matches!(&cut_call, LengthAction::Nudge { partial: None, message } if message.contains("smaller calls")),
            "{cut_call:?}"
        );
        match recover_from_length(8192, 8192, 6000, &reply("", vec![]), MAX_LENGTH_NUDGES) {
            LengthAction::GiveUp(text) => {
                assert!(text.contains("harness.max_tokens") && text.contains("8192"))
            }
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn zero_step_budget_is_unlimited() {
        let mut budget = StepBudget::new(0, 0);
        for _ in 0..100 {
            assert!(budget.begin());
        }
    }

    #[test]
    fn unusable_replies_are_refunded_a_bounded_number_of_times() {
        let mut budget = StepBudget::new(3, 2);
        assert!(
            budget.begin() && budget.refund(),
            "a cut-off reply costs nothing"
        );
        assert!(budget.begin() && budget.refund());
        assert!(
            budget.begin() && !budget.refund(),
            "only two free retries, then it counts"
        );
        assert!(
            budget.begin() && budget.begin() && !budget.begin(),
            "three real steps were all it had"
        );
        // Evaluation runs grant no refunds: their step budget is the thing being measured.
        let mut measured = StepBudget::new(2, 0);
        assert!(measured.begin() && !measured.refund() && measured.begin() && !measured.begin());
    }

    #[test]
    fn zero_step_budget_is_unlimited_but_refunds_remain_bounded() {
        let mut budget = StepBudget::new(0, 2);
        for _ in 0..100 {
            assert!(budget.begin());
        }
        assert!(budget.refund() && budget.refund() && !budget.refund());
        budget.spent = u32::MAX;
        assert!(budget.begin());
        assert_eq!(budget.spent, u32::MAX);
        assert!(budget.begin());
        let h = crate::config::types::Harness {
            max_steps: 0,
            ..Default::default()
        };
        assert_eq!(h.capped_steps(24), 24);
        assert_eq!(h.capped_steps(8), 8);
        assert_eq!(
            crate::config::types::Harness { max_steps: 2, ..h }.capped_steps(24),
            2
        );
    }

    #[test]
    fn a_tool_that_keeps_failing_with_different_arguments_is_warned_then_stopped() {
        let mut guard = StallGuard::default();
        let edit = |n: u32| {
            call(
                "edit",
                json!({"path": "a.rs", "old_string": format!("guess {n}"), "new_string": "x"}),
            )
        };
        for n in 1..5 {
            assert!(
                matches!(guard.call(&edit(n), true), Stall::None),
                "failure {n}"
            );
        }
        let Stall::Warn(advice) = guard.call(&edit(5), true) else {
            panic!("warns at the fifth")
        };
        assert!(
            advice.contains("`edit`")
                && advice.contains("5 times in a row")
                && advice.contains("different arguments")
        );
        for n in 6..10 {
            assert!(
                matches!(guard.call(&edit(n), true), Stall::None),
                "warns once, not every time"
            );
        }
        let Stall::Stop(text) = guard.call(&edit(10), true) else {
            panic!("stops at the tenth")
        };
        assert!(text.contains("`edit`") && text.contains("10 times"));
    }

    #[test]
    fn stop_text_names_the_last_error_of_the_failing_tool() {
        let mut guard = StallGuard::default();
        let edit = |n: u32| {
            call(
                "edit",
                json!({"path": "a.rs", "old_string": format!("guess {n}"), "new_string": "x"}),
            )
        };
        for n in 1..10 {
            guard.note_error(
                "edit",
                &format!("\n  old_string not found in a.rs\nattempt {n}"),
            );
            guard.call(&edit(n), true);
        }
        guard.note_error("edit", "old_string not found in a.rs\nattempt 10");
        let Stall::Stop(text) = guard.call(&edit(10), true) else {
            panic!("stops at the tenth")
        };
        assert!(
            text.contains("`edit`") && text.contains("Last error: old_string not found in a.rs."),
            "{text}"
        );
        assert!(
            !text.contains("attempt"),
            "only the first line is kept: {text}"
        );
    }

    #[test]
    fn a_success_of_that_tool_resets_it_and_other_tools_do_not() {
        let mut guard = StallGuard::default();
        let edit = || {
            call(
                "edit",
                json!({"path": "a", "old_string": "x", "new_string": "y"}),
            )
        };
        for _ in 0..4 {
            assert!(matches!(guard.call(&edit(), true), Stall::None));
        }
        // Reading a file in between does not clear an edit streak: that is the read-guess-read-guess loop.
        assert!(matches!(
            guard.call(&call("read", json!({"path": "a"})), false),
            Stall::None
        ));
        assert!(matches!(guard.call(&edit(), true), Stall::Warn(_)));
        assert!(matches!(guard.call(&edit(), false), Stall::None));
        for _ in 0..4 {
            assert!(
                matches!(guard.call(&edit(), true), Stall::None),
                "a fresh streak"
            );
        }
    }

    #[test]
    fn a_failing_shell_command_is_task_feedback_not_a_stalled_call() {
        let mut guard = StallGuard::default();
        for n in 0..30 {
            assert!(matches!(
                guard.call(
                    &call("bash", json!({"command": format!("cargo test {n}")})),
                    true
                ),
                Stall::None
            ));
        }
    }

    #[test]
    fn rounds_where_every_call_failed_are_counted_and_any_success_clears_them() {
        let mut guard = StallGuard::default();
        for round in 1..4 {
            assert!(matches!(guard.round(true), Stall::None), "round {round}");
        }
        let Stall::Warn(advice) = guard.round(true) else {
            panic!("warns at the fourth")
        };
        assert!(advice.contains("4 rounds"));
        assert!(matches!(guard.round(false), Stall::None));
        for _ in 0..7 {
            assert!(matches!(guard.round(true), Stall::None | Stall::Warn(_)));
        }
        assert!(
            matches!(guard.round(true), Stall::Stop(_)),
            "eight in a row"
        );
    }
}
