//! The `/command` dispatcher: routes each slash command to its handler.

use crate::agent::window::context;
use crate::agent::{Agent, Ui};
use crate::app::bridge::Bridge;
use crate::app::command_registry::{canonical_command, command_panel, turn_count};
use crate::app::core::App;
use crate::app::display::short;
use crate::app::drive::Step;
use crate::app::notices::memory_status_text;
use crate::app::overlay::Which;
use crate::app::selection::{PendingFavorite, cycle_set, next_favorite};
use crate::app::shell::{PAINT_EVERY, Shell};
use crate::config::selection::changes as selection_changes;
use crate::config::types::{Approval, Outcome};
use crate::llm::usage;
use crate::platform;
use crate::storage::session::{Session, SessionMeta};
use crate::storage::skills;
use crate::ui::keys::Action;
use crate::ui::status::Phase;
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Native path rules are not a shell sandbox or a cross-tool filesystem policy.
pub(super) const PERMISSION_PATH_SCOPE: &str = "Path rules apply only to native read, write, edit and ast_edit tools. Denying read --path '*.pem' does not block bash cat, grep, glob or other tools. Deny those tools explicitly or use filesystem confinement to protect those files.";

impl Shell {
    /// Favorites only need the saved settings and discovery snapshot. The active agent is optional:
    /// a live switch saves now and is adopted through the existing end-of-turn settings boundary.
    pub(super) fn model_command(
        &mut self,
        app: &App,
        agent: Option<&mut Agent<'_>>,
        args: &str,
    ) -> Result<Step> {
        match args {
            "cancel" => {
                self.pending_favorite = None;
                self.note("Favorites", "Pending switch cancelled.");
            }
            "next" => {
                self.pending_favorite = None;
                let effective = self.settings_config(app).clone();
                let favorites = cycle_set(&effective);
                if favorites.is_empty() {
                    self.note("Favorites", "No favorites saved. Use Ctrl-F in /models to save one.");
                    return Ok(Step::Done);
                }
                let Some(discovery) = self.catalog.get() else {
                    self.catalog.start(app);
                    self.pending_favorite = Some(PendingFavorite {
                        main: effective.selection(),
                        favorites,
                        scope: self.scope,
                        catalog_request: self.catalog.request,
                    });
                    self.note("Favorites", "Discovering available models; the favorite switch will apply when ready. Esc or /model cancel cancels it.");
                    return Ok(Step::Done);
                };
                let available = discovery.models.iter()
                    .filter(|entry| entry.available && entry.supports_chat())
                    .map(|entry| entry.selection.clone()).collect::<Vec<_>>();
                match next_favorite(&effective, &favorites, &available) {
                    Some(selection) => {
                        let changes = selection_changes(&effective, &selection, "main")?;
                        // Keep the explicit main-role persistence used by the idle command.
                        let result = app.save_settings(self.scope, &changes, Some("main"))
                            .and_then(|_| app.reload());
                        match result {
                            Ok(fresh) => {
                                let step = if let Some(agent) = agent {
                                    self.config_changed(app, agent, fresh)
                                } else {
                                    self.apply_display(app, &fresh);
                                    self.adopt_after_turn(app, Step::Rebuild(Box::new(fresh)));
                                    Step::Done
                                };
                                self.note("Favorites", format!("Switched to {}", selection.display_name()));
                                return Ok(step);
                            }
                            Err(error) => self.note("Favorites", format!("not changed: {error:#}")),
                        }
                    }
                    None => self.note("Favorites", if crate::cli::model_scope::is_active() {
                        "No available models in the --models scope."
                    } else {
                        "No available chat favorites. Use Ctrl-F in /models to save one."
                    }),
                }
            }
            _ => self.note("Model", "Unknown option. Use /model, /model settings, /model defaults, /model local or /model memory."),
        }
        Ok(Step::Done)
    }

