//! Routing commands typed while a turn is running, and the hotkey reference.

use crate::app::command_registry::RUNS_NOW;
use crate::app::core::App;
use crate::app::display::{app_home_hint, short};
use crate::app::improve_view::{improve_jobs_text, improve_stop_text};
use crate::app::overlay::{Which, work_panel};
use crate::app::shell::Shell;
use crate::storage::session::Session;

impl Shell {
    /// Shared by inference and local-compute waiting. Only explicit session/work changes, or an
    /// explicit send-now of a queued message, cancel the running turn.
    pub(in crate::app) fn route_busy_command(&mut self, app: &App, text: String) -> bool {
        // Plain Submit while busy is the explicit empty-Enter queue flush.
        // Stop at the next safe point and dispatch it in this same chat, ahead
        // of deferred commands and the remaining queued messages.
        if !text.trim_start().starts_with('/') {
            self.startup = Some(text);
            return true;
        }
        if text.split_whitespace().next() == Some("/reload") {
            if text.trim() == "/reload" {
                self.refresh_resources(app, true, true);
            } else {
                self.note("Reload", "Usage: /reload");
            }
            return false;
        }
        if let Some(rest) = text.trim().strip_prefix("/todo")
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            if let Some(spec) = self.work_plan(rest.trim())
                && let Err(error) = self.start_busy_overlay(app, Which::Choice(Box::new(spec)))
            {
                self.note("Work plan", format!("{error:#}"));
            }
            return false;
        }
        if text.split_whitespace().next().is_some_and(|word| {
            RUNS_NOW.contains(&word.trim_start_matches('/')) && word == "/queue"
        }) {
            self.queue_command(&text);
            return false;
        }
        if text.trim() == "/model restart" {
            self.note("Model", "Finish or cancel the current turn before restarting its managed model. No server was stopped.");
            return false;
        }
        let overlay = match text.trim() {
            "/palette" => Some(Which::Palette),
            "/model" | "/models" => Some(Which::Models),
            "/roles" => Some(Which::ModelRoles),
            "/model settings" => Some(Which::ModelSettings),
            "/model defaults" => Some(Which::ContextDefaults),
            "/providers" | "/provider" => Some(Which::Providers),
            "/settings" => Some(Which::Settings),
            "/model-settings" => Some(Which::ModelSettings),
            "/extensions" => Some(Which::Extensions),
            _ => text.trim().strip_prefix('/').and_then(|rest| {
                let (name, args) = rest
                    .split_once(char::is_whitespace)
                    .map_or((rest, ""), |(name, args)| (name, args.trim()));
                work_panel(name, args)
            }),
        };
        if let Some(which) = overlay {
            if let Err(error) = self.start_busy_overlay(app, which) {
                self.note("Panel", format!("{error:#}"));
            }
            return false;
        }
        if self.live_command(app, &text) {
            return false;
        }
        let name = text.split_whitespace().next().unwrap_or("");
        if Self::stops_turn(&text) {
            self.note(
                "Command",
                format!("stopping the current turn to run {name}"),
            );
            self.startup = Some(text);
            true
        } else {
            self.note(
                "Command",
                format!("{name} will run when this turn finishes"),
            );
            self.deferred.push_back(text);
            false
        }
    }

    /// Run a command that only reads what the shell already holds, so it works while a turn runs. Returns false
    /// for commands that need the agent: those stop the turn ([`Shell::stops_turn`]) or run right after it.
    fn live_command(&mut self, app: &App, line: &str) -> bool {
        let Some(rest) = line.trim().strip_prefix('/') else {
            return false;
        };
        let (name, args) = rest
            .split_once(char::is_whitespace)
            .map_or((rest, ""), |(name, args)| (name, args.trim()));
        match (name, args) {
            ("reliability", "" | "status") => {
                self.note(
                    "Tool reliability",
                    crate::reliability::render(&crate::reliability::live_report(
                        &app.config,
                        &self.request_policy,
                    )),
                );
            }
            ("policy", "" | "status") => {
                self.note(
                    "Adaptive policy",
                    crate::adaptive::render(&crate::adaptive::report(
                        &app.config,
                        &self.request_policy,
                    )),
                );
            }
            ("help" | "?", "" | "all") => {
                let text = self.help_text(args.trim() == "all");
                self.note("Help", text);
            }
            ("hotkeys", "") => {
                let text = self.hotkeys_text();
                self.note("Hotkeys", text);
            }
            ("search", "") => self.note("Search", Self::SEARCH_USAGE),
            ("search", query) if !query.is_empty() => {
                match Session::search(&app.layout, query, 8) {
                    Ok(hits) if hits.is_empty() => {
                        self.note("Search", format!("No past messages match \"{query}\"."))
                    }
                    Ok(hits) => {
                        let text = hits
                            .iter()
                            .map(|hit| {
                                format!("{} [{}] {}", short(&hit.session_id), hit.role, hit.snippet)
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        self.note("Search", text);
                    }
                    Err(error) => self.note("Search", format!("error: {error:#}")),
                }
            }
            ("improve", "status") => {
                let text = self.improve_status_text(app);
                self.note("Improvement", text);
            }
            ("improve", "jobs") => {
                let text = improve_jobs_text(app);
                self.note("Improvement", text);
            }
            ("improve", rest) if rest == "stop" || rest.starts_with("stop ") => {
                let text = improve_stop_text(app, rest.trim_start_matches("stop"));
                self.note("Improvement", text);
            }
            ("jobs", "") => {
                let text = match self.live_tasks.as_ref().map(|control| control.list()) {
                    Some(jobs) if !jobs.is_empty() => jobs
                        .iter()
                        .map(|(id, running)| {
                            format!("{id}: {}", if *running { "running" } else { "finished" })
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => "no live jobs".to_owned(),
                };
                self.note("Jobs", text);
            }
            ("jobs" | "workers", id) => {
                let result = match &self.live_tasks {
                    Some(control) => control.status(Some(id)),
                    None => Err("no live jobs".to_owned()),
                };
                match result {
                    Ok(value) => self.note("Jobs", value.to_string()),
                    Err(error) => self.note("Jobs", error),
                }
            }
            ("message", rest) if rest.split_once(char::is_whitespace).is_some() => {
                let (id, text) = rest.split_once(char::is_whitespace).unwrap_or_default();
                let result = match &self.live_tasks {
                    Some(control) => control.send(id, text.trim()),
                    None => Err(anyhow::anyhow!("no live jobs")),
                };
                match result {
                    Ok(value) => self.note("Jobs", value.to_string()),
                    Err(error) => self.note("Jobs", format!("{error:#}")),
                }
            }
            ("cancel", id) if !id.is_empty() => {
                let result = match &self.live_tasks {
                    Some(control) => control.cancel(id),
                    None => Err(anyhow::anyhow!("no live jobs")),
                };
                match result {
                    Ok(value) => self.note("Jobs", value.to_string()),
                    Err(error) => self.note("Jobs", format!("{error:#}")),
                }
            }
            ("skills", "") => {
                self.note("Skills", self.skills_listing.clone());
            }
            ("commands", "") => {
                let text = if self.commands.is_empty() {
                    "no custom commands (add .dial/commands/<name>.md)".to_owned()
                } else {
                    self.commands
                        .iter()
                        .map(|def| format!("/{}  {}", def.name, def.description))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.note("Commands", text);
            }
            ("context", "") => {
                let used = self
                    .status
                    .prompt_tokens
                    .map_or_else(|| "unknown".to_owned(), |n| n.to_string());
                self.note(
                    "Context",
                    format!(
                        "last prompt: {used} tokens (window {})",
                        self.status.context_window
                    ),
                );
            }
            _ => return false,
        }
        true
    }

    fn hotkeys_text(&self) -> String {
        let mut lines = Vec::new();
        let mut group = "";
        for row in self.keys.hotkeys() {
            if row.group != group {
                if !lines.is_empty() {
                    lines.push(String::new());
                }
                lines.push(row.group.to_owned());
                group = row.group;
            }
            lines.push(format!(
                "  {:<24} {:<22} {}{}",
                row.id,
                row.keys,
                row.description,
                if row.customized { "  (customized)" } else { "" }
            ));
        }
        lines.push(String::new());
        lines.push("Run /hotkeys when idle to open the shortcut editor.".to_owned());
        lines.push(format!(
            "Remap in {}/keybindings.toml, for example:  \"app.model.select\" = \"Ctrl+K\"",
            app_home_hint()
        ));
        lines.join("\n")
    }
}
