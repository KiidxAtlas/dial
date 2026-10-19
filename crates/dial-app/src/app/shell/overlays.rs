//! Opening, running and finishing overlays (including busy overlays) from the shell.

use crate::agent::Agent;
use crate::app::command_registry::palette_entries;
use crate::app::core::App;
use crate::app::display::config_key;
use crate::app::drive::Step;
use crate::app::improve_view::improve_view;
use crate::app::notices::credential_sources;
use crate::app::overlay::compute::{ComputeMonitor, overlay_compute_release};
use crate::app::overlay::env::{OverlayEnv, favorite_routes, settings_tables};
use crate::app::overlay::run::{
    BusyOverlay, draw_overlay_frame, overlay_input, refresh_overlay, run_overlay,
};
use crate::app::overlay::settings::{
    apply_settings_hints, embedding_setup_offers, setting_setup_offers, settings_embedding_models,
};
use crate::app::overlay::{ActiveOverlay, OverlayOutcome, Which, overlay_destination};
use crate::app::selection::{local_suggestion_base, model_settings_profile};
use crate::app::shell::Shell;
use crate::config::types::Config;
use crate::llm::catalog;
use crate::ui::overlay::compute::ComputeOverlay;
use crate::ui::overlay::models::ModelOverlay;
use crate::ui::overlay::palette::PaletteOverlay;
use crate::ui::overlay::providers::ProviderOverlay;
use crate::ui::overlay::settings::SettingsOverlay;
use crate::ui::overlay::shortcuts::ShortcutOverlay;
use crate::ui::status::Phase;
use crate::ui::theme::ThemeManager;
use anyhow::Result;
use crossterm::event::Event;

impl Shell {
    pub(in crate::app) fn open(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        which: Which,
    ) -> Result<Step> {
        let mut which = which;
        // A settings change that needs a rebuild is carried while the overlay chain keeps
        // navigating, so the requested panel still opens and the rebuild still happens.
        let mut carried = Step::Done;
        loop {
            let outcome = self.run_overlay(app, Some(&mut *agent), which)?;
            if let Some(config) = outcome.latest {
                carried = self.config_changed(app, agent, config);
            }
            if let Some(selection) = outcome.verify_lora {
                self.verify_lora(app, agent, &selection)?;
                return Ok(carried);
            }
            if let Some(command) = outcome
                .next
                .as_deref()
                .and_then(|next| next.strip_prefix("command:"))
            {
                return Ok(match self.command(app, agent, command)? {
                    Step::Done => carried,
                    step => step,
                });
            }
            if let Some(prompt) = outcome
                .next
                .as_deref()
                .and_then(|next| next.strip_prefix("turn:"))
            {
                return Ok(match self.submit(app, agent, prompt.to_owned())? {
                    Step::Done => carried,
                    step => step,
                });
            }
            if let Some(payload) = outcome
                .next
                .as_deref()
                .and_then(|next| next.strip_prefix("subagent-resume:"))
            {
                let id = payload.split('\n').next().unwrap_or_default().to_owned();
                self.resume_worker(app, agent, payload)?;
                // The viewer stays open and tails the resumed run.
                which = Which::SubagentViewer {
                    id,
                    entry: None,
                    from_hub: true,
                };
                continue;
            }
            if let Some(id) = outcome
                .next
                .as_deref()
                .and_then(|next| next.strip_prefix("test:"))
            {
                // The catalog scan probes each provider's endpoint; the reopened page shows the result.
                self.catalog.refresh(app);
                self.note(
                    "Connection test",
                    format!("Checking {id}. Its status appears in the provider list when the check finishes."),
                );
                which = Which::Providers;
                continue;
            }
            match outcome.next.as_deref().and_then(overlay_destination) {
                Some(destination) => which = destination,
                None => return Ok(carried),
            }
        }
    }

    /// `/git [revision]`: the panel on the working tree, or read-only on a pinned revision.
    pub(crate) fn open_git(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        revision: &str,
    ) -> Result<Step> {
        let which = Which::Git {
            root: agent.workspace().root().to_path_buf(),
            revision: revision.to_owned(),
        };
        self.open(app, agent, which)
    }

