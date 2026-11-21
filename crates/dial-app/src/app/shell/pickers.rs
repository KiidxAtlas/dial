//! Building the interactive pickers the composer and commands open.

use crate::agent::Agent;
use crate::app::core::App;
use crate::app::selection::{reasoning_profile, thinking_label};
use crate::app::shell::Shell;
use crate::config::types::Approval;
use crate::ui::overlay::palette::{ChoiceItem, ChoiceSpec};

/// Rules as picker rows, numbered the way `/permissions remove <number>` expects, with the count
/// of saved rules. A rule the project file holds is saved: its row adds `--persist`, so removing
/// it revokes it from the project. Any other rule is session-only: removing it only forgets it.
fn rule_items(agent: &Agent<'_>) -> (usize, Vec<ChoiceItem>) {
    let saved = agent
        .layout()
        .and_then(|layout| {
            crate::agent::safety::approval::load(&layout.state_dir.join("permissions.toml")).ok()
        })
        .unwrap_or_default();
    let mut saved_count = 0;
    let items = agent
        .permission_rules()
        .iter()
        .enumerate()
        .map(|(index, rule)| {
            let is_saved = saved.contains(rule);
            saved_count += usize::from(is_saved);
            let paths: String = rule
                .paths
                .iter()
                .map(|path| format!(" --path {path}"))
                .collect();
            ChoiceItem {
                value: format!("{}{}", index + 1, if is_saved { " --persist" } else { "" }),
                label: format!(
                    "{}. {} {}",
                    index + 1,
                    rule.tool,
                    rule.command_prefix.join(" ")
                )
                .trim_end()
                .to_owned(),
                detail: format!(
                    "{} · {:?}{paths}",
                    if is_saved {
                        "Saved in this project"
                    } else {
                        "This session only"
                    },
                    rule.effect
                ),
                current: false,
            }
        })
        .collect();
    (saved_count, items)
}

impl Shell {
    pub(in crate::app) fn side_thread_picker(&self) -> ChoiceSpec {
        let mut items: Vec<_> = self
            .side
            .threads()
            .iter()
            .map(|thread| ChoiceItem {
                value: format!("threads actions {}", thread.id),
                label: thread.title.clone(),
                detail: format!("{} · {} exchange(s)", thread.id, thread.exchanges.len()),
                current: false,
            })
            .collect();
        items.push(ChoiceItem {
            value: "threads new".into(),
            label: "New side question".into(),
            detail: "Write a question in the composer without changing the main conversation."
                .into(),
            current: false,
        });
        ChoiceSpec {
            title: "Side threads".into(),
            command: "btw".into(),
            items,
            summary: "Choose a thread to open, copy, cancel or promote; or start a new question."
                .into(),
            ..Default::default()
        }
    }

