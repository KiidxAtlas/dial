//! Deterministic turn progress state machine.
//!
//! Coordinates per-request and per-round progress guards (`RepeatedToolCall`, `StallGuard`,
//! `PaceGuard`, `NarrationGuard` output, `OutcomeGradient`, and contract-failure reliability)
//! into one ordered decision surface instead of scattering guard checks across `prepare.rs` and
//! `settle.rs`.
use crate::agent::run::engine::RoundProgress;
use crate::agent::run::guidance::{self, Priority};
use crate::agent::safety::{guard, stream_guards};
use crate::agent::verify::completion::TurnOutcome;
use crate::llm::ToolCall;
use serde_json::{Value, json};

#[cfg(test)]
mod tests;

/// Request-time progress signals computed before building the next model request.
pub(in crate::agent::run::turn) struct RequestAdvisories {
    pub(in crate::agent::run::turn) repeat_streak: Option<guard::RepeatedToolCall>,
    pub(in crate::agent::run::turn) repetition_event: Option<String>,
    pub(in crate::agent::run::turn) notes: Vec<Value>,
}

/// Evaluate request-time repetition and serial-lookup pace signals in one deterministic pass.
pub(in crate::agent::run::turn) fn evaluate_request_advisories(
    messages: &[Value],
    pace: &guard::PaceGuard,
    planning_required: bool,
    research_step: bool,
    names: &[String],
    single_call: bool,
) -> RequestAdvisories {
    let repeat_streak = guard::RepeatedToolCall::detect(
        messages,
        stream_guards::TOOL_CALL_LOOP_THRESHOLD,
        stream_guards::TOOL_CALL_LOOP_EXEMPT_TOOLS,
    );
    let mut repetition_event = None;
    let mut notes = Vec::new();
    if let Some((event, guidance)) =
        stream_guards::repetition_guidance_with_streak(messages, repeat_streak.as_ref())
    {
        repetition_event = Some(event);
        notes.push(guidance::note(Priority::Repetition, guidance));
    }
    if !planning_required
        && !research_step
        && !names.is_empty()
        && let Some(advice) = pace.lookup_advice(single_call, names.iter().any(|n| n == "read"))
    {
        notes.push(guidance::note(Priority::Loop, advice));
    }
    RequestAdvisories {
        repeat_streak,
        repetition_event,
        notes,
    }
}

/// All observed signals from one executed tool round.
pub(in crate::agent::run::turn) struct RoundSignals<'a> {
    pub(in crate::agent::run::turn) raw_progress: RoundProgress,
    pub(in crate::agent::run::turn) short_circuited: bool,
    pub(in crate::agent::run::turn) narration_warning: Option<String>,
    pub(in crate::agent::run::turn) round_succeeded: bool,
    pub(in crate::agent::run::turn) tool_calls: &'a [ToolCall],
    pub(in crate::agent::run::turn) repeat_streak: Option<&'a guard::RepeatedToolCall>,
    pub(in crate::agent::run::turn) form_kinds: &'a [String],
    pub(in crate::agent::run::turn) execution_failed: bool,
    pub(in crate::agent::run::turn) observed_check_failure: bool,
    pub(in crate::agent::run::turn) round_checked: bool,
    pub(in crate::agent::run::turn) mutations: u64,
    pub(in crate::agent::run::turn) failed_checks: usize,
    pub(in crate::agent::run::turn) failing_tests: usize,
    pub(in crate::agent::run::turn) unnamed_failures: usize,
    pub(in crate::agent::run::turn) gradient_rounds: u8,
    pub(in crate::agent::run::turn) reliability_enabled: bool,
    pub(in crate::agent::run::turn) max_contract_failures: u32,
    pub(in crate::agent::run::turn) hermetic: bool,
}

/// A deterministic hard stop emitted by the progress state machine.
#[derive(Debug, PartialEq)]
pub(in crate::agent::run::turn) struct HardStop {
    pub(in crate::agent::run::turn) outcome: TurnOutcome,
    pub(in crate::agent::run::turn) reason: String,
    pub(in crate::agent::run::turn) event_kind: &'static str,
    pub(in crate::agent::run::turn) event_data: Value,
}

/// Consolidated verdict for one settled tool round.
pub(in crate::agent::run::turn) struct RoundEvaluation {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(in crate::agent::run::turn) effective_progress: RoundProgress,
    pub(in crate::agent::run::turn) tool_round_failed: bool,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(in crate::agent::run::turn) hard_stop: Option<HardStop>,
    pub(in crate::agent::run::turn) narration_event: Option<String>,
    pub(in crate::agent::run::turn) advisories: Vec<(Priority, String)>,
}

