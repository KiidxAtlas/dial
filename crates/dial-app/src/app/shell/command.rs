//! The `/command` dispatcher: routes each slash command to its handler.

use crate::agent::window::context;
use crate::agent::{Agent, Ui};
use crate::app::bridge::Bridge;
use crate::app::command_registry::canonical_command;
use crate::app::core::App;
use crate::app::display::{group_digits, short};
use crate::app::drive::Step;
use crate::app::notices::memory_status_text;
use crate::app::overlay::{Which, start_streaming_job, work_panel};
use crate::app::selection::{PendingFavorite, cycle_set, next_favorite};
use crate::app::shell::teach::TEACH_OFF;
use crate::app::shell::{PAINT_EVERY, Shell};
use crate::config::selection::changes as selection_changes;
use crate::config::settings_store::{Change, Scope};
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

impl Shell {
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
        // `/accounts pin` is the clearer spelling of `/session pin`, which pins a ChatGPT account.
        let accounts_pin;
        let (name, args) = if name == "accounts" && (args == "pin" || args.starts_with("pin ")) {
            accounts_pin = format!("pin{}", &args[3..]);
            ("session", accounts_pin.as_str())
        } else {
            (name, args)
        };
        let name = canonical_command(name);
        if name == "todo" {
            self.todo = agent.todo().clone();
            return match self.work_plan(args) {
                Some(spec) => self.open(app, agent, Which::Choice(Box::new(spec))),
                None => Ok(Step::Done),
            };
        }
        if (args.is_empty() || (name == "permissions" && args == "remove"))
            && let Some(spec) = self.picker(app, agent, name, args)
        {
            return self.open(app, agent, Which::Choice(Box::new(spec)));
        }
        if let Some(panel) = work_panel(name, args) {
            return self.open(app, agent, panel);
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
                    let text = self.help_text(false);
                    self.note("Help", text);
                }
            },
            "queue" => self.queue_command(line),
            "review" => {
                platform::interrupt::clear_interrupt();
                self.live_tasks = Some(agent.live_task_control());
                self.background_control = agent.background_control();
                self.composer.set_busy(true);
                self.status.phase = Phase::Working;
                match self.wait_compute(app).and_then(|ready| if ready { self.prepare_local(app) } else { Ok(false) }) {
                    Ok(true) => {}
                    Ok(false) => { self.composer.set_busy(false); return Ok(Step::Done); }
                    Err(error) => { self.composer.set_busy(false); self.note("Review", format!("{error:#}")); return Ok(Step::Done); }
                }
                let guidance = self.commands.iter().find(|def| def.name == "review")
                    .map(|def| context::expand_command(def, args)).unwrap_or_default();
                let started = Instant::now();
                let result = {
                    let mut bridge = Bridge { app, shell: self, interrupt: app.interrupt.clone(), started,
                        last_paint: Instant::now() - PAINT_EVERY, failed: false, pending: Vec::new(), next_id: 0 };
                    bridge.repaint(true);
                    agent.review_target(args, &guidance, &mut bridge)
                };
                app.interrupt.store(false, Ordering::SeqCst);
                self.composer.set_busy(false);
                self.status.activity.clear();
                self.refresh_status(app, agent);
                match result { Ok(report) => self.note("Review", report), Err(error) => self.note("Review", format!("{error:#}")) }
            }
            "review-file" => {
                let result = (|| -> Result<()> {
                    let checkpoints = agent.checkpoints().cloned().context("No checkpoints are available for this session")?;
                    let (turn_text, requested_path) = args.split_once(' ').unwrap_or((args, ""));
                    let turn = if args.is_empty() {
                        let turns = checkpoints.list();
                        if turns.is_empty() { self.note("Review", "No checkpoint changes to review. No files changed."); return Ok(()); }
                        let labels: Vec<_> = turns.iter().map(|entry| format!("{}: {} ({} files)", entry.turn, entry.label, entry.files)).collect();
                        let choices: Vec<_> = labels.iter().map(|label| (label.as_str(), ' ')).collect();
                        let Some(selected) = self.ask("Review checkpoints", "Choose a turn. Esc returns to your draft without changing files.", &choices) else { return Ok(()); };
                        turns[selected].turn
                    } else { turn_text.parse::<u64>().context("Expected a checkpoint turn number")? };
                    let browse = requested_path.trim().is_empty();
                    loop {
                        let path = if browse {
                            let paths = checkpoints.changed_files(turn);
                            if paths.is_empty() { self.note("Review", "No readable changed-file inventory. No files changed."); break; }
                            let choices: Vec<_> = paths.iter().map(|path| (path.as_str(), ' ')).collect();
                            let Some(selected) = self.ask("Changed files", "Choose a file. Reviewed, tested and accepted are separate states; file preview shows recorded checks, not new verification.", &choices) else { break; };
                            paths[selected].clone()
                        } else { requested_path.trim().to_owned() };
                        let (diff, token) = checkpoints.review_file(turn, &path)?;
                        let state = agent.session().control_state();
                        let recorded = state["reliability"]["verification_results"].as_array().map(|checks| checks.iter().map(|check| format!("{}: {}", check["name"].as_str().unwrap_or("check"), check["status"].as_str().unwrap_or("unverified"))).collect::<Vec<_>>().join("\n")).unwrap_or_else(|| "No checks recorded.".to_string());
                        let reviewed = state["reviewed_file"]["identity"].as_str() == Some(token.as_str());
                        let body = format!("{} · {}\n{}\nRecorded checks (not file-specific proof):\n{}\nRevert includes user edits shown here; comparison is not atomic against external writers. Page keys scroll the diff/details.", path, if reviewed { "reviewed version" } else { "not reviewed" }, diff, recorded);
                        match self.ask("Review file", &body, &[("Keep", 'k'), ("Revert file", 'r'), ("Cancel", 'n')]) {
                            Some(0) => {
                                anyhow::ensure!(checkpoints.review_file(turn, &path)?.1 == token, "File changed after preview. Review the new version before keeping it.");
                                let mut state = agent.session().control_state();
                                state["reviewed_file"] = serde_json::json!({"turn":turn,"path":path,"identity":token});
                                agent.session_mut().set_control_state(state)?;
                                self.note("Review", "Kept the reviewed version; review is not test verification.");
                            }
                            Some(1) => {
                                checkpoints.restore_file(turn, &path, &token)?;
                                self.note("Review", "Restored only the selected file. The revert has its own checkpoint for /undo.");
                            }
                            _ => {}
                        }
                        if !browse { break; }
                    }
                    Ok(())
                })();
                if let Err(error) = result { self.note("Review", error.to_string()); }
            }
            "model" if args == "restart" => {
                if self.ask("Restart managed model?", "Unload only this session's Dial-managed local model. The same model reloads on the next request; files and conversation stay intact. External servers are never restarted.", &[("Restart", 'r'), ("Cancel", 'n')]) == Some(0) {
                    match agent.restart_managed_model() {
                        Ok(()) => self.note("Model", "Managed model unloaded; it will reload on the next request."),
                        Err(error) => self.note("Model", error.to_string()),
                    }
                }
            }
            "model" if args == "cancel" => {
                self.pending_favorite = None;
                self.note("Favorites", "Pending switch cancelled.");
            }
            "model" if args == "next" => {
                self.pending_favorite = None;
                let effective = &app.config;
                let favorites = cycle_set(effective);
                if favorites.is_empty() {
                    self.note(
                        "Favorites",
                        "No favorites saved. Use Ctrl-F in /models to save one.",
                    );
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
                    self.note(
                        "Favorites",
                        "Discovering available models; the favorite switch will apply when ready. Esc or /model cancel cancels it.",
                    );
                    return Ok(Step::Done);
                };
                let available = discovery
                    .models
                    .iter()
                    .filter(|entry| {
                        entry.available && entry.supports_chat()
                    })
                    .map(|entry| entry.selection.clone())
                    .collect::<Vec<_>>();
                match next_favorite(effective, &favorites, &available) {
                    Some(selection) => {
                        let changes = selection_changes(effective, &selection, "main")?;
                        match app.save_settings(self.scope, &changes, Some("main"))
                            .and_then(|_| app.reload())
                            .map(|fresh| self.config_changed(app, agent, fresh)) {
                            Ok(step) => {
                                self.note(
                                    "Favorites",
                                    format!("Switched to {}", selection.display_name()),
                                );
                                return Ok(step);
                            }
                            Err(error) => self.note("Favorites", format!("not changed: {error:#}")),
                        }
                    }
                    None => self.note(
                        "Favorites",
                        if crate::cli::model_scope::is_active() {
                            "No available models in the --models scope."
                        } else {
                            "No available chat favorites. Use Ctrl-F in /models to save one."
                        },
                    ),
                }
            }
            "model" if matches!(args, "settings" | "setting") => {
                return self.open(app, agent, Which::ModelSettings);
            }
            "model" if args == "defaults" => {
                return self.open(app, agent, Which::ContextDefaults);
            }
            "model" | "models" if args == "local" => {
                return self.open(app, agent, Which::ModelDownloads);
            }
            "model" | "models" if args == "memory" => {
                return self.open(app, agent, Which::ModelMemory);
            }
            "model" if !args.is_empty() => {
                self.note(
                    "Model",
                    "Unknown option. Use /model to choose a model, /model settings to tune it, /model defaults for shared context defaults, /model local for the Get models downloads in /models, or /model memory for the memory role's models.",
                );
            }
            "model" | "models" => return self.open(app, agent, Which::Models),
            "roles" => return self.open(app, agent, Which::ModelRoles),
            "profile" => {
                if args.is_empty() {
                    self.note(
                        "Profiles",
                        app.config
                            .model_profiles
                            .iter()
                            .map(|(name, profile)| {
                                format!("{name}: {} / {}", profile.provider, profile.model)
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                } else if args != "off" && !app.config.model_profiles.contains_key(args) {
                    self.note("Profile", format!("Unknown profile `{args}`. Define it under [model_profiles] or run /profile to list them."));
                } else {
                    let changes = if args == "off" {
                        vec![Change::Remove {
                            path: "model_profile".into(),
                        }]
                    } else {
                        vec![Change::Set {
                            path: "model_profile".into(),
                            value: args.into(),
                        }]
                    };
                    match self.persist(app, agent, &changes, Scope::Project) {
                        Ok(step) => return Ok(step),
                        Err(error) => self.note("Profile", format!("not changed: {error:#}")),
                    }
                }
            }
            "providers" | "provider" => return self.open(app, agent, Which::Providers),
            "palette" => return self.open(app, agent, Which::Palette),
            "settings" => return self.open(app, agent, Which::Settings),
            "improve" if args == "settings" => return self.open(app, agent, Which::LearningSettings),
            "improve" if args == "models" => return self.open(app, agent, Which::LearningModels),
            "model-settings" => return self.open(app, agent, Which::ModelSettings),
            "hotkeys" => return self.open(app, agent, Which::Shortcuts),
            "accounts" => self.note(
                "Accounts",
                "Usage: /accounts pin (chooses which ChatGPT account this session uses)",
            ),
            "improve" => return self.improve_command(app, agent, args),
            "changes" => {
                let files = self.git.files(&agent.layout().unwrap_or(&app.layout).project_root);
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
            "theme" => return self.theme_command(app, agent, args),
            "new" | "clear" => match agent.layout().context("agent has no project layout").and_then(|layout| {
                Session::create(
                    layout,
                    SessionMeta {
                        model: agent.model().to_owned(),
                    },
                )
            }) {
                Ok(session) => {
                    let id = session.id().to_owned();
                    agent.swap_session(session);
                    agent.set_plan_mode(false);
                    self.stop_loop_for_session_change();
                    self.show_session(agent.session());
                    self.session_id = id.clone();
                    self.note("Session", format!("new session {}", short(&id)));
                }
                Err(error) => self.note("Session", format!("error: {error:#}")),
            },
            "autoresearch" => {
                let layout = agent.layout().ok_or_else(|| anyhow::anyhow!("agent has no project layout"))?.clone();
                let state = layout.state_dir.join("autoresearch");
                let mut words = args.splitn(5, char::is_whitespace).filter(|word| !word.is_empty());
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
                match result { Ok(text) => self.note("Autoresearch", text), Err(error) => self.note("Autoresearch", format!("{error:#}")) }
            }
            "archive" => {
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
            "sessions" | "resume" | "tree" => {
                match self.pick_session(agent, args, name == "tree", name == "resume") {
                    Ok(Some(session)) => {
                        self.show_session(&session);
                        agent.swap_session(session);
                        agent.set_plan_mode(false);
                        self.stop_loop_for_session_change();
                        self.note("Resume", Self::resume_note(agent.session()));
                    }
                    Ok(None) => {}
                    Err(error) => self.note("Sessions", format!("{error:#}")),
                }
            }
            "rename" => match agent.session_mut().rename(args) {
                Ok(()) => self.note("Session", format!("renamed to {}", agent.session().title())),
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
                match agent.session().export(Path::new(&path), html) {
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
            "permissions" => {
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
                    self.note("Permissions", "usage: /permissions; /permissions allow|deny [--persist] <tool> [prefix words | --path <glob>…]; /permissions remove <number> [--persist]");
                }
            }
            "cost" => {
                let (total, turn) = agent.usage();
                if total.all.requests == 0 {
                    self.note(
                        "Cost and usage",
                        "No model requests yet in this session.\nCost and token counts appear here after the first reply.",
                    );
                    return Ok(Step::Done);
                }
                // Summary first; the accounting detail (attempts, reports, pricing coverage) follows
                // per role as `key: value` rows.
                let describe = |role: &str, c: &usage::Counts| format!(
                    "{role}\n  cost: {} · requests: {} · retries: {} · failed: {}\n  tokens in: {} · out: {} · cached reads: {} · cache writes: {} · reasoning: {}\n  priced: {} · unpriced: {} · local: {} · subscription: {}\n  attempts: {} · accepted: {} · discarded: {} · without complete usage: {} · usage reports: {} · cache reports: {} · reasoning reports: {}",
                    c.cost_label(), c.requests, c.retries, c.failed_requests,
                    c.input, c.output, c.cache_read, c.cache_write, c.reasoning,
                    c.priced_requests, c.unpriced_requests, c.local_requests, c.subscription_requests,
                    c.transport_attempts, c.successful_requests, c.discarded_attempts,
                    c.unreported_attempts, c.reported_responses,
                    c.cache_reported_responses, c.reasoning_reported_responses,
                );
                let turn_label = turn
                    .coverage
                    .turn_id
                    .map_or_else(|| "this turn".to_owned(), |id| format!("turn {id}"));
                let mut text = format!(
                    "Session: {} · requests: {} · tokens in: {} · out: {}\nCurrent {turn_label}: {}\nCoverage: {}\n\n{}\n\n{}",
                    total.all.cost_label(), total.all.requests, total.all.input, total.all.output,
                    turn.all.cost_label(), total.coverage.history,
                    describe("Main agent", &total.main), describe("Workers", &total.workers),
                );
                for (model, counts) in &total.by_model {
                    text.push_str(&format!("\n\nModel {model}\n{}\n{}",
                        describe("  Main agent", &counts.main).replace("\n  ", "\n    "),
                        describe("  Workers", &counts.workers).replace("\n  ", "\n    ")));
                }
                text.push_str("\n\nMissing usage or prices are unknown, not free; local and subscription usage is not an invoice estimate. Caps stop on observed cost before the next step; in-flight workers and discarded attempts can add unobserved charges. Accounting for earlier (resumed) sessions is unavailable.");
                self.note("Cost and usage", text);
            }
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
            "fork" => match agent.layout().context("agent has no project layout").and_then(|layout| agent.session().fork(layout)) {
                Ok(session) => {
                    let id = session.id().to_owned();
                    agent.swap_session(session);
                    agent.set_plan_mode(false);
                    self.stop_loop_for_session_change();
                    self.session_id = id.clone();
                    self.note("Session", format!("forked into {}", short(&id)));
                }
                Err(error) => self.note("Session", format!("error: {error:#}")),
            },
            "compact" => {
                let mut options = match crate::config::compaction::CompactionOptions::from_args(args) {
                    Ok(options) => options,
                    Err(error) => { self.note("Context", format!("{error:#}")); return Ok(Step::Done); }
                };
                options.suppress_continuation = true;
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
            }
            "undo" => {
                let mut lines = Vec::new();
                if let (Some(trajectory), Some(store)) = (self.last_trajectory, app.store.as_ref())
                {
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
                if let Some(checkpoints) = agent.checkpoints() {
                    match checkpoints.rollback(1) {
                        Ok(report) => lines.push(format!(
                            "restored {} file(s), removed {}",
                            report.restored.len(),
                            report.removed.len()
                        )),
                        Err(error) => lines.push(format!("rollback: {error:#}")),
                    }
                }
                agent.cancel_background_compaction();
                match crate::app::commands::branch::rewind(app, agent, 1) {
                    Ok(rewound) => {
                        lines.push(format!("dropped {} message(s)", rewound.dropped));
                        lines.extend(rewound.note);
                    }
                    Err(error) => lines.push(format!("rewind: {error:#}")),
                }
                self.note("Undo", lines.join("\n"));
            }
            "rollback" => {
                let turns = if args.is_empty() { Ok(1) } else { args.parse::<usize>() };
                let Ok(turns) = turns else {
                    self.note("Rollback", "Usage: /rollback [n] with a whole number of turns.");
                    return Ok(Step::Done);
                };
                match agent.checkpoints() {
                    Some(checkpoints) => match checkpoints.rollback(turns) {
                        Ok(report) => self.note(
                            "Rollback",
                            format!(
                                "restored {:?}, removed {:?}",
                                report.restored, report.removed
                            ),
                        ),
                        Err(error) => self.note("Rollback", format!("{error:#}")),
                    },
                    None => self.note("Rollback", "checkpoints unavailable"),
                }
            }
            "skills" => {
                let words = crate::runtime::arguments::split_words(args);
                if matches!(words.first().map(String::as_str), Some("install" | "update")) {
                    // Network-bound: run in the job slot so the UI keeps responding; progress streams in.
                    let layout = app.layout.clone();
                    let registries = app.config.skills.registries.clone();
                    let started = start_streaming_job(&mut self.auth_job, "Skills", move |cancel, progress| {
                        skills::management::command(&layout, &registries, words, &cancel, progress)
                    });
                    if !started {
                        self.note("Skills", "Another background job is running; try again when it finishes.");
                    }
                } else {
                    let result = skills::management::command(
                        &app.layout,
                        &app.config.skills.registries,
                        words,
                        &std::sync::atomic::AtomicBool::new(false),
                        &mut |_: &str| {},
                    );
                    match result {
                        Ok(text) => self.note("Skills", text),
                        Err(error) => self.note("Skills", format!("{error:#}")),
                    }
                }
                self.refresh_commands(app);
            }
            "laya" => {
                if matches!(args, "on" | "off") {
                    let enabled = args == "on";
                    let step = self.persist(
                        app,
                        agent,
                        &[Change::Set {
                            path: "laya.enabled".into(),
                            value: toml::Value::Boolean(enabled),
                        }],
                        Scope::Project,
                    )?;
                    self.note("Laya",if enabled {"Decision advice enabled for this project. /laya check probes Dial's managed service."} else {"Decision advice disabled for this project. The managed service can shut down when idle."});
                    return Ok(step);
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
            "code-history" => {
                use crate::tools::knowledge::history::{self, ChangeHistory, Request};
                if !(app.config.codebase.enabled && app.config.codebase.history) {
                    self.note("Code history", "Code history is off. Turn it on in /settings → Project understanding → Code change history (codebase.history).");
                    return Ok(Step::Done);
                }
                let request = match history::parse_request(args) {
                    Ok(request) => request,
                    Err(usage) => {
                        self.note("Code history", usage);
                        return Ok(Step::Done);
                    }
                };
                if request == Request::Clear
                    && self.ask("Clear code history", "Delete every recorded diff for this project? Files are not changed.", &[("Clear code history", 'y'), ("Keep it", 'n')]) != Some(0)
                {
                    return Ok(Step::Done);
                }
                let text = ChangeHistory::open(&app.layout).and_then(|store| match request {
                    Request::Search { query, path, branch } => {
                        let found = store.search(&query, path.as_deref(), branch.as_deref(), 30)?;
                        Ok(format!("{}\n/code-history show <id> prints a diff · filter with path:<file|dir> branch:<name>", history::render_list(&found)))
                    }
                    Request::Show(id) => Ok(match store.show(id)? {
                        Some((version, diff)) => history::render_version(&version, &diff),
                        None => format!("No recorded version #{id}."),
                    }),
                    Request::Clear => Ok(format!("Cleared {} recorded versions.", store.clear()?)),
                });
                self.note("Code history", text.unwrap_or_else(|error| format!("{error:#}")));
            }
            "understanding" if args == "clear" => {
                let result = crate::tools::knowledge::codebase::Codebase::open(&app.layout)
                    .and_then(|cache| cache.clear_architecture());
                match result {
                    Ok(removed) => self.note(
                        "Understanding",
                        format!("Cleared saved architecture synthesis ({removed} records)."),
                    ),
                    Err(error) => self.note("Understanding", format!("error: {error:#}")),
                }
            }
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
            "experience" => {
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
                                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                    bridge.heartbeat()
                                }
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
            }
            "understanding" => {
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
            }
            "memory" => {
                app.interrupt.store(false, Ordering::SeqCst);
                let needs_compute = app.config.memory_uses_local_compute()
                    && !matches!(args.split_whitespace().next(), None | Some("status" | "queue"));
                if needs_compute && !self.wait_compute_required(app, true, false)? {
                    return Ok(Step::Done);
                }
                self.composer.set_busy(true);
                let result = {
                    let mut bridge = Bridge {
                        app, shell: self, interrupt: app.interrupt.clone(),
                        started: Instant::now(), last_paint: Instant::now() - PAINT_EVERY,
                        failed: false, pending: Vec::new(), next_id: 0,
                    };
                    agent.selected_memory_command(args, &mut bridge)
                        .and_then(|value| {
                            // `/memory help` answers with text (a `help` field or a bare string).
                            if let Some(help) = value
                                .get("help")
                                .and_then(Value::as_str)
                                .or_else(|| value.as_str())
                            {
                                Ok(help.to_owned())
                            } else if matches!(args.trim(), "" | "status") && value.get("backend").is_some() {
                                Ok(memory_status_text(&value))
                            } else {
                                serde_json::to_string_pretty(&value).map_err(Into::into)
                            }
                        })
                };
                self.composer.set_busy(false);
                self.note("Memory", result.unwrap_or_else(|error| format!("error: {error:#}")));
            }
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
            "search" if args.trim().is_empty() => self.note("Search", Self::SEARCH_USAGE),
            "search" => match agent.layout().ok_or_else(|| anyhow::anyhow!("agent has no project layout")).and_then(|layout| Session::search(layout, args, 8)) {
                Ok(hits) if hits.is_empty() => self.note("Search", format!("No past messages match \"{}\".", args.trim())),
                Ok(hits) => self.note(
                    "Search",
                    hits.iter()
                        .map(|hit| {
                            format!("{} [{}] {}", short(&hit.session_id), hit.role, hit.snippet)
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Err(error) => self.note("Search", format!("error: {error:#}")),
            },
            "context" => {
                let used = agent.last_prompt_tokens;
                let window = u64::from(agent.context_window());
                let percent = (used * 100).checked_div(window).unwrap_or(0);
                self.note(
                    "Context",
                    format!(
                        "used: {} of {} tokens ({percent}%) · messages: {}\n/compact summarizes older turns to free room; /new starts fresh.",
                        group_digits(used),
                        group_digits(window),
                        agent.session().messages().len()
                    ),
                )
            }
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
                let meaning = agent.approval().description();
                self.note(
                    "Approval",
                    format!("mode: {mode} · {meaning}\nChange it with /approval ask|write|auto|readonly or Shift+Tab."),
                );
            }
            "outcome" => {
                let (score, comment) = args
                    .split_once(char::is_whitespace)
                    .map_or((args, ""), |(score, comment)| (score, comment.trim()));
                match (
                    score.parse::<f64>().ok(),
                    self.last_trajectory,
                    app.store.as_ref(),
                ) {
                    (Some(score), Some(trajectory), Some(store)) => {
                        let outcome = Outcome {
                            trajectory,
                            score,
                            source: "user".into(),
                            reference: (!comment.is_empty()).then(|| comment.to_owned()),
                        };
                        match store.outcome(&outcome) {
                            Ok(()) => {
                                self.note("Outcome", format!("recorded {score} for trajectory {trajectory}"));
                                self.start_live_monitor("Outcome", app.config.clone());
                            }
                            Err(error) => self.transcript.error(&format!("{error:#}")),
                        }
                    }
                    (None, _, _) => self.note("Outcome", Self::OUTCOME_USAGE),
                    (_, None, _) => self.note("Outcome", "No completed turn to score yet. Finish a turn, then rate it with /outcome <0-100>."),
                    (_, _, None) => self.note("Outcome", TEACH_OFF),
                }
            }
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
                None => self.note(
                    "Command",
                    format!("unknown command /{name}; /help lists commands"),
                ),
            },
        }
        Ok(Step::Done)
    }
}