    /// Apply UI/approval now, then adopt the authoritative config at this safe boundary.
    pub(in crate::app) fn config_changed(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        fresh: Config,
    ) -> Step {
        let fresh = fresh.for_coding();
        self.ui = fresh.ui.clone();
        self.composer.set_history_limit(fresh.ui.history_limit);
        self.composer.set_word_completion(fresh.ui.word_completion);
        let themes = ThemeManager::new(&app.layout.user_dir, &fresh.ui, self.dark);
        if let Some(error) = themes.last_error() {
            self.note("Theme", format!("{error}; keeping last good theme"));
        } else {
            self.themes = themes;
        }
        if fresh.approval != self.cfg_approval {
            self.cfg_approval = fresh.approval;
            agent.set_approval(fresh.approval);
        }
        if config_key(&fresh) != config_key(&app.config) {
            Step::Rebuild(Box::new(fresh))
        } else {
            Step::Done
        }
    }

    fn make_overlay(&mut self, app: &App, which: Which) -> Result<ActiveOverlay> {
        self.refresh_resources(app, false, self.status.phase != Phase::Idle);
        if matches!(
            which,
            Which::Models
                | Which::ModelRoles
                | Which::LearningModels
                | Which::ModelDownloads
                | Which::ModelMemory
                | Which::Providers
        ) {
            self.catalog.start(app);
        }
        let effective = match app.reload() {
            Ok(config) => config,
            Err(error) => {
                self.note("Settings", format!("{error:#}; using last good settings"));
                self.resources.reload.config().clone()
            }
        };
        let discovery = self.catalog.get();
        let learning_settings = matches!(which, Which::LearningSettings);
        let context_defaults = matches!(which, Which::ContextDefaults);
        let learning_models = matches!(which, Which::LearningModels);
        let overlay = match which {
            Which::Choice(spec) => ActiveOverlay::Palette(PaletteOverlay::choice(*spec)),
            Which::Palette => ActiveOverlay::Palette(PaletteOverlay::new(
                palette_entries(&self.commands, &self.skills),
                self.status.phase != Phase::Idle,
            )),
            Which::Compute => ActiveOverlay::Compute(ComputeOverlay::new()),
            Which::Improve => ActiveOverlay::Improve(Box::new(
                crate::ui::overlay::improve::ImproveOverlay::new(improve_view(app)),
            )),
            Which::Subagents => ActiveOverlay::Subagents(Box::new(
                crate::ui::subagent_hub::SubagentHub::new(self.session_id.clone()),
            )),
            Which::SubagentsActivity => ActiveOverlay::Subagents(Box::new(
                crate::ui::subagent_hub::SubagentHub::open_activity(self.session_id.clone()),
            )),
            Which::SubagentViewer {
                id,
                entry,
                from_hub,
            } => ActiveOverlay::SubagentViewer(Box::new(
                crate::ui::overlay::subagent_viewer::SubagentViewer::new(
                    id,
                    self.session_id.clone(),
                    entry,
                    from_hub,
                    self.ui.clone(),
                    std::time::Instant::now(),
                ),
            )),
            Which::Extensions => ActiveOverlay::Extensions(Box::new(
                crate::app::commands::extensions_browser::overlay(
                    app.layout(),
                    &effective,
                    self.extensions_job.as_ref(),
                ),
            )),
            Which::Shortcuts => ActiveOverlay::Shortcuts(ShortcutOverlay::new()),
            Which::Jobs => {
                ActiveOverlay::Jobs(Box::new(crate::ui::overlay::jobs::JobsOverlay::new()))
            }
            Which::Git { root, revision } => ActiveOverlay::Git(Box::new(
                crate::ui::overlay::git::GitOverlay::open(
                    crate::git_panel::Repo::new(&root, &effective),
                    (!revision.is_empty()).then_some(revision.as_str()),
                )
                .map_err(anyhow::Error::msg)?,
            )),
            Which::ModelSettings => {
                let (name, profile) = model_settings_profile(&effective);
                let mut overlay =
                    crate::ui::overlay::model_settings::ModelSettingsOverlay::new(name, profile);
                if effective.assistance.local.suggestions {
                    overlay = overlay
                        .with_suggestions(&effective.user_dir, local_suggestion_base(&effective));
                }
                ActiveOverlay::ModelSettings(Box::new(overlay))
            }
            Which::Settings | Which::LearningSettings | Which::ContextDefaults => {
                let (values, global, project) = settings_tables(app, &effective)?;
                let themes = self
                    .themes
                    .names()
                    .into_iter()
                    .filter(|e| e.error.is_none())
                    .map(|e| e.name)
                    .collect();
                let overlay = SettingsOverlay::new(values, project, global, self.scope, themes);
                let mut overlay = if learning_settings {
                    overlay.learning_only()
                } else {
                    overlay.coding_only()
                };
                if context_defaults {
                    overlay = overlay.context_defaults();
                }
                apply_settings_hints(&mut overlay, &effective);
                overlay.set_setup_offers(setting_setup_offers(&app.layout.user_dir, &effective));
                // The Embedding model row lists discovered embedding models and curated setups.
                if effective.memory.backend
                    == crate::config::memory_backend::MemoryBackendId::Semantic
                {
                    self.catalog.start(app);
                }
                overlay.set_embedding_models(settings_embedding_models(discovery.as_ref()));
                overlay.set_embedding_setups(embedding_setup_offers(&app.layout.user_dir));
                ActiveOverlay::Settings(Box::new(overlay))
            }
            Which::Models
            | Which::ModelRoles
            | Which::LearningModels
            | Which::ModelDownloads
            | Which::ModelMemory => {
                let mut models =
                    ModelOverlay::new(discovery, effective.role_statuses(), self.scope)
                        .with_favorites(favorite_routes(&effective))
                        .with_memory_required(crate::config::memory_backend::requires_memory_role(
                            &effective.memory,
                        ))
                        .with_main_lora(self.status.lora)
                        .with_learning(learning_models)
                        .with_usage(&app.layout.user_dir)
                        .with_downloads(
                            app.layout(),
                            crate::stats::local_models::Settings::from_config(&effective),
                            effective.python.clone(),
                        );
                models.set_memory_optional(
                    crate::config::memory_backend::auto_retain_uses_memory_role(&effective.memory),
                );
                models.set_memory_backend(
                    crate::config::memory_backend::active_backend(&effective.memory).as_str(),
                );
                match which {
                    Which::ModelRoles => models.show_roles(),
                    Which::ModelDownloads => models.show_downloads(),
                    Which::ModelMemory => models.show_memory(),
                    _ => {}
                }
                ActiveOverlay::Models(Box::new(models))
            }
            Which::Providers => {
                let user_dir = app.layout.user_dir.clone();
                let has_key = Box::new(move |url: &str| {
                    catalog::credential_token(&user_dir, url)
                        .ok()
                        .flatten()
                        .is_some()
                });
                let credential_sources_for_overlay = credential_sources(
                    &app.layout.user_dir,
                    &effective,
                    discovery.as_ref().map_or(&[], |d| d.providers.as_slice()),
                );
                let mut providers =
                    ProviderOverlay::new(discovery, effective.providers, has_key, self.scope);
                providers.set_chatgpt_accounts(
                    crate::llm::chatgpt::accounts(&app.layout.user_dir).unwrap_or_default(),
                );
                providers.set_anthropic_signed_in(crate::llm::anthropic_oauth::signed_in(
                    &app.layout.user_dir,
                ));
                providers.set_chatgpt_pending(
                    self.auth_job
                        .as_ref()
                        .is_some_and(|job| job.label == "ChatGPT"),
                );
                providers.set_credential_sources(credential_sources_for_overlay);
                ActiveOverlay::Providers(providers)
            }
        };
        Ok(overlay)
    }

