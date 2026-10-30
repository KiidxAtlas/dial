//! Routing commands typed while a turn is running, and the hotkey reference.

use crate::app::command_registry::{DuringTurn, canonical_command, command_panel, during_turn};
use crate::app::core::App;
use crate::app::display::{app_home_hint, short};
use crate::app::drive::Step;
use crate::app::improve_view::{improve_jobs_text, improve_stop_text};
use crate::app::overlay::Which;
use crate::app::overlay::auth::AuthJob;
use crate::app::shell::Shell;
use crate::storage::session::Session;
use std::path::Path;

impl Shell {
    /// Shared by inference and local-compute waiting. Only explicit session/work changes, or an
    /// explicit send-now of a queued message, cancel the running turn.
    pub(in crate::app) fn route_busy_command(&mut self, app: &App, text: String) -> bool {
        // Plain Submit while busy is the explicit empty-Enter queue flush.
        // Stop at the next safe point and dispatch it in this same chat, ahead
        // of deferred commands and the remaining queued messages.
        if !crate::ui::composer::is_slash_command(&text) {
            self.startup = Some(text);
            return true;
        }
        let rest = text.trim().trim_start_matches('/');
        let (name, args) = rest
            .split_once(char::is_whitespace)
            .map_or((rest, ""), |(name, args)| (name, args.trim()));
        let name = canonical_command(name);
        let policy = during_turn(name, args);
        if name == "reload" {
            if args.is_empty() {
                self.refresh_resources(app, true, true);
            } else {
                self.note("Reload", "Usage: /reload");
            }
            return false;
        }
        if name == "todo" {
            if let Some(spec) = self.work_plan(args)
                && let Err(error) = self.start_busy_overlay(app, Which::Choice(Box::new(spec)))
            {
                self.note("Work plan", format!("{error:#}"));
            }
            return false;
        }
        if name == "queue" {
            self.queue_command(&text);
            return false;
        }
        if policy == DuringTurn::Unavailable {
            self.note("Model", "Finish or cancel the current turn before restarting its managed model. No server was stopped.");
            return false;
        }
        let overlay = command_panel(name, args);
        if let Some(which) = overlay {
            if let Err(error) = self.start_busy_overlay(app, which) {
                self.note("Panel", format!("{error:#}"));
            }
            return false;
        }
        if policy == DuringTurn::Now && self.live_command(app, &text) {
            return false;
        }
        let name = text.split_whitespace().next().unwrap_or("");
        if policy == DuringTurn::Stop {
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

    /// Run a command that needs only the shell and app, never the agent, so it works while a turn
    /// runs. Returns false for commands that need the agent: those stop the turn
    /// ([`Shell::stops_turn`]) or run right after it.
    fn live_command(&mut self, app: &App, line: &str) -> bool {
        let Some(rest) = line.trim().strip_prefix('/') else {
            return false;
        };
        let (name, args) = rest
            .split_once(char::is_whitespace)
            .map_or((rest, ""), |(name, args)| (name, args.trim()));
        let name = canonical_command(name);
        match (name, args) {
            ("model", args) => {
                let result = self.model_command(app, None, args);
                self.adopt_live(app, "Model", result);
            }
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
            ("help" | "?", _) => {
                let text = self.help_text(args.trim() == "all");
                self.note("Help", text);
            }
            ("hotkeys", "") => {
                let text = self.hotkeys_text();
                self.note("Hotkeys", text);
            }
            ("search", "") => self.note("Search", Self::SEARCH_USAGE),
            ("btw", args) => {
                let snapshot = self.side.busy_snapshot();
                let text = self.side.command(args, snapshot.as_deref());
                self.note("BTW", text);
            }
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
            ("message", rest) => {
                let Some((id, text)) = rest.split_once(char::is_whitespace) else {
                    self.note("Jobs", "Usage: /message <id> <text>");
                    return true;
                };
                let result = match &self.live_tasks {
                    Some(control) => control.send(id, text.trim()),
                    None => Err(anyhow::anyhow!("no live jobs")),
                };
                match result {
                    Ok(value) => self.note("Jobs", value.to_string()),
                    Err(error) => self.note("Jobs", format!("{error:#}")),
                }
            }
            ("cancel", id) => {
                if id.is_empty() {
                    self.note("Jobs", "Usage: /cancel <id>");
                    return true;
                }
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
            ("archive", args) => self.archive_note(app, args),
            ("changes", "") => self.changes_note(&app.layout.project_root),
            ("outcome", args) => self.outcome_command(app, args),
            ("code-history", args) if args.trim() != "clear" => self.code_history(app, args),
            ("understanding", "clear") => self.understanding_clear(app),
            ("skills", args) => self.skills_command(app, args),
            ("theme", args) => {
                let result = self.theme_command(app, None, args);
                self.adopt_live(app, "Theme", result);
            }
            ("profile", args) => {
                let result = self.profile_command(app, None, args);
                self.adopt_live(app, "Profile", result);
            }
            ("laya", "on" | "off") => {
                let result = self.laya_toggle(app, None, args == "on");
                self.adopt_live(app, "Laya", result);
            }
            _ => {
                let name = canonical_command(name);
                let result = match crate::app::commands::run_live(self, app, name, args) {
                    Some(result) => result,
                    None => match crate::app::commands::run_settings(self, app, None, name, args) {
                        Some(result) => result,
                        None => return false,
                    },
                };
                self.adopt_live(app, name, result);
            }
        }
        true
    }

    /// A command run during a turn may ask for a rebuild; hold it for the turn's end.
    fn adopt_live(&mut self, app: &App, title: &str, result: anyhow::Result<Step>) {
        match result {
            Ok(step) => self.adopt_after_turn(app, step),
            Err(error) => self.note(title, format!("{error:#}")),
        }
    }

    /// Background jobs (usage, sign-in, installs) stream progress and report while a turn runs.
    /// Jobs whose result continues an agent-bound flow wait for the idle loop.
    pub(in crate::app) fn pump_jobs_during_turn(&mut self, app: &App) {
        for (label, line) in AuthJob::drain_lines(&mut self.auth_job) {
            self.job_line(app, label, &line);
        }
        self.sync_live_pane(app, None);
        if self
            .auth_job
            .as_ref()
            .is_some_and(|job| matches!(job.label, "Smithery" | "Setup" | "Model"))
        {
            return;
        }
        if let Some((label, result)) = AuthJob::collect(&mut self.auth_job, app) {
            self.job_ended(label, &result);
            self.report_job_result(label, result);
            self.transcript.settle_job_notices();
            self.catalog.refresh(app);
        }
    }

    /// The transcript report for a finished background job that needs no follow-up.
    pub(in crate::app) fn report_job_result(
        &mut self,
        label: &'static str,
        result: anyhow::Result<String>,
    ) {
        match result {
            Ok(message) if message.is_empty() => {
                if self.transcript.job_waiting(label) {
                    self.transcript.notice_merge(label, "Done.");
                }
            }
            // Streamed output already said everything; a bare "Finished." adds nothing.
            Ok(message) if message == "Finished." && self.transcript.notice_has_output(label) => {}
            Ok(text) if label == crate::app::commands::extensions::MCP_PROMPT_LABEL => {
                let draft =
                    crate::app::commands::extensions::composed_draft(&self.composer.draft(), &text);
                self.composer.set_draft(&draft);
                self.transcript.notice_merge(
                    label,
                    "Put the prompt in the composer. Review it, then send.",
                );
            }
            Ok(message) => self.transcript.notice_merge(label, &message),
            Err(error) => self.transcript.notice_merge(label, &format!("{error:#}")),
        }
    }

    /// `/archive [sessions|prompts|projects|<project>]`: the user-wide history archive.
    pub(super) fn archive_note(&mut self, app: &App, args: &str) {
        let archive = crate::storage::archive::Archive::new(&app.layout.user_dir);
        let result = match args.trim() {
            "sessions" => archive.sessions(None, 30).map(|view| view.text),
            "prompts" => archive.prompts(None, None, 30).map(|view| view.text),
            "projects" | "" => archive.projects(30).map(|view| view.text),
            other => archive.prompts(Some(other), None, 30).map(|view| view.text),
        };
        match result {
            Ok(text) if text.trim().is_empty() => self.note("Archive", "no archived history"),
            Ok(text) => self.note("Archive", text),
            Err(error) => self.note("Archive", format!("{error:#}")),
        }
    }

    /// `/changes`: files changed in the current Git snapshot of `root`.
    pub(super) fn changes_note(&mut self, root: &Path) {
        let files = self.git.files(root);
        let text = if files.is_empty() {
            "No changes in the current Git snapshot.".into()
        } else {
            files
                .iter()
                .map(|file| {
                    let stats = if file.untracked {
                        "new".into()
                    } else {
                        match (file.added, file.removed) {
                            (Some(a), Some(r)) => format!("+{a} −{r}"),
                            _ => "binary".into(),
                        }
                    };
                    format!("{}  {stats}", crate::ui::text::one_line(&file.path))
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        self.note("Changes", text);
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