    /// Picker-only actions resolve to the existing commands; no thread semantics change.
    pub(in crate::app) fn side_thread_action(&mut self, args: &str) -> Option<String> {
        if args == "threads new" {
            if self.composer.draft().trim().is_empty() {
                self.composer.set_draft("/btw new ");
            } else {
                self.note(
                    "BTW",
                    "Your draft is kept. Send /btw new <question> to start a new side thread.",
                );
            }
            return None;
        }
        let Some(id) = args.strip_prefix("threads actions ") else {
            return Some(args.to_owned());
        };
        let choice = self.ask("Side thread", "Opening or copying leaves the main conversation unchanged. Promotion creates an independent full session.", &[("Open", 'o'), ("Copy answer", 'c'), ("Cancel streaming answer", 'x'), ("Promote to session", 'p'), ("New question", 'n')])?;
        match choice {
            0 => Some(format!("open {id}")),
            1 => {
                let text = crate::app::side::command(&mut self.side, &format!("open {id}"), None);
                if text.starts_with("error:") {
                    self.note("BTW", text);
                    return None;
                }
                Some("copy".into())
            }
            2 => Some("cancel".into()),
            3 => Some(format!("promote {id}")),
            _ => {
                self.side_thread_action("threads new");
                None
            }
        }
    }
    /// The value picker a bare command opens in the TUI: `/approval`, `/effort`, `/profile`,
    /// `/theme` and `/permissions remove`. `None` lets normal command routing continue.
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
        // `summary` heads the list and says what choosing does; `prompt` names what is filtered.
        let picker = |title: &str,
                      command: &str,
                      verb: &'static str,
                      remove: Option<&'static str>,
                      items: Vec<ChoiceItem>,
                      (prompt, summary): (&str, String)| ChoiceSpec {
            title: title.to_owned(),
            command: command.to_owned(),
            verb,
            remove,
            items,
            prompt: prompt.to_owned(),
            summary,
            ..Default::default()
        };
        match (name, args) {
            ("approval", "") => {
                let current = format!("{:?}", agent.approval()).to_lowercase();
                let items = Approval::NAMES
                    .iter()
                    .map(|value| {
                        let mode: Approval = serde_json::from_value(serde_json::json!(value))
                            .expect("approval metadata");
                        ChoiceItem {
                            label: crate::ui::status::approval_label(value).to_owned(),
                            detail: approval_detail(value, mode),
                            ..pick(value, mode.description(), *value == current)
                        }
                    })
                    .collect();
                Some(picker(
                    "Approval",
                    "approval",
                    "",
                    None,
                    items,
                    (
                        "Filter modes:",
                        format!(
                            "When Dial asks before acting; now {}",
                            crate::ui::status::approval_label(&current)
                        ),
                    ),
                ))
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
                let now = if app.config.thinking_auto {
                    "auto".to_owned()
                } else {
                    level.to_string()
                };
                Some(picker(
                    "Thinking effort",
                    "effort",
                    "",
                    None,
                    items,
                    (
                        "Filter levels:",
                        format!("How much the model reasons per prompt; now {now}"),
                    ),
                ))
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
                Some(picker(
                    "Capability profile",
                    "profile",
                    "",
                    None,
                    items,
                    (
                        "Filter profiles:",
                        format!("Model preset; now {}", active.unwrap_or("off")),
                    ),
                ))
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
                Some(picker(
                    "Theme",
                    "theme",
                    "",
                    None,
                    items,
                    ("Filter themes:", format!("Colors and glyphs; now {active}")),
                ))
            }
            ("permissions", "") => {
                let (saved, rules) = rule_items(agent);
                let session = rules.len() - saved;
                let mut items = vec![
                    ChoiceItem {
                        value: "allow".into(),
                        label: "Allow for this session…".into(),
                        detail: "Fills the composer with /permissions allow bash; type the command and send it.".into(),
                        current: false,
                    },
                    ChoiceItem {
                        value: "allow --persist".into(),
                        label: "Always allow in this project…".into(),
                        detail: "Fills the composer with /permissions allow --persist bash; saved to the project.".into(),
                        current: false,
                    },
                ];
                items.extend(rules);
                let mut spec = picker(
                    "Permission rules",
                    "permissions",
                    "",
                    Some("remove"),
                    items,
                    (
                        "Filter rules:",
                        format!(
                            "{saved} saved · {session} this session. Delete or x twice removes a rule: saved rules are revoked, session rules are forgotten."
                        ),
                    ),
                );
                // The two add rows are actions; Delete on them must not remove anything.
                spec.fixed = &["allow", "allow --persist"];
                Some(spec)
            }
            ("permissions", "remove") => {
                let (_, items) = rule_items(agent);
                let count = items.len();
                (!items.is_empty()).then(|| {
                    picker(
                        "Remove a permission rule",
                        "permissions",
                        "remove",
                        None,
                        items,
                        (
                            "Filter rules:",
                            format!(
                                "{count} saved {}; Enter removes the selected one. {}",
                                if count == 1 { "rule" } else { "rules" },
                                super::command::PERMISSION_PATH_SCOPE
                            ),
                        ),
                    )
                })
            }
            _ => None,
        }
    }

    /// A read-only plan browser shared by idle commands and the live-turn command route.
    /// Detail panels return to the plan; cancelling either layer never alters the agent's tasks.
    pub(super) fn work_plan(
        &mut self,
        args: &str,
        progress: Option<&serde_json::Value>,
    ) -> Option<ChoiceSpec> {
        if args == "list" {
            let body = format!(
                "{}\n\n{}",
                self.todo.render(),
                crate::ui::tasks::progress_text(progress)
            );
            self.note("Work plan", body.trim_start());
            return None;
        }
        if args == "progress" {
            self.note(
                "Deliverables and checks",
                crate::ui::tasks::progress_text(progress),
            );
            return None;
        }
        let mut selected = None;
        if !args.is_empty() {
            let index = args
                .strip_prefix("show ")
                .and_then(|number| number.trim().parse::<usize>().ok())
                .and_then(|number| number.checked_sub(1));
            let Some(task) = index.and_then(|index| self.todo.tasks().get(index)) else {
                self.note("Work plan", "Use /todo to browse, /todo list for text, /todo progress for deliverables and checks, or /todo show <number>. No task state changed.");
                return None;
            };
            let detail = crate::ui::tasks::step_detail(task);
            self.ask("Step details", &detail, &[("Back to work plan", 'b')])?;
            selected = index;
        }
        let mut plan = Some(
            crate::ui::tasks::plan_picker(&self.todo).unwrap_or_else(|| ChoiceSpec {
                title: "Work plan".into(),
                command: "todo".into(),
                verb: "show",
                summary: "No plan recorded yet.".into(),
                ..Default::default()
            }),
        );
        if let Some(plan) = &mut plan {
            plan.items.push(ChoiceItem {
                value: "__progress".into(),
                label: "Progress".into(),
                detail: crate::ui::tasks::progress_text(progress),
                current: false,
            });
        }
        if let (Some(plan), Some(index)) = (&mut plan, selected) {
            for item in &mut plan.items {
                item.current = item.value == (index + 1).to_string();
            }
        }
        if plan.is_none() {
            self.note("Work plan", "No plan recorded yet. Dial creates steps when work needs a checklist; you can keep writing in the composer.");
        }
        plan
    }
}

/// A picker row for an approval mode: its name and what it does. YOLO's row is marked as a risk,
/// since it is the mode that also runs calls recovered from model text unasked. The picker is the
/// explicit way to choose it; Shift+Tab never reaches it.
fn approval_detail(value: &str, mode: Approval) -> String {
    match mode {
        Approval::Yolo => format!("{value} · RISK: {}", mode.description()),
        _ => format!("{value} · {}", mode.description()),
    }
}

#[cfg(test)]
mod tests;