    pub(in crate::app) fn run_overlay(
        &mut self,
        app: &App,
        agent: Option<&mut Agent<'_>>,
        which: Which,
    ) -> Result<OverlayOutcome> {
        let compute = matches!(which, Which::Compute);
        self.term.set_overlay_mouse(self.ui.mouse);
        let mut overlay = self.make_overlay(app, which)?;
        let Shell {
            term,
            themes,
            keys,
            status,
            catalog,
            scope,
            dark,
            auth_job,
            background_control,
            pty_approval,
            extensions_pending,
            extensions_job,
            live_tasks,
            runtime,
            ..
        } = self;
        // Browsing panels keep resident models loaded; only an explicit compute action releases them.
        let mut release = overlay_compute_release(app, agent);
        let mut env = OverlayEnv {
            app,
            themes,
            keys,
            status,
            catalog,
            scope,
            dark: *dark,
            latest: None,
            next: None,
            verify_lora: None,
            release_compute: release.as_mut().map(|release| release as &mut dyn FnMut()),
            seen_generation: 0,
            auth_job,
            background_control: background_control.as_ref(),
            pty_approval: *pty_approval,
            live_tasks: live_tasks.as_ref(),
            runtime,
            extensions_pending,
            extensions_job,
            compute_monitor: if compute {
                Some(ComputeMonitor::new(app))
            } else {
                None
            },
        };
        env.seen_generation = env.catalog.generation;
        let result = term.with_overlay(|ot| run_overlay(ot, &mut env, &mut overlay));
        env.themes.restore();
        result?;
        Ok(OverlayOutcome {
            latest: env.latest,
            next: env.next,
            verify_lora: env.verify_lora,
        })
    }

