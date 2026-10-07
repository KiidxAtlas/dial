//! Loop-level safeguards that keep a weak or confused model productive: how tool results enter
//! the conversation, what to do when a generation runs out of output tokens, and noticing a model
//! that repeats a failing call.

use crate::llm::{Reply, ToolCall};

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
    /// Keep failures across intervening reads. Only success for this exact call resets its count.
    pub fn observe(&mut self, call: &ToolCall, failed: bool) -> Option<String> {
        let key = format!("{}\u{0}{}", call.name, call.arguments);
        if !failed {
            self.failures.remove(&key);
            return None;
        }
        let count = self.failures.entry(key).or_default();
        *count += 1;
        (*count >= REPEAT_LIMIT).then(|| {
            format!(
                "[This exact call has now failed {count} times with the same arguments this turn. Do not issue it again unchanged: change the arguments or the approach. Reading another file does not reset this count; the turn stops after {FAILURE_LIMIT} identical failures.]"
            )
        })
    }

    pub fn stop_message(&self, call: &ToolCall) -> Option<String> {
        let count = *self
            .failures
            .get(&format!("{}\u{0}{}", call.name, call.arguments))?;
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

    /// Start a step; false once a nonzero budget is used up. Zero means unlimited.
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
}

impl StallGuard {
    pub fn call(&mut self, call: &ToolCall, failed: bool) -> Stall {
        if !CALL_TOOLS.contains(&call.name.as_str()) {
            return Stall::None;
        }
        if !failed {
            self.streaks.remove(&call.name);
            return Stall::None;
        }
        let count = self.streaks.entry(call.name.clone()).or_default();
        *count += 1;
        let (count, name) = (*count, &call.name);
        if count >= STREAK_STOP {
            Stall::Stop(format!(
                "Stopped: `{name}` failed {count} times in a row without one success. The model is not recovering from the errors it is shown. Try a smaller request, a different edit mode, or a different model."
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
        let same = call("edit", json!({"path": "a", "old": "x", "new": "y"}));
        assert!(guard.observe(&same, true).is_none());
        assert!(guard.observe(&same, true).is_none());
        let advice = guard.observe(&same, true).expect("third identical failure");
        assert!(advice.contains("failed 3 times") && advice.contains("same arguments"));
        assert!(guard.observe(&same, true).unwrap().contains("4 times"));
        // Other calls do not erase a failure history; success for the exact call does.
        let changed = call("edit", json!({"path": "a", "old": "x2", "new": "y"}));
        assert!(guard.observe(&changed, true).is_none());
        assert!(guard.observe(&same, true).unwrap().contains("5 times"));
        assert!(guard.stop_message(&same).is_some());
        assert!(guard.observe(&same, false).is_none());
        assert!(guard.observe(&same, true).is_none() && guard.observe(&same, true).is_none());
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
    fn a_tool_that_keeps_failing_with_different_arguments_is_warned_then_stopped() {
        let mut guard = StallGuard::default();
        let edit = |n: u32| {
            call(
                "edit",
                json!({"path": "a.rs", "old": format!("guess {n}"), "new": "x"}),
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
    fn a_success_of_that_tool_resets_it_and_other_tools_do_not() {
        let mut guard = StallGuard::default();
        let edit = || call("edit", json!({"path": "a", "old": "x", "new": "y"}));
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