/// Normalize raw round progress against round-level execution/admission failures and check for
/// hard repeated-call stops before advancing stateful guards.
pub(in crate::agent::run::turn) fn normalize_round_progress(
    raw_progress: RoundProgress,
    short_circuited: bool,
    execution_failed: bool,
    observed_check_failure: bool,
    has_form_failures: bool,
) -> RoundProgress {
    if raw_progress != RoundProgress::Productive
        && (short_circuited || execution_failed || observed_check_failure || has_form_failures)
    {
        RoundProgress::Stalled
    } else {
        raw_progress
    }
}

/// Check whether a short-circuited repeated call streak crossed the hard-stop threshold.
pub(in crate::agent::run::turn) fn repeated_call_hard_stop(
    short_circuited: bool,
    repeat_streak: Option<&guard::RepeatedToolCall>,
) -> Option<HardStop> {
    if !short_circuited {
        return None;
    }
    let repeat = repeat_streak?;
    if repeat.count <= stream_guards::TOOL_CALL_LOOP_THRESHOLD + 1 {
        return None;
    }
    let attempts = repeat.count + 1;
    let result = if repeat.result_summary.is_empty() {
        "(no text result)"
    } else {
        repeat.result_summary.as_str()
    };
    let reason = format!(
        "Blocked: the model sent the identical `{}` call {attempts} times in a row and kept repeating it after being told it would not run again.\nArguments: `{}`\nLast executed result: `{result}`\nCompleted effects and the conversation are kept; no completion was recorded. Send a direction (for example the corrected command) or say \"continue\" to resume.",
        repeat.tool_name, repeat.arguments_summary
    );
    Some(HardStop {
        outcome: TurnOutcome::Blocked,
        reason,
        event_kind: "repeated_call_stop",
        event_data: json!({"name": repeat.tool_name, "count": attempts}),
    })
}

/// Advance the round's stateful guards (`StallGuard`, `PaceGuard`, `OutcomeGradient`,
/// `reliability::Stats`) and produce the ordered advisories and failure classification.
pub(in crate::agent::run::turn) fn evaluate_round(
    signals: RoundSignals<'_>,
    stall: &mut guard::StallGuard,
    pace: &mut guard::PaceGuard,
    reliability: &mut crate::reliability::Stats,
    outcome_gradient: &mut crate::agent::run::outcome_gradient::Gradient,
) -> RoundEvaluation {
    let effective_progress = normalize_round_progress(
        signals.raw_progress,
        signals.short_circuited,
        signals.execution_failed,
        signals.observed_check_failure,
        !signals.form_kinds.is_empty(),
    );
    let hard_stop = repeated_call_hard_stop(signals.short_circuited, signals.repeat_streak);
    let mut advisories = Vec::new();
    let narration_event = signals.narration_warning.clone();
    if let Some(advice) = signals.narration_warning {
        advisories.push((Priority::Loop, advice));
    }
    // A short-circuited round neither failed nor succeeded, so it must not advance failure streaks.
    if !signals.short_circuited
        && let guard::Stall::Warn(advice) = stall.round(!signals.round_succeeded)
    {
        advisories.push((Priority::Failure, advice));
    }
    // A round in which work was withheld is not evidence of serial lookups.
    pace.observe_round(if signals.short_circuited {
        &[]
    } else {
        signals.tool_calls
    });
    if signals.round_checked && signals.mutations > 0 {
        let score = signals.failed_checks + signals.failing_tests + signals.unnamed_failures;
        if let Some(advice) = outcome_gradient.observe(score, signals.gradient_rounds) {
            advisories.push((Priority::Loop, advice));
        }
    }
    reliability.round(!signals.form_kinds.is_empty(), signals.round_succeeded);
    let tool_round_failed = effective_progress == RoundProgress::Stalled
        || !signals.form_kinds.is_empty()
        || signals.execution_failed
        || signals.observed_check_failure;
    if !signals.hermetic {
        let failures = reliability.consecutive_contract_failures;
        let limit = signals.max_contract_failures;
        if signals.reliability_enabled
            && limit > 0
            && failures >= limit
            && failures.is_multiple_of(limit)
        {
            advisories.push((
                Priority::Failure,
                format!(
                    "[Escalation from dial] {failures} tool calls in a row broke the tool contract, and none of the rejected calls ran. Call only the offered tools, with arguments that match their schemas exactly. Do not restate the rules or explain the failure at length: make the next call now."
                ),
            ));
        }
    }
    RoundEvaluation {
        effective_progress,
        tool_round_failed,
        hard_stop,
        narration_event,
        advisories,
    }
}