    pub(in crate::app) fn command(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        line: &str,
    ) -> Result<Step> {
        let Some(rest) = line.strip_prefix('/') else {
            return self.run_turn(app, agent, line);
        };
        let (name, args) = rest
            .split_once(char::is_whitespace)
            .map_or((rest, ""), |(name, args)| (name, args.trim()));
        if let Some(draft) = crate::app::command_registry::hubs::draft(name, args) {
            if self.composer.draft().trim().is_empty() {
                self.composer.set_draft(draft);
            } else {
                self.note(
                    "Command",
                    format!("Your draft is kept. Enter {draft}<value> to continue."),
                );
            }
            return Ok(Step::Done);
        }
        let (name, args) = crate::app::command_registry::hubs::expand(name, args);
        // `/accounts pin` is the clearer spelling of `/session pin`, which pins a ChatGPT account.
        let accounts_pin;
        let (name, args) = if name == "accounts" && (args == "pin" || args.starts_with("pin ")) {
            accounts_pin = format!("pin{}", &args[3..]);
            ("session", accounts_pin.as_str())
        } else {
            (name, args)
        };
        let name = canonical_command(name);
        // Compatibility spellings keep their distinct behavior through unified subactions.
        let (name, args) = if name == "help" && args == "commands" {
            ("commands", "")
        } else if name == "undo" && (args == "files" || args.starts_with("files ")) {
            ("rollback", args.strip_prefix("files").unwrap_or("").trim())
        } else if name == "review" && (args == "files" || args.starts_with("files ")) {
            (
                "review-file",
                args.strip_prefix("files").unwrap_or("").trim(),
            )
        } else if name == "sessions" && args == "fork" {
            ("fork", "")
        } else {
            (name, args)
        };
        if let Some(reason) =
            crate::ui::command_surface::unavailable_reason(name, args, self.command_context())
        {
            self.note(&format!("/{name}"), reason);
            return Ok(Step::Done);
        }
        if name == "worker" {
            if args.is_empty() {
                self.note("Worker", "Usage: /worker <message>");
                return Ok(Step::Done);
            }
            self.note("Worker", "No turn is running; sending this as a normal task. Use /worker during a turn for independent work.");
            return self.run_turn(app, agent, args);
        }
        if name == "activity" && args == "resume" {
            let report = crate::app::restore::reviewed_restore(app, self, agent)?;
            self.note(
                "Resume interrupted workers",
                format!(
                    "{} resumed; {} refused",
                    report.restored.len(),
                    report.rejected.len()
                ),
            );
            for (id, reason) in report.rejected {
                self.note("Resume refused", format!("{id}: {reason}"));
            }
            return Ok(Step::Done);
        }
        if name == "todo" {
            self.todo = agent.todo().clone();
            let progress = agent.progress_view();
            return match self.work_plan(args, Some(&progress)) {
                Some(spec) => self.open(app, agent, Which::Choice(Box::new(spec))),
                None => Ok(Step::Done),
            };
        }
        if (args.is_empty() || (name == "permissions" && args == "remove"))
            && let Some(spec) = self.picker(app, agent, name, args)
        {
            return self.open(app, agent, Which::Choice(Box::new(spec)));
        }
        if let Some(panel) = command_panel(name, args) {
            return self.open(app, agent, panel);
        }
        let changes_session = matches!(name, "new" | "clear" | "fork" | "move" | "undo")
            || (name == "wt"
                && matches!(args.split_whitespace().next(), Some("create" | "switch")));
        if changes_session {
            self.poll_input_recovery(app);
            if let Err(error) = self.prepare_input_session_change() {
                self.note("Input recovery", format!("{error:#}"));
                return Ok(Step::Done);
            }
        }
        if let Some(result) = crate::app::commands::run(self, app, agent, name, args) {
            return result.or_else(|error| {
                self.note(&format!("/{name}"), format!("{error:#}"));
                Ok(Step::Done)
            });
        }
        match name {
            "reload" => {
                if !args.is_empty() {
                    self.note("Reload", "Usage: /reload");
                    return Ok(Step::Done);
                }
                self.refresh_resources(app, true, false);
                if let Some(config) = self.pending_config.take() {
                    return Ok(self.config_changed(app, agent, config));
                }
                if std::mem::take(&mut self.pending_resource_rebuild) {
                    return Ok(Step::Rebuild(Box::new(app.config.clone())));
                }
            }
            "compute" => return self.open(app, agent, Which::Compute),
            "processes" | "bg" => return self.open(app, agent, Which::Jobs),
            "exit" | "quit" | "q" => return Ok(Step::Quit),
            "help" | "?" => match args {
                "" => return self.open(app, agent, Which::Palette),
                "all" => {
                    let text = self.help_text(true);
                    self.note("Help", text);
                }
                _ => {
                    let text =
                        super::help::command_help(args).or_else(|| super::help::topic_help(args)).unwrap_or_else(|| self.help_text(false));
                    self.note("Help", text);
                }
            },
            "queue" => self.queue_command(line),
            "review" => return self.review_command(app, agent, args),
            "review-file" => self.review_checkpoints(agent, args),
            "seance" => return self.seance_command(app, agent, args),
            "model" if args == "restart" => {
                if self.ask("Restart managed model?", "Unload only this session's Dial-managed local model. The same model reloads on the next request; files and conversation stay intact. External servers are never restarted.", &[("Restart", 'r'), ("Cancel", 'n')]) == Some(0) {
                    match agent.restart_managed_model() {
                        Ok(()) => self.note("Model", "Managed model unloaded; it will reload on the next request."),
                        Err(error) => self.note("Model", error.to_string()),
                    }
                }
            }
            "model" if matches!(args, "next" | "cancel") => return self.model_command(app, Some(agent), args),
            "model" if !args.is_empty() => {
                self.note(
                    "Model",
                    "Unknown option. Use /model to choose a model, /model settings to tune it, /model defaults for shared context defaults, /model local for the Get models downloads in /models, or /model memory for the memory role's models.",
                );
            }
            "profile" => return self.profile_command(app, Some(agent), args),
            "hotkeys" => return self.open(app, agent, Which::Shortcuts),
            "accounts" => self.accounts_note(app),
            "improve" => return self.improve_command(app, agent, args),
            "changes" => self.changes_note(&agent.layout().unwrap_or(&app.layout).project_root),
            "theme" => return self.theme_command(app, Some(agent), args),
            "new" | "clear" => match agent.gate_session_change("session_before_switch", serde_json::json!({"reason":"new"})).map_err(anyhow::Error::msg).and_then(|()| agent.layout().context("agent has no project layout")).and_then(|layout| {
                Session::create(
                    layout,
                    SessionMeta {
                        model: agent.model().to_owned(),
                    },
                )
            }) {
                Ok(session) => {
                    let id = session.id().to_owned();
                    self.swap_agent_session(app, agent, session)?;
                    agent.set_plan_mode(false);
                    self.stop_loop_for_session_change();
                    self.show_session(agent.session());
                    self.set_session_id(&id);
                    self.note("Session", format!("new session {}", short(&id)));
                }
                Err(error) => self.note("Session", format!("error: {error:#}")),
            },
            "autoresearch" => return self.autoresearch_command(agent, args),
            "archive" => self.archive_note(app, args),
            "sessions" | "resume" | "tree" => {
                // `/tree forks` is the fork browser; its argument is a keyword, not a search.
                if name == "tree" && args != "forks" {
                    self.note("Tree", "Usage: /tree (this session's branches) or /tree forks (session forks).");
                    return Ok(Step::Done);
                }
                let query = if name == "tree" { "" } else { args };
                match self.pick_session(agent, query, name == "tree", name == "sessions" || name == "resume") {
                    Ok(Some(session)) => {
                        if let Err(reason) = agent.gate_session_change(
                            "session_before_switch",
                            serde_json::json!({"reason":"resume","target_session_id":session.id()}),
                        ) {
                            self.note("Resume", reason);
                            return Ok(Step::Done);
                        }
                        self.swap_agent_session(app, agent, session)?;
                        self.show_session(agent.session());
                        agent.set_plan_mode(false);
                        self.stop_loop_for_session_change();
                        self.note("Resume", Self::resume_note(agent.session()));
                    }
                    Ok(None) => {}
                    Err(error) => self.note("Sessions", format!("{error:#}")),
                }
            }
            "rename" => match agent.session_mut().rename(args) {
                Ok(()) => {
                    self.set_scope(&agent.session().title());
                    self.note("Session", format!("renamed to {}", agent.session().title()))
                }
                Err(error) => self.note("Session", format!("{error:#}")),
            },
            "export" => {
                let html = args.split_whitespace().any(|a| a == "--html");
                let path = args
                    .split_whitespace()
                    .filter(|a| *a != "--html")
                    .collect::<Vec<_>>()
                    .join(" ");
                let path = if path.is_empty() {
                    format!(
                        "session-{}.{}",
                        agent.session().id(),
                        if html { "html" } else { "md" }
                    )
                } else {
                    path
                };
                let exported = match agent.layout() {
                    Some(layout) => agent.session().export(layout, Path::new(&path), html),
                    None => Err(anyhow::anyhow!("agent has no project layout")),
                };
                match exported {
                    Ok(()) => self.note("Export", format!("saved {path}")),
                    Err(error) => self.note("Export", format!("{error:#}")),
                }
            }
            "copy" => self.copy_answer(agent),
            "plan" => {
                let next = match args {
                    "" => !agent.plan_mode(),
                    "on" => true,
                    "off" => false,
                    _ => {
                        self.note("Plan", "usage: /plan [on|off]");
                        return Ok(Step::Done);
                    }
                };
                agent.set_plan_mode(next);
                self.note(
                    "Plan",
                    if next {
                        "read-only planning enabled"
                    } else {
                        "planning disabled"
                    },
                );
            }
            "permissions" => return self.permissions_command(agent, args),
            "cost" => return self.cost_command(agent),
            "workers" => {
                let mut bridge = Bridge {
                    app,
                    shell: self,
                    interrupt: app.interrupt.clone(),
                    started: Instant::now(),
                    last_paint: Instant::now() - PAINT_EVERY,
                    failed: false,
                    pending: Vec::new(),
                    next_id: 0,
                };
                let result = agent
                    .subagent_status(if args.is_empty() { None } else { Some(args) }, &mut bridge);
                match result {
                    Ok(value) => self.note("Jobs", value.to_string()),
                    Err(error) => self.note("Jobs", format!("{error:#}")),
                }
            }
            "message" => {
                let result = match args.split_once(char::is_whitespace) {
                    Some((id, text)) => agent.subagent_message(id, text.trim()),
                    None => Err(anyhow::anyhow!("usage: /message <id> <text>")),
                };
                match result {
                    Ok(value) => self.note("Jobs", value.to_string()),
                    Err(error) => self.note("Jobs", format!("{error:#}")),
                }
            }
            "cancel" => match agent.cancel_subagent(args) {
                Ok(value) => self.note("Jobs", value.to_string()),
                Err(error) => self.note("Jobs", format!("{error:#}")),
            },
            "fork" => match agent.gate_session_change("session_before_branch", serde_json::json!({"reason":"fork"})).map_err(anyhow::Error::msg).and_then(|()| agent.layout().context("agent has no project layout")).and_then(|layout| agent.session().fork(layout)) {
                Ok(session) => {
                    let id = session.id().to_owned();
                    self.swap_agent_session(app, agent, session)?;
                    agent.set_plan_mode(false);
                    self.stop_loop_for_session_change();
                    self.set_session_id(&id);
                    self.set_scope(&agent.session().title());
                    self.note("Session", format!("forked into {}", short(&id)));
                }
                Err(error) => self.note("Session", format!("error: {error:#}")),
            },
            "compact" => return self.compact_command(app, agent, args),
            "undo" => return self.undo_command(app, agent, args),
            "rollback" => {
                let Some(turns) = turn_count(args) else {
                    self.note("Rollback", "Usage: /rollback [n] with a whole number of turns.");
                    return Ok(Step::Done);
                };
                match agent.checkpoints() {
                    Some(checkpoints) => match checkpoints.rollback(turns) {
                        Ok(report) => self.note(
                            "Rollback",
                            format!(
                                "restored {:?}, removed {:?}{}",
                                report.restored, report.removed,
                                if report.warnings.is_empty() { String::new() } else { format!("\n{}", report.warnings.join("\n")) }
                            ),
                        ),
                        Err(error) => self.note("Rollback", format!("{error:#}")),
                    },
                    None => self.note("Rollback", "checkpoints unavailable"),
                }
            }
            "skills" => self.skills_command(app, args),
            "laya" => {
                if matches!(args, "on" | "off") {
                    return self.laya_toggle(app, Some(agent), args == "on");
                }
                if !matches!(args, "" | "status" | "check" | "start") {
                    self.note("Laya", "Usage: /laya status | check | start | on | off");
                } else {
                    app.interrupt.store(false, Ordering::SeqCst);
                    let mut bridge = Bridge {
                        app,
                        shell: self,
                        interrupt: app.interrupt.clone(),
                        started: Instant::now(),
                        last_paint: Instant::now() - PAINT_EVERY,
                        failed: false,
                        pending: Vec::new(),
                        next_id: 0,
                    };
                    bridge.shell.composer.set_busy(true);
                    let result = agent.laya_status(args == "check", args == "start", &mut bridge);
                    bridge.shell.composer.set_busy(false);
                    match result {
                        Ok(text) => bridge.shell.note("Laya", text),
                        Err(error) => bridge.shell.note("Laya", format!("{error:#}")),
                    }
                }
            }
            "code-history" => self.code_history(app, args),
            "understanding" if args == "clear" => self.understanding_clear(app),
            "understanding" if args == "build" => {
                app.interrupt.store(false, Ordering::SeqCst);
                if !self.wait_compute(app)? || !self.prepare_local(app)? {
                    return Ok(Step::Done);
                }
                let mut bridge = Bridge {
                    app,
                    shell: self,
                    interrupt: app.interrupt.clone(),
                    started: Instant::now(),
                    last_paint: Instant::now() - PAINT_EVERY,
                    failed: false,
                    pending: Vec::new(),
                    next_id: 0,
                };
                bridge.shell.composer.set_busy(true);
                let result = agent.synthesize_architecture(true, &mut bridge);
                bridge.shell.composer.set_busy(false);
                match result {
                    Ok(()) => {
                        let cancel = app.interrupt.clone();
                        let text = crate::tools::knowledge::codebase::Codebase::open(&app.layout)
                            .and_then(|cache| cache.understanding("architecture", &cancel))
                            .map(|report| crate::tools::knowledge::codebase::render_understanding(&report, 40))
                            .unwrap_or_else(|error| format!("{error:#}"));
                        bridge.shell.note("Understanding", text);
                    }
                    Err(error) => bridge.shell.note("Architecture", format!("{error:#}")),
                }
            }
            "reliability" => {
                self.request_policy = agent.session().control_state();
                self.note(
                    "Tool reliability",
                    crate::reliability::render(&crate::reliability::live_report(
                        &app.config,
                        &self.request_policy,
                    )),
                );
            }
            "policy" => {
                self.request_policy = agent.session().control_state();
                self.note(
                    "Adaptive policy",
                    crate::adaptive::render(&crate::adaptive::report(
                        &app.config,
                        &self.request_policy,
                    )),
                );
            }
            "steering" => {
                let mut value = crate::steering::status(&app.config);
                value["runtime"] = agent.steering_runtime();
                self.note("Activation steering", crate::steering::render(&value));
            }
            "experience" => return self.experience_command(app, agent, args),
            "understanding" => return self.understanding_command(app, args),
            "memory" => return self.memory_command(app, agent, args),
            "commands" => {
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
            "search" => return self.search_command(app, agent, args),
            "attachments" => self.attachments_command(args),
            "draft" => self.draft_command(app, args),
            "context" => return self.context_command(agent),
            "approval" => {
                match args {
                    "ask" => agent.set_approval(Approval::Ask),
                    "write" => agent.set_approval(Approval::Write),
                    "auto" => agent.set_approval(Approval::Auto),
                    "readonly" => agent.set_approval(Approval::Readonly),
                    "" => {}
                    _ => {
                        self.note("Approval", "usage: /approval ask|write|auto|readonly");
                        return Ok(Step::Done);
                    }
                }
                let mode = format!("{:?}", agent.approval()).to_lowercase();
                let label = crate::ui::status::approval_label(&mode);
                let meaning = agent.approval().description();
                self.note(
                    "Approval",
                    format!("{label} ({mode}) · {meaning}\nChange it with /approval ask|write|auto|readonly or Shift+Tab."),
                );
            }
            "outcome" => self.outcome_command(app, args),
            other if other.starts_with("skill:") => {
                let skill = &other["skill:".len()..];
                let known = skills::discover(&app.layout)
                    .into_iter()
                    .any(|s| s.name == skill);
                if !known {
                    self.note(
                        "Skill",
                        format!("unknown skill {skill:?}; /skills lists them"),
                    );
                    return Ok(Step::Done);
                }
                let task = if args.is_empty() {
                    String::new()
                } else {
                    format!(" Task: {args}")
                };
                return self.run_turn(
                    app,
                    agent,
                    &format!(
                        "Use the \"{skill}\" skill (load it with the skill tool first).{task}"
                    ),
                );
            }
            _ => match self.commands.iter().find(|def| def.name == name) {
                Some(def) => {
                    let prompt = context::expand_command(def, args);
                    return self.run_turn(app, agent, &prompt);
                }
                // Not a command (a pasted path such as `/Users/me/notes.md`, or other text that
                // happens to start with a slash): send the line as an ordinary message.
                None => return self.run_turn(app, agent, line),
            },
        }
        Ok(Step::Done)
    }

    fn review_command(&mut self, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
        if args.is_empty()
            && agent
                .checkpoints()
                .is_some_and(|checkpoints| checkpoints.list().iter().any(|turn| turn.files > 0))
        {
            match self.ask(
                "Review changes",
                "Get a read-only AI review, or inspect checkpointed changes to keep or revert files.",
                &[("AI review", 'a'), ("Keep / revert files", 'f')],
            ) {
                Some(0) => {}
                Some(1) => {
                    self.review_checkpoints(agent, "");
                    return Ok(Step::Done);
                }
                _ => return Ok(Step::Done),
            }
        }
        platform::interrupt::clear_interrupt();
        self.live_tasks = Some(agent.live_task_control());
        self.background_control = agent.background_control();
        self.composer.set_busy(true);
        self.status.phase = Phase::Working;
        match self.wait_compute(app).and_then(|ready| {
            if ready {
                self.prepare_local(app)
            } else {
                Ok(false)
            }
        }) {
            Ok(true) => {}
            Ok(false) => {
                self.composer.set_busy(false);
                return Ok(Step::Done);
            }
            Err(error) => {
                self.composer.set_busy(false);
                self.note("Review", format!("{error:#}"));
                return Ok(Step::Done);
            }
        }
        let guidance = self
            .commands
            .iter()
            .find(|def| def.name == "review")
            .map(|def| context::expand_command(def, args))
            .unwrap_or_default();
        let started = Instant::now();
        let result = {
            let mut bridge = Bridge {
                app,
                shell: self,
                interrupt: app.interrupt.clone(),
                started,
                last_paint: Instant::now() - PAINT_EVERY,
                failed: false,
                pending: Vec::new(),
                next_id: 0,
            };
            bridge.repaint(true);
            agent.review_target(args, &guidance, &mut bridge)
        };
        app.interrupt.store(false, Ordering::SeqCst);
        self.composer.set_busy(false);
        self.status.activity.clear();
        self.refresh_status(app, agent);
        if crate::cli::watch::enabled(&app.layout, &self.session_id) {
            let body = match &result {
                Ok(report) => report.clone(),
                Err(error) => format!("{error:#}"),
            };
            if let Err(error) = crate::cli::watch::append(
                &app.layout,
                &self.session_id,
                crate::cli::watch::Event::Text {
                    title: "Review".into(),
                    body,
                },
            ) {
                self.note("Live feed", format!("Could not record review: {error:#}"));
            }
        }
        match result {
            Ok(report) => self.note("Review", report),
            Err(error) => self.note("Review", format!("{error:#}")),
        }
        Ok(Step::Done)
    }

    fn seance_command(&mut self, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
        let (source, question) = args
            .split_once(char::is_whitespace)
            .map_or(("", ""), |(source, question)| (source, question.trim()));
        if source.is_empty() || question.is_empty() {
            self.note(
                "Seance",
                "Usage: /seance <session id prefix or worker id> <question>",
            );
            return Ok(Step::Done);
        }
        platform::interrupt::clear_interrupt();
        self.live_tasks = Some(agent.live_task_control());
        self.background_control = agent.background_control();
        self.composer.set_busy(true);
        self.status.phase = Phase::Working;
        match self.wait_compute(app).and_then(|ready| {
            if ready {
                self.prepare_local(app)
            } else {
                Ok(false)
            }
        }) {
            Ok(true) => {}
            Ok(false) => {
                self.composer.set_busy(false);
                return Ok(Step::Done);
            }
            Err(error) => {
                self.composer.set_busy(false);
                self.note("Seance", format!("{error:#}"));
                return Ok(Step::Done);
            }
        }
        let started = Instant::now();
        let result = {
            let mut bridge = Bridge {
                app,
                shell: self,
                interrupt: app.interrupt.clone(),
                started,
                last_paint: Instant::now() - PAINT_EVERY,
                failed: false,
                pending: Vec::new(),
                next_id: 0,
            };
            bridge.repaint(true);
            agent.run_seance(source, question, &mut bridge)
        };
        app.interrupt.store(false, Ordering::SeqCst);
        self.composer.set_busy(false);
        self.status.activity.clear();
        self.refresh_status(app, agent);
        match result {
            Ok(answer) => self.note("Seance", answer),
            Err(error) => self.note("Seance", format!("{error:#}")),
        }
        Ok(Step::Done)
    }

    fn autoresearch_command(&mut self, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
        let layout = agent
            .layout()
            .ok_or_else(|| anyhow::anyhow!("agent has no project layout"))?
            .clone();
        let state = layout.state_dir.join("autoresearch");
        let mut words = args
            .splitn(5, char::is_whitespace)
            .filter(|word| !word.is_empty());
        let action = words.next().unwrap_or("status");
        let result = match action {
            "init" => match (words.next(), words.next(), words.next(), words.next()) {
                (Some(name), Some(metric), Some(direction), Some(benchmark)) => match crate::tools::knowledge::autoresearch::experiment::parse_direction(direction) {
                    Ok(maximize) => crate::tools::knowledge::autoresearch::experiment::init(&layout.project_root, &state, name, benchmark, metric, maximize).map(|e| format!("Initialized {} on branch {}.{}", e.name, e.branch, if e.origin.is_empty() { String::new() } else { format!("\nLeave with /autoresearch exit; it returns to {}.", e.origin) })),
                    Err(_) => anyhow::bail!("direction must be max or min. Usage: /autoresearch init <name> <metric> <max|min> <benchmark command>"),
                },
                _ => anyhow::bail!("Usage: /autoresearch init <name> <metric> <max|min> <benchmark command>"),
            },
            "run" => crate::tools::knowledge::autoresearch::experiment::run(&layout.project_root, &state).map(|run| format!("{} = {} · {}{}", run.metric, run.commit, if run.kept {"best"} else {"not best"}, if run.output.is_empty(){String::new()}else{format!("\n{}",run.output)})),
            "log" => crate::tools::knowledge::autoresearch::experiment::log(&state).map(|runs| runs.into_iter().map(|r| format!("#{} {} {} {}", r.id, r.metric, if r.kept {"best"} else {""}, r.commit)).collect::<Vec<_>>().join("\n")),
            "notes" => { let text = args.trim().split_once(char::is_whitespace).map_or("", |(_, rest)| rest.trim()); if text.is_empty() { crate::tools::knowledge::autoresearch::experiment::notes(&state) } else { crate::tools::knowledge::autoresearch::experiment::update_notes(&state, text).map(|()| "Experiment notes updated.".to_owned()) } }
            "resume" | "status" | "" => crate::tools::knowledge::autoresearch::experiment::current(&state).map(|experiment| experiment.map(|e| format!("{} · {} · {} ({})\n{}", e.name, e.metric, e.branch, e.benchmark, crate::tools::knowledge::autoresearch::experiment::notes(&state).unwrap_or_default())).unwrap_or_else(|| "No autoresearch experiment. Use /autoresearch init <name> <metric> <max|min> <benchmark command>.".to_owned())),
            "exit" => crate::tools::knowledge::autoresearch::experiment::exit(&layout.project_root, &state),
            other => anyhow::bail!("Unknown autoresearch action: {other}. Use init, run, log, notes, resume, exit."),
        };
        match result {
            Ok(text) => self.note("Autoresearch", text),
            Err(error) => self.note("Autoresearch", format!("{error:#}")),
        }
        Ok(Step::Done)
    }

    fn permissions_command(&mut self, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
        if args == "why" {
            let text = if self.policy_log.is_empty() {
                "No tool call has passed the approval policy in this session yet.".to_owned()
            } else {
                let rows: Vec<&str> = self.policy_log.iter().rev().map(String::as_str).collect();
                format!(
                    "Most recent first. Each row names the rule that decided the call and what would change it.\n{}",
                    rows.join("\n")
                )
            };
            self.note("Permissions", text);
            return Ok(Step::Done);
        }
        self.note("Permissions", PERMISSION_PATH_SCOPE);
        if let Some(rest) = args.strip_prefix("remove ") {
            let mut words = rest.split_whitespace();
            let index = words
                .next()
                .and_then(|s| s.parse::<usize>().ok())
                .and_then(|n| n.checked_sub(1));
            let persist = words.any(|s| s == "--persist");
            match index {
                Some(index) => match agent.remove_permission_rule(index, persist) {
                    Ok(()) => self.note("Permissions", "rule removed"),
                    Err(error) => self.note("Permissions", format!("{error:#}")),
                },
                None => self.note(
                    "Permissions",
                    "usage: /permissions remove <number> [--persist]",
                ),
            }
        } else if args.starts_with("allow ") || args.starts_with("deny ") {
            use crate::agent::safety::approval::{Effect, PermissionRule};
            let mut words = args.split_whitespace();
            let effect = if words.next() == Some("allow") {
                Effect::Allow
            } else {
                Effect::Deny
            };
            let first = words.next().unwrap_or("");
            let persist = first == "--persist";
            let tool = if persist {
                words.next().unwrap_or("")
            } else {
                first
            };
            // `--path <glob>` (repeatable) scopes file tools; remaining words are a bash command prefix.
            let (mut paths, mut command_prefix) = (Vec::new(), Vec::new());
            while let Some(word) = words.next() {
                if word == "--path" {
                    paths.extend(words.next().map(|p| p.trim_matches(['\'', '"']).to_owned()));
                } else {
                    command_prefix.push(word.to_owned());
                }
            }
            let prefix = if tool == "bash" {
                crate::agent::safety::approval::bash_prefix(&command_prefix)
            } else {
                Ok(command_prefix)
            };
            match prefix {
                Ok(command_prefix) => {
                    let rule = PermissionRule {
                        effect,
                        tool: tool.into(),
                        command_prefix,
                        paths,
                    };
                    match agent.add_permission_rule(rule, persist) {
                        Ok(()) => self.note(
                            "Permissions",
                            if persist {
                                "rule saved"
                            } else {
                                "session rule added"
                            },
                        ),
                        Err(error) => self.note("Permissions", format!("{error:#}")),
                    }
                }
                Err(error) => self.note("Permissions", error),
            }
        } else if args.is_empty() {
            let rules = agent
                .permission_rules()
                .iter()
                .enumerate()
                .map(|(i, rule)| {
                    format!(
                        "{}: {:?} {} {}{}",
                        i + 1,
                        rule.effect,
                        rule.tool,
                        rule.command_prefix.join(" "),
                        rule.paths
                            .iter()
                            .map(|p| format!(" --path {p}"))
                            .collect::<String>()
                    )
                })
                .collect::<Vec<_>>();
            self.note(
                "Permissions",
                if rules.is_empty() {
                    "No allow or deny rules yet. /permissions allow <tool> [prefix] adds one; \"Always allow\" in an approval prompt adds one too.".into()
                } else {
                    rules.join("\n")
                },
            );
        } else {
            self.note("Permissions", "usage: /permissions; /permissions why; /permissions allow|deny [--persist] <tool> [prefix words | --path <glob>…]; /permissions remove <number> [--persist]");
        }
        Ok(Step::Done)
    }

    fn cost_command(&mut self, agent: &Agent<'_>) -> Result<Step> {
        let (total, turn) = agent.usage();
        if total.all.requests == 0 {
            self.note(
                "Cost and usage",
                "No model requests yet in this session.\nCost and token counts appear here after the first reply.",
            );
            return Ok(Step::Done);
        }
        // Summary first, then one short block per role that made requests. Accounting detail
        // (pricing mix, discarded or unreported attempts) appears only when it says something.
        let describe = |role: &str, c: &usage::Counts| -> Option<String> {
            if c.requests == 0 && c.transport_attempts == 0 {
                return None;
            }
            let mut rows = vec![format!(
                "{role}: {} · {} request{}{}{}",
                c.cost_label(),
                c.requests,
                if c.requests == 1 { "" } else { "s" },
                if c.retries > 0 {
                    format!(" · {} retries", c.retries)
                } else {
                    String::new()
                },
                if c.failed_requests > 0 {
                    format!(" · {} failed", c.failed_requests)
                } else {
                    String::new()
                },
            )];
            let mut tokens = vec![format!("in {}", c.input), format!("out {}", c.output)];
            if c.cache_read + c.cache_write > 0 {
                tokens.push(format!(
                    "cache read {} / write {}",
                    c.cache_read, c.cache_write
                ));
            }
            if c.reasoning > 0 {
                tokens.push(format!("reasoning {}", c.reasoning));
            }
            rows.push(format!("  tokens   {}", tokens.join(" · ")));
            rows.push(format!(
                "  cache    {}",
                cache_line(c).trim_start_matches("cache hit rate: ")
            ));
            let billing: Vec<String> = [
                (c.priced_requests, "priced"),
                (c.unpriced_requests, "unpriced"),
                (c.local_requests, "local"),
                (c.subscription_requests, "subscription"),
            ]
            .into_iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, label)| format!("{n} {label}"))
            .collect();
            if !billing.is_empty() {
                rows.push(format!("  billing  {}", billing.join(" · ")));
            }
            if c.discarded_attempts > 0
                || c.unreported_attempts > 0
                || c.transport_attempts != c.successful_requests
            {
                rows.push(format!(
                    "  attempts {} sent · {} accepted · {} discarded · {} without complete usage",
                    c.transport_attempts,
                    c.successful_requests,
                    c.discarded_attempts,
                    c.unreported_attempts
                ));
            }
            Some(rows.join("\n"))
        };
        let turn_label = turn
            .coverage
            .turn_id
            .map_or_else(|| "this turn".to_owned(), |id| format!("turn {id}"));
        let mut text = format!(
            "Session: {} · requests: {} · tokens in: {} · out: {}\nCurrent {turn_label}: {}\nCoverage: {}",
            total.all.cost_label(),
            total.all.requests,
            total.all.input,
            total.all.output,
            turn.all.cost_label(),
            total.coverage.history,
        );
        for block in [
            describe("Main agent", &total.main),
            describe("Workers", &total.workers),
        ]
        .into_iter()
        .flatten()
        {
            text.push_str(&format!("\n\n{block}"));
        }
        if let Some(line) = prefix_reset_line(agent.prefix_resets()) {
            text.push_str(&format!("\n\n{line}"));
        }
        // Per-model blocks only add information when more than one model served requests.
        if total.by_model.len() > 1 {
            for (model, counts) in &total.by_model {
                for block in [
                    describe(&format!("{model} · main"), &counts.main),
                    describe(&format!("{model} · workers"), &counts.workers),
                ]
                .into_iter()
                .flatten()
                {
                    text.push_str(&format!("\n\n{block}"));
                }
            }
        }
        text.push_str("\n\nMissing usage or prices are unknown, not free; subscription and local use is not invoice-priced. Caps act on observed cost; in-flight workers and discarded attempts can add charges. Earlier (resumed) sessions are not counted.");
        self.note("Cost and usage", text);
        Ok(Step::Done)
    }

    fn compact_command(&mut self, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
        self.compact_command_review(app, agent, args, |shell, preview| {
            shell.ask(
                "Compact context?",
                preview,
                &[("Cancel", 'c'), ("Compact", 's')],
            ) == Some(1)
        })
    }

    fn compact_command_review(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        args: &str,
        review: impl FnOnce(&mut Self, &str) -> bool,
    ) -> Result<Step> {
        let mut options = match crate::config::compaction::CompactionOptions::from_args(args) {
            Ok(options) => options,
            Err(error) => {
                self.note("Context", format!("{error:#}"));
                return Ok(Step::Done);
            }
        };
        options.suppress_continuation = true;
        let preview = match super::context_decision::render_compaction_preflight(agent, &options) {
            Ok(preview) => preview,
            Err(error) => {
                self.note(
                    "Context",
                    format!("Compaction preview unavailable: {error:#}. Nothing changed."),
                );
                return Ok(Step::Done);
            }
        };
        if !review(self, &preview) {
            return Ok(Step::Done);
        }
        if !self.wait_compute(app)? {
            return Ok(Step::Done);
        }
        self.composer.set_busy(true);
        self.status.phase = Phase::Compacting;
        self.status.activity = "compacting".into();
        let result = {
            let mut bridge = Bridge {
                app,
                shell: self,
                interrupt: app.interrupt.clone(),
                started: Instant::now(),
                last_paint: Instant::now() - PAINT_EVERY,
                failed: false,
                pending: Vec::new(),
                next_id: 0,
            };
            bridge.repaint(true);
            agent.compact_with_options(&mut bridge, options)
        };
        self.composer.set_busy(false);
        if let Err(error) = result {
            self.transcript.error(&format!("{error:#}"));
        }
        Ok(Step::Done)
    }

    fn undo_command(&mut self, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
        let Some(turns) = turn_count(args) else {
            self.note(
                "Undo",
                "Usage: /undo [n] with a whole number of turns (default 1).",
            );
            return Ok(Step::Done);
        };
        // Resolve and validate the exact conversation range before any filesystem mutation.
        let checkpoint_turns = match agent.session().rewind_checkpoint_turns(turns) {
            Ok(turns) => turns,
            Err(error) => {
                self.note("Undo", format!("{error:#}"));
                return Ok(Step::Done);
            }
        };
        let approval = match crate::app::commands::branch::preflight_rewind(agent, turns) {
            Ok(approval) => approval,
            Err(error) => {
                self.note("Undo", format!("{error:#}; files and conversation kept"));
                return Ok(Step::Done);
            }
        };
        let checkpoint_session = agent.session().id().to_owned();
        let mut lines = Vec::new();
        if let (Some(trajectory), Some(store)) = (self.last_trajectory, app.store.as_ref()) {
            let outcome = Outcome {
                trajectory,
                score: 0.0,
                source: "user:undo".into(),
                reference: None,
            };
            match store.outcome(&outcome) {
                Ok(()) => self.start_live_monitor("Undo", app.config.clone()),
                Err(error) => lines.push(format!("could not record undo feedback: {error:#}")),
            }
        }
        if !checkpoint_turns.is_empty() {
            let result = agent
                .checkpoints()
                .context("checkpoints unavailable; conversation kept")
                .and_then(|checkpoints| {
                    checkpoints.rollback_conversation(agent.session().id(), &checkpoint_turns)
                });
            match result {
                Ok(report) => {
                    lines.push(format!(
                        "restored {} file(s), removed {}",
                        report.restored.len(),
                        report.removed.len()
                    ));
                    if !report.warnings.is_empty() {
                        lines.extend(report.warnings);
                        lines.push("Restoration incomplete; conversation kept. Inspect files before retrying.".into());
                        self.note("Undo", lines.join("\n"));
                        return Ok(Step::Done);
                    }
                }
                Err(error) => {
                    lines.push(format!("rollback: {error:#}; conversation kept"));
                    self.note("Undo", lines.join("\n"));
                    return Ok(Step::Done);
                }
            }
        }
        agent.cancel_background_compaction();
        match crate::app::commands::branch::rewind_approved(app, agent, approval) {
            Ok(rewound) => {
                lines.push(format!("dropped {} message(s)", rewound.dropped));
                agent.record_feedback(crate::telemetry::FeedbackKind::Undo, turns as u64);
                lines.extend(rewound.note);
                if let Some(checkpoints) = agent.checkpoints()
                    && let Err(error) = checkpoints
                        .finalize_conversation_rollback(&checkpoint_session, &checkpoint_turns)
                {
                    lines.push(format!(
                        "conversation rewound, checkpoint cleanup: {error:#}"
                    ));
                }
            }
            Err(error) => lines.push(format!("rewind: {error:#}")),
        }
        self.note("Undo", lines.join("\n"));
        Ok(Step::Done)
    }

    fn experience_command(&mut self, app: &App, agent: &Agent<'_>, args: &str) -> Result<Step> {
        if args == "ledger" {
            self.note(
                "Investigation",
                serde_json::to_string_pretty(
                    &agent.session().control_state()["investigation_ledger"],
                )
                .unwrap_or_default(),
            );
        } else {
            let result = std::thread::scope(|scope| {
                let (tx, rx) = std::sync::mpsc::channel();
                let layout = &app.layout;
                scope.spawn(move || {
                    let _ = tx.send(
                        crate::storage::experience::Experience::open(layout)
                            .and_then(|store| store.inspect())
                            .map(|report| crate::storage::experience::render(&report)),
                    );
                });
                let mut bridge = Bridge {
                    app,
                    shell: self,
                    interrupt: app.interrupt.clone(),
                    started: Instant::now(),
                    last_paint: Instant::now() - PAINT_EVERY,
                    failed: false,
                    pending: Vec::new(),
                    next_id: 0,
                };
                loop {
                    match rx.recv_timeout(Duration::from_millis(80)) {
                        Ok(result) => break result,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => bridge.heartbeat(),
                        Err(_) => {
                            break Err(anyhow::anyhow!("experience worker disconnected"));
                        }
                    }
                }
            });
            let mut text = result.unwrap_or_else(|error| format!("{error:#}"));
            let on = |flag: bool| if flag { "on" } else { "off" };
            text.push_str(&format!("\nAssistance {} · syntax guard {} · related tests {} · compact tools {}\nTeacher {} · needs a hosted experience role · {}s budget\nEdit controls in /settings; model role in {} → Roles → experience.",on(app.config.assistance.enabled),on(app.config.assistance.syntax_guard),on(app.config.assistance.selective_verification),on(app.config.assistance.compact_tools),on(app.config.assistance.teacher_enabled),app.config.assistance.teacher_timeout_secs,self.keys.label(Action::ModelSelect)));
            self.note("Experience", text);
        }
        Ok(Step::Done)
    }

    fn understanding_command(&mut self, app: &App, args: &str) -> Result<Step> {
        app.interrupt.store(false, Ordering::SeqCst);
        self.composer.set_busy(true);
        let result = std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel();
            let layout = &app.layout;
            let cancel = app.interrupt.clone();
            scope.spawn(move || {
                let _ = tx.send(
                    crate::tools::knowledge::codebase::Codebase::open(layout)
                        .and_then(|cache| cache.understanding(args, &cancel))
                        .map(|report| {
                            crate::tools::knowledge::codebase::render_understanding(&report, 40)
                        }),
                );
            });
            let mut bridge = Bridge {
                app,
                shell: self,
                interrupt: app.interrupt.clone(),
                started: Instant::now(),
                last_paint: Instant::now() - PAINT_EVERY,
                failed: false,
                pending: Vec::new(),
                next_id: 0,
            };
            bridge.set_activity("inspecting saved understanding");
            loop {
                match rx.recv_timeout(Duration::from_millis(80)) {
                    Ok(result) => break result,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => bridge.heartbeat(),
                    Err(_) => {
                        break Err(anyhow::anyhow!("understanding worker disconnected"));
                    }
                }
            }
        });
        self.composer.set_busy(false);
        self.note(
            "Understanding",
            result.unwrap_or_else(|error| format!("{error:#}")),
        );
        Ok(Step::Done)
    }

    fn memory_command(&mut self, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
        app.interrupt.store(false, Ordering::SeqCst);
        let needs_compute = app.config.memory_uses_local_compute()
            && !matches!(
                args.split_whitespace().next(),
                None | Some("status" | "queue")
            );
        if needs_compute && !self.wait_compute_required(app, true, false)? {
            return Ok(Step::Done);
        }
        self.composer.set_busy(true);
        let result = {
            let mut bridge = Bridge {
                app,
                shell: self,
                interrupt: app.interrupt.clone(),
                started: Instant::now(),
                last_paint: Instant::now() - PAINT_EVERY,
                failed: false,
                pending: Vec::new(),
                next_id: 0,
            };
            agent
                .selected_memory_command(args, &mut bridge)
                .and_then(|value| {
                    // `/memory help` answers with text (a `help` field or a bare string).
                    if let Some(help) = value
                        .get("help")
                        .and_then(Value::as_str)
                        .or_else(|| value.as_str())
                    {
                        Ok(help.to_owned())
                    } else if matches!(args.trim(), "" | "status") && value.get("backend").is_some()
                    {
                        Ok(memory_status_text(&value))
                    } else {
                        serde_json::to_string_pretty(&value).map_err(Into::into)
                    }
                })
        };
        self.composer.set_busy(false);
        self.note(
            "Memory",
            result.unwrap_or_else(|error| format!("error: {error:#}")),
        );
        Ok(Step::Done)
    }

    fn context_command(&mut self, agent: &Agent<'_>) -> Result<Step> {
        let mut pending: Vec<Value> = self
            .composer
            .queued_messages()
            .into_iter()
            .map(|(text, _)| serde_json::json!({"role":"user","content":text}))
            .collect();
        let draft = self.composer.expanded_draft();
        if !draft.trim().is_empty() {
            pending.push(serde_json::json!({"role":"user","content":draft}));
        }
        let text = super::context_decision::render_context(agent, &pending)
            .unwrap_or_else(|error| format!("Estimated next request unavailable: {error:#}. Prior usage is not a next-request estimate."));
        self.note("Context", text);
        Ok(Step::Done)
    }
}

