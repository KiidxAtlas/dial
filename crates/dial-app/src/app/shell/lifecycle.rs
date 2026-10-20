//! Shell start, adoption of a new configuration, and teardown.

use crate::agent;
use crate::agent::window::context;
use crate::agent::workflow::todo::TodoList;
use crate::app::command_registry::command_entries;
use crate::app::completions::command_argument_completions;
use crate::app::core::App;
use crate::app::discovery::Catalog;
use crate::app::display::{config_key, session_age, terminal_dark};
use crate::app::git::{Git, git_output};
use crate::app::learning::Learning;
use crate::app::replay::replay_session;
use crate::app::resources::{
    RESOURCE_TTL, RESOURCE_WATCHED_FALLBACK, ResourceWatch, resource_events, resources_fingerprint,
    settings_fingerprint,
};
use crate::app::shell::Shell;
use crate::app::shell::teach::TeachState;
use crate::config::settings_store::{self, Scope};
use crate::storage::session::Session;
use crate::storage::skills;
use crate::ui::composer::Composer;
use crate::ui::keys::{Action, Keymap};
use crate::ui::status::StatusData;
use crate::ui::term::InlineTerminal;
use crate::ui::theme::ThemeManager;
use crate::ui::transcript::Transcript;
use crate::ui::welcome::{self, WelcomeInfo};
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::time::Instant;

