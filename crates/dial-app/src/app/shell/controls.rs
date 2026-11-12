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
        let mut items = vec![
            item(
                "approval",
                "Approval · session",
                format!(
                    "Active: {} · choose explicitly; planning enforces read-only",
                    approval_label(&status.approval)
                ),
            ),
            item(
                "plan on",
                "Planning on · session",
                format!(
                    "Active: {} · explicitly enable read-only planning",
                    on(status.planning)
                ),
            ),
            item(
                "plan off",
                "Planning off · session",
                format!(
                    "Active: {} · explicitly leave planning",
                    on(status.planning)
                ),
            ),
            item(
                "effort",
                "Reasoning(main)",
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
                    "Active: {} / {} tokens · inspect context",
                    status
                        .prompt_tokens
                        .map_or_else(|| "unknown".into(), |n| n.to_string()),
                    status.context_window
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
                "Context · persistent defaults",
                "Edit defaults separately from current context occupancy".into(),
            ),
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
            item(
                "settings",
                "All persistent settings",
                "Choose scope in Settings; saving is not the same as adopting agent settings"
                    .into(),
            ),
        ];
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
            summary: "Active session facts above; persistent defaults stay separate. Enter opens the owning command/editor; Esc returns to your untouched draft.".into(),
            prompt: "Find a control:".into(),
            items,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests;