/// Share of prompt tokens served from the provider's prompt cache. Prompt totals include cached
/// reads; responses that reported no cache fields count as uncached, so this is a lower bound.
fn cache_line(counts: &usage::Counts) -> String {
    if counts.cache_reported_responses == 0 {
        return "cache hit rate: not reported by the provider".to_owned();
    }
    if counts.input == 0 {
        return "cache hit rate: unknown (no prompt tokens reported)".to_owned();
    }
    let rate = counts.cache_read.min(counts.input) as f64 * 100.0 / counts.input as f64;
    let partial = if counts.cache_reported_responses < counts.reported_responses {
        format!(
            "at least {rate:.0}% of prompt tokens (cache reported on {} of {} responses)",
            counts.cache_reported_responses, counts.reported_responses
        )
    } else {
        format!("{rate:.0}% of prompt tokens")
    };
    format!("cache hit rate: {partial}")
}

/// Main-agent requests whose prompt did not extend the previous one, by cause. Each one made a
/// provider prompt cache start over from the changed part; `None` when every request extended
/// the one before it.
fn prefix_reset_line(resets: &std::collections::BTreeMap<&str, u64>) -> Option<String> {
    let label = |cause: &str| match cause {
        "model_changed" => "model switched",
        "system_changed" => "instructions changed",
        "tools_changed" => "tool list changed",
        "history_changed" => "earlier messages rewritten (compaction or pruning)",
        _ => "other",
    };
    let parts = resets
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(cause, count)| format!("{} {count}", label(cause)))
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| {
        format!(
            "Prompt cache restarts (main agent): {}. Each makes the provider re-read the prompt from the changed part; keeping the model, instructions and tools fixed during a session keeps the cache warm.",
            parts.join(" · ")
        )
    })
}

#[cfg(test)]
mod tests;
