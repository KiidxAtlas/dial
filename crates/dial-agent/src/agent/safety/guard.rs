//! Loop-level safeguards that keep a weak or confused model productive: how tool results enter
//! the conversation, what to do when a generation runs out of output tokens, and noticing a model
//! that repeats a failing call.

use crate::llm::{Reply, ToolCall};

/// Argument identity for one tool call. This is the only normalizer: duplicate detection, result
/// matching and execution-side identity all read through it.
pub(in crate::agent) fn normalized_arguments(call: &ToolCall) -> serde_json::Value {
    normalize_arguments(&call.name, &call.arguments)
}

/// Removes only top-level harness metadata: presentation keys (`i`, `__intent`, legacy `_intent`)
/// and, for bash, the waiting timeout. Nested fields are application payload and are never
/// removed, so `{"filter":{"i":1}}` and `{"filter":{"i":2}}` remain distinct calls.
fn normalize_arguments(name: &str, arguments: &serde_json::Value) -> serde_json::Value {
    let mut args = arguments.clone();
    if let Some(object) = args.as_object_mut() {
        object.remove("i");
        object.remove("__intent");
        object.remove("_intent");
        if name == "bash" {
            object.remove("timeout");
            if let Some(command) = object.get("command").and_then(|v| v.as_str())
                // Expansions and escapes depend on quoting; keep them exact.
                // This fingerprint never changes the executed command.
                && !command.contains(['*', '?', '[', ']', '~', '#', '!', '\\'])
                && let Some(words) = crate::agent::safety::approval::shell_words(command)
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
pub(in crate::agent) struct RepeatedToolCall {
    pub tool_name: String,
    pub count: usize,
    pub arguments_summary: String,
    pub result_summary: String,
    /// Identity of the streak's call set. A reply whose calls produce the same key repeats it.
    key: String,
    /// The streak's newest turn, with the result each of its calls received.
    earlier: Vec<EarlierCall>,
}

#[derive(Debug, PartialEq, Eq)]
struct EarlierCall {
    name: String,
    arguments: serde_json::Value,
    id: String,
    result: String,
}

/// Sorted canonical `[name, arguments]` pairs: the identity of one assistant turn's call set.
fn call_key(calls: &[(String, String, serde_json::Value)]) -> String {
    let mut parts: Vec<String> = calls
        .iter()
        .map(|(_, name, args)| {
            serde_json::json!([name, identity_arguments(name, args)]).to_string()
        })
        .collect();
    parts.sort();
    serde_json::to_string(&parts).unwrap_or_default()
}

/// Summarized text of the tool result that answers `id`, or empty when none is in `messages`.
fn result_text_for(messages: &[serde_json::Value], id: &str) -> String {
    messages
        .iter()
        .rev()
        .find(|message| {
            message["role"].as_str() == Some("tool") && message["tool_call_id"].as_str() == Some(id)
        })
        .map(|message| match &message["content"] {
            serde_json::Value::String(text) => summarize_text(text, 200),
            serde_json::Value::Null => String::new(),
            other => summarize_text(&other.to_string(), 200),
        })
        .unwrap_or_default()
}

/// Canonical identity of a call's arguments: the top-level normalizer, then object keys sorted at
/// every depth so key order never changes identity. Sorting never removes a field.
fn identity_arguments(name: &str, arguments: &serde_json::Value) -> serde_json::Value {
    sorted_keys(&normalize_arguments(name, arguments))
}

fn sorted_keys(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sorted_keys).collect()),
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<&String, &Value> = map.iter().collect();
            sorted
                .into_iter()
                .map(|(key, value)| (key.clone(), sorted_keys(value)))
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
    pub(in crate::agent) fn detect(
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
            let key = call_key(&parsed);
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
        let earlier = newest
            .iter()
            .map(|(id, name, args)| EarlierCall {
                name: name.clone(),
                arguments: identity_arguments(name, args),
                id: id.clone(),
                result: result_text_for(messages, id),
            })
            .collect();
        let arguments_summary = summarize_text(&identity_arguments(&name, &args).to_string(), 400);
        Some(Self {
            tool_name: name,
            count,
            arguments_summary,
            result_summary: result_text_for(messages, &id),
            key: streak_key.unwrap_or_default(),
            earlier,
        })
    }

    /// When the reply's calls repeat this streak exactly (same call set, every call well formed),
    /// returns one synthetic result per call, in order. Each names the earlier identical call and
    /// its result, so the repeat is answered without running again. `None` lets the calls run.
    pub(in crate::agent) fn short_circuit(&self, calls: &[ToolCall]) -> Option<Vec<String>> {
        if calls.is_empty() || calls.iter().any(|call| call.malformed_arguments) {
            return None;
        }
        let parsed: Vec<(String, String, serde_json::Value)> = calls
            .iter()
            .map(|call| (call.id.clone(), call.name.clone(), call.arguments.clone()))
            .collect();
        if call_key(&parsed) != self.key {
            return None;
        }
        let mut unmatched: Vec<&EarlierCall> = self.earlier.iter().collect();
        parsed
            .iter()
            .map(|(_, name, args)| {
                let canonical = identity_arguments(name, args);
                let at = unmatched
                    .iter()
                    .position(|earlier| earlier.name == *name && earlier.arguments == canonical)?;
                let earlier = unmatched.remove(at);
                Some(
                    crate::agent::safety::stream_guards::render_duplicate_call_result(
                        name,
                        &earlier.id,
                        &earlier.result,
                    ),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod repeated_tool_call_tests;

/// Repeated narration can disguise a loop even when the model varies its calls.
/// This is feedback, not a stop condition: repeated wording alone does not prove
/// the tools failed to make progress. Result/failure guards enforce those stops.
#[derive(Default)]
pub(in crate::agent) struct NarrationGuard {
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
/// Length stops that repeat past this many nudges get stronger guidance; they never end the turn.
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
/// failed tests; see [`crate::tools::code::diagnostics`]) from the omitted middle are kept between the two
/// ends, within a quarter of `max`, so a long build log still shows the first error and its location.
pub fn clip_evidence(text: &str, max: usize) -> String {
    use crate::tools::code::diagnostics;
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
    /// Failures never end the turn: guidance is stated when the repeat starts and again at each
    /// further multiple of `FAILURE_LIMIT`, in stronger words.
    pub fn observe(&mut self, call: &ToolCall, failed: bool) -> Option<String> {
        let key = Self::key(call);
        if !failed {
            self.failures.remove(&key);
            return None;
        }
        let count = self.failures.entry(key).or_default();
        *count += 1;
        let count = *count;
        if call.name == "read" {
            return Some(format!(
                "[Read recovery: this exact call has now failed {count} times with the same arguments. Use the diagnostic to correct the path or numeric selector (offset/limit is also supported), or locate the file once with a scoped glob/grep. Reuse context already returned and request only missing lines. Do not retry unchanged, even after other reads; continue the task with the corrected call.]"
            ));
        }
        if count < REPEAT_LIMIT {
            return None;
        }
        if count.is_multiple_of(FAILURE_LIMIT) {
            return Some(Self::escalation(call, count));
        }
        Some(format!(
            "[This exact call has now failed {count} times with the same arguments this turn. Do not issue it again unchanged: change the arguments or the approach. Reading another file does not reset this count.]"
        ))
    }

    /// Stronger guidance for an identical call that keeps failing. The identical retry is withheld
    /// rather than run, so the model is told what to do instead. The turn itself is never ended.
    fn escalation(call: &ToolCall, count: u32) -> String {
        if call.name == "todo" && call.arguments["op"] == "done" {
            return format!(
                "[Escalation: `todo done` for the same target has failed {count} times this turn. Rewording the completion claim is not recovery. Deliver the report, satisfy the reported check, or block the step with a concrete reason. The turn continues.]"
            );
        }
        format!(
            "[Escalation: `{}` with the same arguments has failed {count} times this turn, and the feedback so far has not changed the call. Identical retries are withheld, not run. Choose a different tool or different arguments that the error text supports, or report the blocker in your answer. The turn continues.]",
            call.name
        )
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
/// The same tool failing this many calls in a row, whatever its arguments, earns a warning. The warning
/// repeats with stronger wording at each further multiple of this count. Failures never end the turn.
const STREAK_WARN: u32 = 5;
/// Rounds of tool calls in which every call failed. Warnings repeat at each further multiple of this count.
const ROUND_WARN: u32 = 4;

/// Guidance from a stall check, delivered to the model in the same turn. No variant ends the turn.
pub enum Stall {
    None,
    Warn(String),
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
        if count == STREAK_WARN {
            Stall::Warn(format!(
                "[`{name}` has now failed {count} times in a row, with different arguments each time. Guessing again will not help: each error says what is wrong or what the file really contains. Re-read the file and quote it exactly, take a different approach, or report the blocker in your answer.]"
            ))
        } else if count > STREAK_WARN && count % STREAK_WARN == 0 {
            Stall::Warn(format!(
                "[Escalation: `{name}` has failed {count} times in a row without one success.{cause} Repeating the same kind of guess is not working. Before the next `{name}` call, read the exact current state it targets and make one call that copies it exactly, or switch to a different tool. The turn continues.]"
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
        if rounds == ROUND_WARN {
            Stall::Warn(format!(
                "{rounds} rounds of tool calls in a row have failed completely. Stop and reconsider: read the errors, check the current state of the files, and change your approach, or report the blocker in your answer."
            ))
        } else if rounds > ROUND_WARN && rounds.is_multiple_of(ROUND_WARN) {
            Stall::Warn(format!(
                "[Escalation: every tool call has failed for {rounds} rounds in a row. Retrying the same kind of call is not making progress. Run one small call known to work, confirm the project and tools respond, then change approach. The turn continues.]"
            ))
        } else {
            Stall::None
        }
    }
}

/// Single lookups in a row, each its own model round, before the model is told to batch them.
const SERIAL_WARN: u32 = 5;
/// Tools that only observe the project: they neither change what a command would print nor
/// depend on one another unless the model chooses so.
const OBSERVING_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "find",
    "ast_search",
    "artifact_read",
    "todo",
    "skill",
    "recall",
];

/// Wasted motion that is not a failure: asking the shell again for a result the project has not
/// changed under, and spending one model round per lookup that could have shared a round. Both
/// only advise; neither blocks a call.
#[derive(Default)]
pub struct PaceGuard {
    /// Shell pipelines that finished since the project last (possibly) changed.
    current: std::collections::HashSet<String>,
    serial: u32,
}

impl PaceGuard {
    /// Observe one finished call; returns guidance when it repeats a still-current shell command.
    pub fn call(
        &mut self,
        call: &ToolCall,
        output: &crate::runtime::tool_output::ToolOutput,
    ) -> Option<String> {
        use crate::runtime::execution::ToolStatus;
        let observing = OBSERVING_TOOLS.contains(&call.name.as_str());
        if call.name != "bash" {
            if !observing || !output.execution.changed_paths.is_empty() {
                self.current.clear();
            }
            return None;
        }
        let heads = crate::tools::exec::bash_interceptor::pipeline_heads(
            call.arguments["command"].as_str().unwrap_or_default(),
        );
        let repeated = heads
            .iter()
            .find(|(head, _)| self.current.contains(head))
            .map(|(head, _)| head.clone());
        // Only commands known not to write can leave earlier results current.
        if heads.is_empty()
            || !output.execution.changed_paths.is_empty()
            || heads
                .iter()
                .any(|(head, _)| !crate::agent::safety::approval::safe_bash(head))
        {
            self.current.clear();
        }
        let complete = matches!(
            output.execution.status,
            None | Some(ToolStatus::Ok | ToolStatus::Error)
        ) && output
            .process_outcome()
            .is_none_or(|p| p.started && !p.timed_out && !p.cancelled && p.output_complete);
        if complete {
            // A run that was cut by a trailing filter still ran to completion, so rerunning it
            // unfiltered is exactly the wasted second run this exists to catch.
            self.current.extend(heads.into_iter().map(|(head, _)| head));
        }
        repeated.map(|head| {
            format!(
                "[`{head}` already ran earlier this turn and nothing has changed the project since, so that result is still current. Use it (its complete output is behind the `[complete output: artifact://…]` link when it was long, readable with `artifact_read` by line range) instead of running the command again.]"
            )
        })
    }

    /// One round of tool calls finished. After `SERIAL_WARN` rounds that each held a single lookup,
    /// the model is told to share rounds between independent lookups; once per streak.
    pub fn round(&mut self, calls: &[ToolCall]) -> Option<String> {
        let lookup = match calls {
            [only] => {
                OBSERVING_TOOLS.contains(&only.name.as_str())
                    || (only.name == "bash"
                        && only.arguments["command"]
                            .as_str()
                            .is_some_and(crate::agent::safety::approval::safe_bash))
            }
            _ => false,
        };
        if !lookup {
            self.serial = 0;
            return None;
        }
        self.serial += 1;
        (self.serial == SERIAL_WARN).then(|| {
            format!(
                "[{SERIAL_WARN} rounds in a row each made a single lookup. When the next lookups do not depend on each other's results, issue them together in one response (several `read` paths, `grep`, `glob`, safe `bash`): they run concurrently and cost one round.]"
            )
        })
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
    // Past the nudge limit the nudge gets stronger instead of ending the turn. Nudged replies are
    // refunded only a bounded number of times per turn (`StepBudget::FREE_RETRIES`); after that each
    // one is charged to `harness.max_steps`, which remains the bound on this loop.
    let escalated = nudges >= MAX_LENGTH_NUDGES;
    let nudge = |partial: Option<String>, text: &str| LengthAction::Nudge {
        partial,
        message: if escalated {
            format!(
                "[Escalation: {nudges} replies in a row hit the output limit ({current} tokens) without finishing. Make the next reply short: take one concrete action, or give the answer in a few sentences, and put long content into a file written in parts.]\n{text}"
            )
        } else {
            text.into()
        },
    };
    if !reply.tool_calls.is_empty() {
        return nudge(
            None,
            "Your last tool call was cut off by the output limit, so nothing ran. Make smaller calls (for a large file, write a first part and extend it with edit) and keep your reasoning short.",
        );
    }
    if reply.text.trim().is_empty() {
        return nudge(
            None,
            "You used the whole output limit on thinking and produced no answer. Think less: take the next action or answer directly.",
        );
    }
    nudge(
        Some(reply.text.clone()),
        "Your reply was cut off by the output limit. Continue exactly where you stopped.",
    )
}

#[cfg(test)]
mod tests;
