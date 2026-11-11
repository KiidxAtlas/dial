//! Presentation only; the agent owns projection, budget and method selection.
use crate::agent::Agent;
use crate::agent::window::decision::{CompactionPreflight, ContextDecision};
use crate::app::display::group_digits;
use crate::config::compaction::{CompactionMethod, CompactionOptions};
use anyhow::Result;
use serde_json::Value;

pub(super) fn render_context(agent: &Agent<'_>, pending: &[Value]) -> Result<String> {
    Ok(render_decision(&agent.context_decision(pending)?))
}

fn render_decision(report: &ContextDecision) -> String {
    // The status line shows the model window; say plainly when the history budget is smaller.
    let window = if report.model_window > report.window {
        format!(
            "{} tokens (model) · history planned against {} (efficiency cap)",
            group_digits(u64::from(report.model_window)),
            group_digits(u64::from(report.window))
        )
    } else {
        format!("{} tokens", group_digits(u64::from(report.window)))
    };
    let mut text = format!(
        "Model: {}/{}\nWindow: {window}\nPrompt budget: {} · response reserve: {}\nAuto-compact at: {}\n\nNext request (estimate): {} tokens{}\n  fixed overhead: {}\n  history + pending input: {}",
        report.provider,
        report.model,
        group_digits(report.prompt_budget),
        group_digits(report.response_reserve),
        report.trigger.map_or_else(
            || "off (manual context control)".to_owned(),
            |n| format!("{} tokens", group_digits(n))
        ),
        group_digits(report.next_tokens),
        if report.pending_messages > 0 {
            format!(
                " ({} queued/draft messages included)",
                report.pending_messages
            )
        } else {
            String::new()
        },
        group_digits(report.fixed_tokens),
        group_digits(report.history_tokens),
    );
    if let Some(last) = &report.last_request {
        text.push_str(&format!(
            "\n\nLast sent request (estimate): {} tokens",
            group_digits(last.total_tokens())
        ));
        let rows = last.rows();
        let label_w = rows
            .iter()
            .map(|r| r.label.chars().count())
            .max()
            .unwrap_or(0);
        for row in rows {
            text.push_str(&format!(
                "\n  {:<label_w$}  {:>9}",
                row.label,
                group_digits(row.tokens)
            ));
        }
    } else {
        text.push_str("\n\nLast sent request: none yet.");
    }
    text.push_str("\n\nRecommendation: ");
    if report.fixed_tokens >= report.prompt_budget {
        text.push_str("Reduce fixed instructions/context/tool overhead or choose a larger-window model with /model. History compaction alone cannot fit this prompt.");
    } else if report.next_tokens > report.prompt_budget
        || report.trigger.is_some_and(|t| report.next_tokens >= t)
    {
        text.push_str("Review /compact to summarize eligible older history. If the recent tail or pending input is large, shorten that input or choose a larger-window /model instead.");
    } else {
        text.push_str("There is headroom; compaction is not needed.");
    }
    text.push_str("\nCounts are Dial's estimates, not provider usage.");
    if report.approximate_media {
        text.push_str(" Images and provider-native replay are present: their token cost is approximate/opaque, not an exact capacity guarantee.");
    }
    text
}

pub(super) fn render_compaction_preflight(
    agent: &Agent<'_>,
    options: &CompactionOptions,
) -> Result<String> {
    Ok(render_preflight(&agent.compaction_preflight(options)?))
}

fn render_preflight(report: &CompactionPreflight) -> String {
    let mut text = if report.notes_reset {
        "Selected operation: notes-backed new context (no summary method).\n".to_owned()
    } else {
        format!(
            "Selected/fallback method order: {}\n",
            if report.methods.is_empty() {
                "none enabled".to_owned()
            } else {
                report
                    .methods
                    .iter()
                    .map(|m| format!("{m:?}").to_lowercase())
                    .collect::<Vec<_>>()
                    .join(" → ")
            }
        )
    };
    text.push_str(&format!("Eligible older history: {} messages · recent tail retained: {} messages\nRecent-tail policy budget: {} tokens (whole call/result groups retained).", report.eligible_messages, report.retained_messages, report.keep_recent_tokens));
    if report.notes_reset {
        text.push_str("\nUses saved context notes; the execution owner validates the notes and carry before resetting.");
    } else if report.eligible_messages == 0 {
        text.push_str("\nNo eligible earlier complete history: execution may leave history unchanged without a summary request.");
    }
    if report.methods.iter().any(|m| {
        matches!(
            m,
            CompactionMethod::Soft | CompactionMethod::Handoff | CompactionMethod::Remote
        )
    }) {
        text.push_str(&format!("\nMay make a local or hosted model request, including fallback requests. Portable summary route: {}/{}. Native remote compaction uses the active route{}; local work consumes compute and hosted requests may incur charges. Exact request count, prices and savings are unknown.", report.summary_provider, report.summary_model, if report.remote_endpoint { " or configured compaction endpoint" } else { "" }));
    } else {
        text.push_str("\nNo summary-model request is selected by this method order; local processing may still consume compute.");
    }
    text.push_str("\nPreview only: hooks, retained critical state, refreshed context and method capability/profitability checks are validated at execution and can change this plan or produce a no-op. No exact savings are promised. Queued input and the composer draft remain pending and are not compacted. Cancel leaves history unchanged and does not start inference.");
    text
}

#[cfg(test)]
mod tests;
