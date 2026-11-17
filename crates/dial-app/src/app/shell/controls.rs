//! A doorway into existing session commands and persistent settings editors.
use super::Shell;
use crate::config::types::Config;
use crate::ui::overlay::palette::{ChoiceItem, ChoiceSpec};
use crate::ui::status::{Phase, approval_label};

impl Shell {
    pub(super) fn controls_picker(&self, saved: &Config) -> ChoiceSpec {
        let status = &self.status;
        let on = |value| match value {
            Some(true) => "on",
            Some(false) => "off",
            None => "unknown",
        };
        let item = |command: &str, label: &str, detail: String| ChoiceItem {
            value: command.into(),
            label: label.into(),
            detail,
            current: false,
        };
        let reasoning = if status.thinking.is_empty() {
            "provider default"
        } else {
            &status.thinking
        };
        let saved_reasoning = crate::app::selection::reasoning_profile(saved)
            .ok()
            .flatten()
            .as_ref()
            .map(crate::app::selection::thinking_label)
            .unwrap_or_else(|| "provider default".into());
        let mut items = vec![item(
            "approval",
            "Approval · session",
            format!(
                "Active: {} · choose explicitly; planning enforces read-only",
                approval_label(&status.approval)
            ),
        )];
        // One planning row: the explicit command that changes the current state. Both appear
        // only while the state is unknown, so an entry never offers what is already active.
        if status.planning != Some(true) {
            items.push(item(
                "plan on",
                "Start planning · session",
                format!(
                    "Active: {} · read-only planning until you leave it",
                    on(status.planning)
                ),
            ));
        }
        if status.planning != Some(false) {
            items.push(item(
                "plan off",
                "Leave planning · session",
                format!(
                    "Active: {} · return to the approval mode above",
                    on(status.planning)
                ),
            ));
        }
        items.extend([
            item(
                "effort",
                "Reasoning · main model",
                format!(
                    "Active: {reasoning} · saved: {saved_reasoning} · open the main model's effort choices"
                ),
            ),
            item(
                "settings assistance.enabled",
                "Assistance · persistent default",
                format!(
                    "Active: {} · saved: {} · edit defaults in Settings",
                    on(status.assistance),
                    on(Some(saved.assistance.enabled))
                ),
            ),
            item(
                "model",
                "Model",
                format!(
                    "Active: {}/{} · saved: {} · Why this main model (recorded): {}",
                    status.provider,
                    status.model,
                    saved.selection().display_label(),
                    status.main_model_explanation.as_deref().unwrap_or("unavailable (not recorded)")
                ),
            ),
            item(
                "context",
                "Context · current session",
                format!(
                    "Active: {} · inspect what fills it",
                    context_use(status.prompt_tokens, u64::from(status.context_window))
                ),
            ),
            item(
                "providers",
                "Provider connections",
                format!(
                    "Active provider: {} · credentials and connection editor",
                    status.provider
                ),
            ),
            item(
                "model defaults",
                "Context limits · persistent defaults",
                "Edit the saved context limits, not what this session uses now".into(),
            ),
            item(
                "settings",
                "All persistent settings",
                "Choose scope in Settings; saving is not the same as adopting agent settings"
                    .into(),
            ),
            item(
                "open:tool-evidence",
                "Inspect tool evidence",
                "Read one retained call's full output and arguments without expanding other tool cards".into(),
            ),
            // Explanations last: they change nothing, so they follow the controls that do.
            item(
                "help stop-scopes",
                "Stop · understand the scope",
                "Read only: current answer, one worker, session jobs and persistent jobs; nothing is stopped".into(),
            ),
            item(
                "help undo-meanings",
                "Undo · files, history or settings?",
                "Read only: compare rollback, conversation rewind and removing a settings override; nothing is undone".into(),
            ),
        ]);
        // The status line's facts become keyboard targets here: each row names its direct key, so
        // the next time the key alone does it.
        for (value, action) in [
            ("approval", crate::ui::keys::Action::ApprovalCycle),
            ("model", crate::ui::keys::Action::ModelSelect),
            ("providers", crate::ui::keys::Action::ProviderSetup),
        ] {
            let key = self.keys.label(action);
            if !key.is_empty()
                && let Some(row) = items.iter_mut().find(|row| row.value == value)
            {
                row.detail.push_str(&format!(" · key {key}"));
            }
        }
        // Saved values are never projected into the active snapshot while the agent is lent out.
        if status.phase != Phase::Idle {
            let timing = "Saved defaults; agent changes apply after this turn (not active now)";
            for row in &mut items {
                let differs = match row.value.as_str() {
                    "settings assistance.enabled" => {
                        status.assistance != Some(saved.assistance.enabled)
                    }
                    "model" => {
                        saved.selection().display_label()
                            != format!(
                                "{}/{}",
                                status.provider,
                                status.model.trim_end_matches(" [plan]")
                            )
                    }
                    "effort" => saved_reasoning != reasoning,
                    _ => false,
                };
                if differs {
                    row.detail.push_str(&format!(" · {timing}"));
                }
                if row.value == "model" {
                    row.detail.push_str(" · Snapshot at last agent boundary; in-flight fallback/promotion appears in model events");
                }
            }
        }
        ChoiceSpec {
            title: "Session controls & status".into(),
            summary: "Active session facts; persistent defaults are marked as such. Enter opens the owning command or editor; Esc returns to your draft.".into(),
            prompt: "Find a control:".into(),
            items,
            ..Default::default()
        }
    }

    pub(super) fn tool_evidence_picker(&self) -> ChoiceSpec {
        ChoiceSpec {
            title: "Inspect tool evidence".into(),
            summary: "Select one retained call. Inspection is read only; Ctrl+O in the transcript still expands all cards.".into(),
            empty_message: "No retained tool calls in this conversation yet.".into(),
            items: self.transcript.retained_tool_calls().into_iter().map(|(id, name, summary)| ChoiceItem {
                value: format!("open:tool-evidence:{}", serde_json::to_string(&id).expect("string serialization")),
                label: format!("{} · {}", crate::ui::text::one_line(&name), crate::ui::text::one_line(&id)),
                detail: crate::ui::text::one_line(&summary),
                current: false,
            }).collect(),
            ..Default::default()
        }
    }
}

/// `12.3k of 200k tokens (6%)`, or `unknown` when nothing has been measured yet.
fn context_use(used: Option<u64>, window: u64) -> String {
    let short = |n: u64| {
        if n >= 1_000_000 {
            format!("{:.1}M", n as f64 / 1_000_000.0)
        } else if n >= 10_000 {
            format!("{}k", n / 1_000)
        } else if n >= 1_000 {
            format!("{:.1}k", n as f64 / 1_000.0)
        } else {
            n.to_string()
        }
    };
    match (used, window) {
        (None, 0) => "unknown".into(),
        (None, window) => format!("unknown of {} tokens", short(window)),
        (Some(used), 0) => format!("{} tokens", short(used)),
        (Some(used), window) => format!(
            "{} of {} tokens ({}%)",
            short(used),
            short(window),
            (used * 100 / window).min(999)
        ),
    }
}

#[cfg(test)]
mod tests;