    pub(super) fn start_busy_overlay(&mut self, app: &App, which: Which) -> Result<()> {
        if self.busy_overlay.is_some() {
            return Ok(());
        }
        let compute = matches!(which, Which::Compute);
        let overlay = self.make_overlay(app, which)?;
        self.term.set_overlay_mouse(self.ui.mouse);
        self.term.open_overlay()?;
        self.busy_overlay = Some(BusyOverlay {
            overlay,
            paste_guard: Default::default(),
            compute_monitor: compute.then(|| ComputeMonitor::new(app)),
            verify_lora: None,
            latest: None,
            next: None,
            seen_generation: self.catalog.generation,
        });
        Ok(())
    }

    /// A single bounded panel step; never waits for terminal input or agent completion.
    pub(in crate::app) fn step_busy_overlay(
        &mut self,
        app: &App,
        event: Option<Event>,
    ) -> Result<bool> {
        let Some(mut state) = self.busy_overlay.take() else {
            return Ok(false);
        };
        let Shell {
            term,
            themes,
            keys,
            status,
            catalog,
            scope,
            dark,
            auth_job,
            background_control,
            pty_approval,
            extensions_pending,
            extensions_job,
            live_tasks,
            runtime,
            ..
        } = self;
        let mut env = OverlayEnv {
            compute_monitor: state.compute_monitor.take(),
            verify_lora: state.verify_lora.take(),
            latest: state.latest.take(),
            next: state.next.take(),
            seen_generation: state.seen_generation,
            auth_job,
            background_control: background_control.as_ref(),
            pty_approval: *pty_approval,
            live_tasks: live_tasks.as_ref(),
            runtime,
            extensions_pending,
            extensions_job,
            app,
            themes,
            keys,
            status,
            catalog,
            scope,
            dark: *dark,
            release_compute: None,
        };
        refresh_overlay(&mut env, &mut state.overlay);
        let redraw = event.is_none();
        let closed = event.is_some_and(|event| {
            overlay_input(&mut env, &mut state.overlay, &mut state.paste_guard, event)
        });
        let draw_result = if redraw && !closed {
            term.draw_overlay_placed(|frame| draw_overlay_frame(frame, &env, &mut state.overlay))
        } else {
            Ok(())
        };
        state.compute_monitor = env.compute_monitor.take();
        state.verify_lora = env.verify_lora.take();
        state.latest = env.latest.take();
        state.next = env.next.take();
        state.seen_generation = env.seen_generation;
        self.busy_overlay = Some(state);
        draw_result?;
        if closed {
            self.finish_busy_overlay(app)
        } else {
            Ok(false)
        }
    }

    pub(in crate::app) fn finish_busy_overlay(&mut self, app: &App) -> Result<bool> {
        let Some(state) = self.busy_overlay.take() else {
            return Ok(false);
        };
        self.themes.restore();
        self.term.close_overlay()?;
        if let Some(config) = state.latest {
            self.pending_config = Some(config);
            self.note("Settings", "Saved; changes apply after the current turn.");
        }
        if let Some(selection) = state.verify_lora {
            self.pending_verification = Some(selection);
            self.note(
                "LoRA verification",
                "Verification runs after the current turn releases compute.",
            );
        }
        let next_panel = state.next.as_deref().and_then(overlay_destination);
        if let Some(which) = next_panel {
            self.start_busy_overlay(app, which)?;
        } else if let Some(command) = state
            .next
            .as_deref()
            .and_then(|next| next.strip_prefix("command:"))
        {
            return Ok(self.route_busy_command(app, command.to_owned()));
        } else if let Some(payload) = state
            .next
            .as_deref()
            .and_then(|next| next.strip_prefix("subagent-resume:"))
        {
            let id = payload.split('\n').next().unwrap_or_default().to_owned();
            self.pending_resume = Some(payload.to_owned());
            self.note(
                "Subagents",
                "The resume starts after the current turn finishes.",
            );
            self.start_busy_overlay(
                app,
                Which::SubagentViewer {
                    id,
                    entry: None,
                    from_hub: true,
                },
            )?;
        }
        Ok(false)
    }
}
