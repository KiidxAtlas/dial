//! Building the interactive pickers the composer and commands open.

use crate::agent::Agent;
use crate::app::core::App;
use crate::app::selection::{reasoning_profile, thinking_label};
use crate::app::shell::Shell;
use crate::config::types::Approval;
use crate::ui::overlay::palette::{ChoiceItem, ChoiceSpec};

impl Shell {
    /// The value picker a bare command opens in the TUI: `/approval`, `/effort`, `/profile`,
    /// `/theme`, `/queue` and `/permissions remove`. `None` keeps the command's text output.
    pub(super) fn picker(
        &self,
        app: &App,
        agent: &Agent<'_>,
        name: &str,
        args: &str,
    ) -> Option<ChoiceSpec> {
        let pick = |value: &str, detail: &str, current: bool| ChoiceItem {
            value: value.to_owned(),
            label: value.to_owned(),
            detail: detail.to_owned(),
            current,
        };
        let picker = |title: &str,
                      command: &str,
                      verb: &'static str,
                      remove: Option<&'static str>,
                      items: Vec<ChoiceItem>| ChoiceSpec {
            title: title.to_owned(),
            command: command.to_owned(),
            verb,
            remove,
            items,
        };
        match (name, args) {
            ("approval", "") => {
                let current = format!("{:?}", agent.approval()).to_lowercase();
                let items = Approval::NAMES
                    .iter()
                    .map(|value| {
                        let mode: Approval = serde_json::from_value(serde_json::json!(value))
                            .expect("approval metadata");
                        pick(value, mode.description(), *value == current)
                    })
                    .collect();
                Some(picker("Approval", "approval", "", None, items))
            }
            ("effort", "") => {
                let profile = reasoning_profile(&app.config).ok().flatten()?;
                let level = thinking_label(&profile);
                let mut items: Vec<ChoiceItem> = profile
                    .capabilities
                    .reasoning_efforts
                    .iter()
                    .map(|effort| {
                        let current = !app.config.thinking_auto && effort.as_str() == level;
                        pick(effort.as_str(), "", current)
                    })
                    .collect();
                items.push(pick(
                    "auto",
                    "Dial judges each prompt's thinking level",
                    app.config.thinking_auto,
                ));
                Some(picker("Thinking effort", "effort", "", None, items))
            }
            ("profile", "") => {
                let active = app.config.model_profile.as_deref();
                let mut items: Vec<ChoiceItem> = app
                    .config
                    .model_profiles
                    .iter()
                    .map(|(key, profile)| {
                        let detail = format!("{} / {}", profile.provider, profile.model);
                        pick(key, &detail, active == Some(key.as_str()))
                    })
                    .collect();
                items.push(pick("off", "no profile", active.is_none()));
                Some(picker("Capability profile", "profile", "", None, items))
            }
            ("theme", "") => {
                let active = self.themes.current().name().to_owned();
                let items = self
                    .themes
                    .names()
                    .into_iter()
                    .filter(|entry| entry.error.is_none())
                    .map(|entry| pick(&entry.name, "", entry.name == active))
                    .collect();
                Some(picker("Theme", "theme", "", None, items))
            }
            ("queue", "") => {
                let items: Vec<ChoiceItem> = self
                    .composer
                    .queued_messages()
                    .into_iter()
                    .enumerate()
                    .map(|(index, (message, follow_up))| ChoiceItem {
                        value: (index + 1).to_string(),
                        label: format!("{}. {}", index + 1, crate::ui::text::one_line(&message)),
                        detail: if follow_up {
                            "follow-up for after the turn"
                        } else {
                            "steering the current turn"
                        }
                        .to_owned(),
                        current: false,
                    })
                    .collect();
                (!items.is_empty())
                    .then(|| picker("Queued messages", "queue", "edit", Some("remove"), items))
            }
            ("permissions", "remove") => {
                let items: Vec<ChoiceItem> = agent
                    .permission_rules()
                    .iter()
                    .enumerate()
                    .map(|(index, rule)| ChoiceItem {
                        value: (index + 1).to_string(),
                        label: format!(
                            "{}. {} {}",
                            index + 1,
                            rule.tool,
                            rule.command_prefix.join(" ")
                        )
                        .trim_end()
                        .to_owned(),
                        detail: format!("{:?}", rule.effect),
                        current: false,
                    })
                    .collect();
                (!items.is_empty()).then(|| {
                    picker(
                        "Remove a permission rule",
                        "permissions",
                        "remove",
                        None,
                        items,
                    )
                })
            }
            _ => None,
        }
    }
}