impl Shell {
    pub(in crate::app) fn new(app: &App, startup: String) -> Result<Shell> {
        let mut term = InlineTerminal::enter()?;
        term.clear_screen()?;
        let dark = terminal_dark(std::env::var("COLORFGBG").ok().as_deref());
        let (keys, warnings) = Keymap::load(&app.layout.user_dir);
        let themes = ThemeManager::new(&app.layout.user_dir, &app.config.ui, dark);
        let history = settings_store::load_history(&app.layout, app.config.ui.history_limit)
            .unwrap_or_default();
        let commands = context::discover_commands(&app.layout);
        let mut shell = Shell {
            loop_mode: Default::default(),
            runtime: Default::default(),
            pending_result: None,
            shared_prose: false,
            auth_job: None,
            extensions_pending: None,
            extensions_job: None,
            restart_requested: false,
            retry_noted: false,
            smithery_resume: None,
            commit_job: None,
            term,
            themes,
            keys,
            composer: {
                let mut composer = Composer::new(history, app.config.ui.history_limit);
                composer.set_history_provider(crate::storage::archive::history_provider(
                    &app.layout.user_dir,
                ));
                composer.set_word_completion(app.config.ui.word_completion);
                composer.set_project_root(app.layout.project_root.clone());
                composer.load_learned_words(app.layout.user_dir.join("learned-words.json"));
                composer
            },
            paste_guard: Default::default(),
            transcript: Transcript::new(),
            status: StatusData::default(),
            ui: app.config.ui.clone(),
            usage_status: crate::app::commands::UsageStatus::new(app.layout.user_dir.clone()),
            catalog: Catalog::new(),
            git: Git::default(),
            learning: Learning::default(),
            yield_compute: false,
            adapter_monitor: None,
            request_policy: Value::Null,
            commands,
            scope: Scope::Global,
            tools_expanded: false,
            home: None,
            started: Instant::now(),
            request_phase: agent::RequestPhase::Waiting,
            last_trajectory: None,
            teach_pending: Vec::new(),
            teach_hint_shown: false,
            teach_state: TeachState::of(app),
            teach_nudge_stage: 0,
            improve_watch: None,
            startup: (!startup.trim().is_empty()).then_some(startup),
            cfg_approval: app.config.approval,
            dark,
            session_id: String::new(),
            title: format!(
                "dial · {}",
                app.layout
                    .project_root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
            turn_output_tokens: 0,
            todo: TodoList::new(),
            skills: Vec::new(),
            deferred: Default::default(),
            deferred_editor: false,
            deferred_approval_cycle: false,
            deferred_display_reset: false,
            held: Default::default(),
            live_tasks: None,
            workers: Default::default(),
            recent_worker: None,
            background_control: None,
            pty_approval: app.config.approval,
            maintenance_control: None,
            skills_listing: String::new(),
            pending_config: None,
            busy_overlay: None,
            pending_verification: None,
            pending_resume: None,
            pending_favorite: None,
            resources: ResourceWatch {
                at: Instant::now() - RESOURCE_TTL,
                settings: None,
                events: resource_events(&app.layout),
                resources: None,
                diagnostic: None,
                reload: settings_store::SettingsReload::new(&app.layout, &app.overlay, &app.config),
            },
            pending_resource_rebuild: false,
            voice_input: None,
            voice_gesture: Default::default(),
            speech: None,
        };
        for warning in warnings {
            shell.note("Key bindings", &warning);
        }
        if let Some(error) = shell.themes.last_error() {
            let error = error.to_owned();
            shell.note("Theme", &error);
        }
        if let Some(error) = &app.store_error {
            shell.note("Learning", error);
        }
        shell.refresh_commands(app);
        Ok(shell)
    }

    /// Observe disk on a bounded timer. A running agent only receives a pending snapshot.
    pub(in crate::app) fn refresh_resources(&mut self, app: &App, force: bool, busy: bool) -> bool {
        let signalled = self.resources.events.as_ref().is_some_and(|events| {
            events
                .dirty
                .swap(false, std::sync::atomic::Ordering::AcqRel)
        });
        let interval = match &self.resources.events {
            Some(events) if events.complete => RESOURCE_WATCHED_FALLBACK,
            _ => RESOURCE_TTL,
        };
        if !force && !signalled && self.resources.at.elapsed() < interval {
            return false;
        }
        self.resources.at = Instant::now();
        let mut diagnostics = Vec::new();
        let mut changed = false;
        match settings_fingerprint(&app.layout) {
            Ok(stamp) if force || self.resources.settings != Some(stamp) => {
                self.resources.settings = Some(stamp);
                let (config, errors) = self.resources.reload.reload(
                    &app.layout,
                    &app.overlay,
                    app.models.borrow().as_ref(),
                );
                diagnostics.extend(errors);
                // Compare the same coding view used by App::new/adopt. Saved learning
                // preferences differ intentionally and are not an external settings edit.
                let config = config.for_coding();
                let key = config_key(&config);
                let different = key != config_key(&app.config);
                let pending_changed = self
                    .pending_config
                    .as_ref()
                    .is_none_or(|pending| config_key(pending) != key);
                self.pending_config = different.then_some(config);
                if different && pending_changed {
                    self.note(
                        "Settings",
                        if busy {
                            "External settings validated; they will apply after the current turn."
                        } else {
                            "External settings validated; applying them at the idle boundary."
                        },
                    );
                }
                changed = true;
            }
            Ok(_) => {}
            Err(error) => diagnostics.push(format!(
                "Settings change check: {error:#}; keeping last good settings"
            )),
        }
        let stamp = match resources_fingerprint(&app.layout) {
            Ok(stamp) => Some(stamp),
            Err(error) => {
                diagnostics.push(format!("Resource change check: {error:#}"));
                None
            }
        };
        if force || stamp.is_some_and(|stamp| self.resources.resources != Some(stamp)) {
            let had_resources = self.resources.resources.is_some();
            self.resources.resources = stamp;
            diagnostics.extend(self.reload_disk_resources(app));
            // Skills and prompt resources are captured when an agent is constructed.
            self.pending_resource_rebuild |= had_resources || force;
            changed = true;
        }
        let diagnostic = (!diagnostics.is_empty()).then(|| diagnostics.join("\n"));
        if diagnostic != self.resources.diagnostic {
            if let Some(error) = &diagnostic {
                self.note("Reload", error);
                changed = true;
            }
            self.resources.diagnostic = diagnostic;
        }
        if force {
            self.note("Reload", if busy {
                "Disk resources refreshed; validated settings and agent context will apply after the current turn. Invalid edits retain the last good values."
            } else {
                "Disk resources refreshed. Validated settings and agent context apply at this idle boundary; invalid edits retain the last good values."
            });
        }
        changed
    }

    fn reload_disk_resources(&mut self, app: &App) -> Vec<String> {
        let mut diagnostics = Vec::new();
        let key_path = app.layout.user_dir.join("keybindings.toml");
        match fs::read_to_string(&key_path) {
            Ok(text) => match text.parse::<toml::Table>() {
                Ok(table) => {
                    let (keys, warnings) = Keymap::from_table(&table);
                    self.keys = keys;
                    diagnostics.extend(warnings);
                }
                Err(error) => diagnostics.push(format!(
                    "{}: {error}; keeping last good key bindings",
                    key_path.display()
                )),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.keys = Keymap::defaults()
            }
            Err(error) => diagnostics.push(format!(
                "{}: {error}; keeping last good key bindings",
                key_path.display()
            )),
        }
        let themes = ThemeManager::new(&app.layout.user_dir, &self.ui, self.dark);
        if let Some(error) = themes.last_error() {
            diagnostics.push(format!("{error}; keeping last good theme"));
        } else {
            self.themes = themes;
        }
        self.refresh_command_resources(app);
        diagnostics
    }

    fn refresh_command_resources(&mut self, app: &App) {
        // Replace, rather than append, so deletion and same-name overrides agree across
        // completion, palette, help and dispatch.
        self.commands = context::discover_commands(&app.layout);
        let found = skills::discover(&app.layout);
        self.composer
            .set_commands(command_entries(&self.commands, &found));
        let mut arguments =
            command_argument_completions(&app.config, &skills::discover_all(&app.layout));
        arguments.extend(
            self.themes
                .names()
                .into_iter()
                .filter(|entry| entry.error.is_none())
                .map(|entry| format!("/theme {}", entry.name)),
        );
        self.composer.set_argument_completions(arguments);
        self.skills = found;
        self.skills_listing =
            skills::management::list(&app.layout).unwrap_or_else(|error| format!("{error:#}"));
    }

    pub(super) fn refresh_commands(&mut self, app: &App) {
        self.refresh_command_resources(app);
        let paths = ignore::WalkBuilder::new(&app.layout.project_root)
            .follow_links(false)
            .require_git(false)
            .build()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .filter_map(|entry| {
                entry
                    .path()
                    .strip_prefix(&app.layout.project_root)
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .filter(|path| !path.chars().any(|c| c.is_control() || c.is_whitespace()))
            .collect();
        self.composer.set_files(paths);
    }

    /// Home screen and, for a resumed session, its earlier conversation.
    pub(in crate::app) fn begin(&mut self, app: &App, session: &Session, resumed: bool) {
        self.session_id = session.id().to_owned();
        if self.ui.welcome {
            let (sessions, recent): (Vec<String>, Vec<(String, String)>) =
                Session::list_newest(&app.layout, 24)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|s| s.id != session.id() && s.messages > 0)
                    // A session that only ever ran a slash command is not a conversation to resume.
                    .filter(|s| !s.first_message.trim_start().starts_with('/'))
                    .map(|s| {
                        // Untitled sessions are named by their first message, else by size.
                        let title = [s.title.trim(), s.first_message.trim()]
                            .into_iter()
                            .find(|t| !t.is_empty())
                            .map_or_else(
                                || format!("untitled · {} messages", s.messages),
                                str::to_owned,
                            );
                        (s.id.clone(), (title, session_age(&s.started)))
                    })
                    .take(3)
                    .unzip();
            // The composer's bottom border already offers commands, actions, model and settings;
            // the home adds only what it does not.
            let tips = if self.ui.tips {
                vec![
                    ("@".to_owned(), "attach files".to_owned()),
                    ("!".to_owned(), "run a shell command".to_owned()),
                    (
                        "/extensions".to_owned(),
                        "MCP servers and plugins".to_owned(),
                    ),
                ]
            } else {
                Vec::new()
            };
            let selection = app.config.selection();
            // The status line reads git in the background, so it is still empty when the home is
            // first drawn; ask git directly (one fast local call).
            let branch = if self.status.branch.is_empty() {
                git_output(
                    &app.layout.project_root,
                    &["symbolic-ref", "--quiet", "--short", "HEAD"],
                )
                .map(|b| b.trim().to_owned())
                .unwrap_or_default()
            } else {
                self.status.branch.clone()
            };
            let info = WelcomeInfo {
                version: env!("CARGO_PKG_VERSION").into(),
                provider: selection.provider.clone(),
                model: selection
                    .display_name()
                    .trim_start_matches(&format!("{}/", selection.provider))
                    .to_owned(),
                project: {
                    let root = app.layout.project_root.display().to_string();
                    match std::env::var("HOME") {
                        Ok(home)
                            if !home.is_empty()
                                && (root == home || root.starts_with(&format!("{home}/"))) =>
                        {
                            format!("~{}", &root[home.len()..])
                        }
                        _ => root,
                    }
                },
                tips,
                recent,
                resume_hint: format!(
                    "{} to pick one, Enter to resume · /resume for all",
                    if self.themes.current().ascii() {
                        "Down"
                    } else {
                        "\u{2193}"
                    }
                ),
                setup_hint: format!(
                    "{} or /providers to sign in to a hosted model; /setup installs the local model runtime",
                    self.keys.label(Action::ModelSelect)
                ),
                branch,
                approval: format!("{:?}", app.config.approval).to_lowercase(),
                sandbox: format!("{:?}", app.config.sandbox.mode).to_lowercase(),
            };
            let motion = crate::ui::fx::motion(self.ui.motion, self.themes.current());
            self.home = Some(welcome::Home::new(info, sessions, motion));
        }
        if resumed {
            replay_session(&mut self.transcript, session);
            self.note("Resume", Self::resume_note(session));
        }
    }

    pub(in crate::app) fn after_adopt(&mut self, app: &App) {
        self.teach_changed(app);
        self.ui = app.config.ui.clone();
        self.cfg_approval = app.config.approval;
        self.catalog.refresh(app);
        let themes = ThemeManager::new(&app.layout.user_dir, &app.config.ui, self.dark);
        if let Some(error) = themes.last_error() {
            self.note("Theme", format!("{error}; keeping last good theme"));
        } else {
            self.themes = themes;
        }
        self.refresh_command_resources(app);
        self.pending_resource_rebuild = false;
    }

    pub(in crate::app) fn finish(&mut self) -> Option<String> {
        self.auth_job.take();
        // A read-out or microphone capture must not outlive the shell.
        self.stop_speech();
        if let Some(capture) = self.voice_input.take() {
            capture.stop();
        }
        // Whatever is still pending belongs in the scrollback before the live region is erased.
        let _ = self.flush();
        self.term.leave();
        (!self.session_id.is_empty()).then(|| self.session_id.clone())
    }

    /// Commit everything finalized so the conversation survives in scrollback.
    pub(in crate::app) fn flush(&mut self) -> Result<()> {
        self.transcript.finish_turn();
        self.paint(None)
    }
}
