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
    settings_value_choices,
};
use crate::app::overlay::{ActiveOverlay, OverlayOutcome, Which, overlay_destination};
use crate::app::selection::{
    local_suggestion_base, model_settings_inherited_edit_mode, model_settings_profile,
};
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
            if let Some(action) = outcome.next.as_deref() {
                let command = if let Some(id) = action.strip_prefix("history-open:") {
                    Some(format!("/search open {id}"))
                } else if action == "history-return" {
                    Some("/search back".to_owned())
                } else {
                    None
                };
                if let Some(command) = command {
                    return Ok(match self.command(app, agent, &command)? {
                        Step::Done => carried,
                        step => step,
                    });
                }
            }
            // `/peers` panel requests run against the agent, then the panel reopens fresh.
            if let Some((action, selected)) = outcome
                .next
                .as_deref()
                .and_then(crate::ui::overlay::peers::PeerAction::parse)
            {
                match crate::app::commands::peers::act(agent, action, selected) {
                    Ok(panel) => {
                        which = panel;
                        continue;
                    }
                    Err(error) => {
                        self.note("Project peers", error);
                        return Ok(carried);
                    }
                }
            }
            if let Some(id) = outcome
                .next
                .as_deref()
                .and_then(|next| next.strip_prefix("test:"))
            {
                // The catalog scan probes each provider's endpoint; the reopened page shows the result.
                self.catalog.refresh_config(app, &app.reload()?);
                self.note(
                    "Metadata connection check",
                    format!("Checking {id}. Cached endpoint metadata appears in Providers; this does not verify a model response."),
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
        self.apply_display(app, &fresh);
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

    /// The settings the shell itself draws with (UI, composer, theme); safe during a turn.
    pub(in crate::app) fn apply_display(&mut self, app: &App, fresh: &Config) {
        self.ui = fresh.ui.clone();
        self.composer.set_history_limit(fresh.ui.history_limit);
        self.composer.set_word_completion(fresh.ui.word_completion);
        self.composer.set_busy_send(fresh.ui.busy_send);
        let themes = ThemeManager::new(&app.layout.user_dir, &fresh.ui, self.dark);
        if let Some(error) = themes.last_error() {
            self.note("Theme", format!("{error}; keeping last good theme"));
        } else {
            self.themes = themes;
        }
    }

    fn make_overlay(&mut self, app: &App, which: Which) -> Result<ActiveOverlay> {
        use crate::app::overlay::activity::{
            ActivityOverlay, ActivityTab, queue_choices, schedule_choices,
        };
        let activity_tab = match &which {
            Which::Subagents | Which::SubagentsActivity => Some(ActivityTab::Workers),
            Which::Jobs => Some(ActivityTab::Processes),
            Which::Compute => Some(ActivityTab::Compute),
            Which::Activity(tab) => Some(*tab),
            _ => None,
        };
        let which = match which {
            Which::Activity(ActivityTab::Downloads) => Which::ModelDownloads,
            other => other,
        };
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
        let settings_path = match &which {
            Which::SettingsAt(path) => Some(path.clone()),
            _ => None,
        };
        let selected_model = match &which {
            Which::SelectedModelSettings(selection) => Some(effective.with_selection(selection)),
            _ => None,
        };
        let context_defaults = matches!(which, Which::ContextDefaults);
        let learning_models = matches!(which, Which::LearningModels);
        let overlay = match which {
            Which::Activity(tab) => {
                let spec = match tab {
                    ActivityTab::Queue => queue_choices(&self.composer.queued_messages()),
                    ActivityTab::Schedule => schedule_choices(),
                    ActivityTab::Attention => {
                        crate::app::overlay::activity::attention_choices(&Default::default(), &[])
                    }
                    _ => return self.make_overlay(app, tab.destination()),
                };
                ActiveOverlay::Palette(PaletteOverlay::choice(spec))
            }
            Which::Choice(spec) => ActiveOverlay::Palette(PaletteOverlay::choice(*spec)),
            Which::HistorySearch(panel) => ActiveOverlay::HistorySearch(Box::new(panel)),
            Which::BackupModels => {
                let mut panel =
                    crate::app::overlay::backup::BackupOverlay::new(&effective, self.scope);
                let project = crate::config::settings_store::load_layer(
                    &app.layout,
                    crate::config::settings_store::Scope::Project,
                )?;
                panel.set_project_layer(project);
                if let Some(discovery) = discovery.as_ref() {
                    panel.set_choices(discovery);
                }
                ActiveOverlay::BackupModels(Box::new(panel))
            }
            Which::Controls => {
                ActiveOverlay::Palette(PaletteOverlay::choice(self.controls_picker(&effective)))
            }
            Which::Palette => ActiveOverlay::Palette(
                PaletteOverlay::new(
                    palette_entries(&self.commands, &self.skills),
                    self.status.phase != Phase::Idle,
                )
                .with_command_context(self.command_context())
                .with_unified_search(super::help::palette_topics(), {
                    let layout = app.layout.clone();
                    let current = self.session_id.clone();
                    move || {
                        crate::app::command_registry::palette_sessions(&layout, &current)
                            .map_err(|error| format!("{error:#}"))
                    }
                }),
            ),
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
            } => ActiveOverlay::SubagentViewer(Box::new({
                // Opening a finished worker answers its pinned attention row.
                self.workers.dismiss(&id);
                crate::ui::overlay::subagent_viewer::SubagentViewer::new(
                    id,
                    self.session_id.clone(),
                    entry,
                    from_hub,
                    self.ui.clone(),
                    std::time::Instant::now(),
                )
                .with_editors(self.worker_editors.clone())
            })),
            Which::Extensions => ActiveOverlay::Extensions(Box::new(
                crate::app::commands::extensions_browser::overlay(
                    app.layout(),
                    &effective,
                    self.extensions_job.as_ref(),
                ),
            )),
            Which::ExtensionsTab(tab) => {
                let mut panel = crate::app::commands::extensions_browser::overlay(
                    app.layout(),
                    &effective,
                    self.extensions_job.as_ref(),
                );
                panel.browser.select_tab(tab);
                ActiveOverlay::Extensions(Box::new(panel))
            }
            Which::Stats(range) => ActiveOverlay::Stats(Box::new(
                crate::app::commands::session::stats_overlay(app, &range)?,
            )),
            Which::Shortcuts => ActiveOverlay::Shortcuts(ShortcutOverlay::new()),
            Which::Peers(panel) => {
                let panel = *panel;
                let mut overlay = crate::ui::overlay::peers::PeersOverlay::new(panel.view);
                if let Some(id) = &panel.selected {
                    overlay.select(id);
                }
                if let Some((message, is_error)) = panel.banner {
                    overlay.banner(message, is_error);
                }
                ActiveOverlay::Peers(Box::new(overlay))
            }
            Which::Jobs => {
                ActiveOverlay::Jobs(Box::new(crate::ui::overlay::jobs::JobsOverlay::new()))
            }
            Which::Git { root, revision } => {
                use crate::ui::overlay::git::{GitOverlay, GitTab};
                let tab = GitTab::from_args(&revision);
                let pinned = revision.as_str();
                let mut panel = GitOverlay::open(
                    crate::git_panel::Repo::new(&root, &effective),
                    (tab.is_none() && !pinned.is_empty()).then_some(pinned),
                )
                .map_err(anyhow::Error::msg)?;
                if let Some(tab) = tab {
                    panel.select_tab(tab);
                }
                ActiveOverlay::Git(Box::new(panel))
            }
            Which::ModelSettings | Which::SelectedModelSettings(_) => {
                let target = selected_model.as_ref().unwrap_or(&effective);
                let (name, profile) = model_settings_profile(target);
                // Resolve the selected route's raw admitted harness, not the current agent's
                // effective version: that version already contains the profile pin being reset.
                let stored = if target.teaching() && selected_model.is_none() {
                    let store = app.store.as_ref().filter(|_| {
                        effective.state_dir == app.config.state_dir
                            && effective.model == app.config.model
                            && effective.use_case == app.config.use_case
                    });
                    Some(match store {
                        Some(store) => store.active()?.harness,
                        None => {
                            App::open_store(&effective.clone().for_coding())?
                                .active()?
                                .harness
                        }
                    })
                } else {
                    None
                };
                let inherited_edit_mode = model_settings_inherited_edit_mode(
                    target,
                    &name,
                    stored.as_ref().unwrap_or(&target.harness),
                )?;
                let mut overlay =
                    crate::ui::overlay::model_settings::ModelSettingsOverlay::new(name, profile)
                        .with_scope(self.scope)
                        .with_project_name(
                            &app.layout
                                .project_root
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy(),
                        )
                        .with_inherited_edit_mode(inherited_edit_mode)
                        .with_model_files();
                if selected_model.is_some() {
                    overlay = overlay.for_selected_model();
                }
                if target.assistance.local.suggestions {
                    overlay =
                        overlay.with_suggestions(&target.user_dir, local_suggestion_base(target));
                }
                ActiveOverlay::ModelSettings(Box::new(overlay))
            }
            Which::ImageSettings => {
                let route = crate::tools::net::image_gen::current_route(&effective)
                    .map_err(|error| anyhow::anyhow!("No image model to configure: {error:#}"))?;
                let (key, settings) = effective.image.entry_for(&route.model);
                ActiveOverlay::ImageSettings(Box::new(
                    crate::ui::overlay::image_settings::ImageSettingsOverlay::new(
                        key,
                        route.api,
                        settings,
                        route.settings,
                        &crate::config::image::installed_loras(),
                    ),
                ))
            }
            Which::Settings
            | Which::LearningSettings
            | Which::ContextDefaults
            | Which::SettingsAt(_) => {
                let (values, global, project) = settings_tables(app, &effective)?;
                let themes = self
                    .themes
                    .names()
                    .into_iter()
                    .filter(|e| e.error.is_none())
                    .map(|e| e.name)
                    .collect();
                let overlay = SettingsOverlay::new(values, project, global, self.scope, themes)
                    .with_sources(effective.setting_sources.clone())
                    .with_project_name(
                        &app.layout
                            .project_root
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy(),
                    );
                let mut overlay = if learning_settings {
                    overlay.learning_only()
                } else {
                    overlay.coding_only()
                };
                if context_defaults {
                    overlay = overlay.context_defaults();
                }
                if let Some(path) = settings_path.as_deref() {
                    overlay.focus_path(path);
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
                // Model, provider and profile rows offer choices; path rows browse from the project.
                overlay.set_value_choices(settings_value_choices(
                    &effective,
                    discovery.as_ref(),
                    &app.layout.user_dir,
                ));
                overlay.set_base_dir(app.layout.project_root.clone());
                crate::app::overlay::env::refresh_settings_application(
                    &mut overlay,
                    app,
                    &effective,
                    self.status.phase != Phase::Idle,
                );
                ActiveOverlay::Settings(Box::new(overlay))
            }
            Which::Models
            | Which::ModelRoles
            | Which::ModelRole(_)
            | Which::LearningModels
            | Which::ModelDownloads
            | Which::ModelMemory => {
                let downloads = self
                    .model_downloads
                    .get_or_insert_with(|| {
                        crate::ui::overlay::models::ModelDownloads::open(
                            app.layout(),
                            crate::stats::local_models::Settings::from_config(&effective),
                            effective.python.clone(),
                        )
                        .into_shared()
                    })
                    .clone();
                let mut models =
                    ModelOverlay::new(discovery, effective.role_statuses(), self.scope)
                        .with_project_name(
                            &app.layout
                                .project_root
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy(),
                        )
                        .with_laya_checkpoint(crate::config::laya::expected_checkpoint(
                            &effective.laya,
                        ))
                        .with_favorites(favorite_routes(&effective))
                        .with_memory_required(crate::config::memory_backend::requires_memory_role(
                            &effective.memory,
                        ))
                        .with_main_lora(self.status.lora)
                        .with_learning(learning_models)
                        .with_usage(&app.layout.user_dir)
                        .with_shared_downloads(downloads);
                models.set_memory_optional(
                    crate::config::memory_backend::auto_retain_uses_memory_role(&effective.memory),
                );
                models.set_memory_backend(
                    crate::config::memory_backend::active_backend(&effective.memory).as_str(),
                );
                match which {
                    Which::ModelRoles => models.show_roles(),
                    Which::ModelRole(role) => {
                        models.show_role(&role);
                    }
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
                    ProviderOverlay::new(discovery, effective.providers, has_key, self.scope)
                        .with_project_name(
                            &app.layout
                                .project_root
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy(),
                        );
                providers.set_chatgpt_accounts(
                    crate::llm::chatgpt::accounts(&app.layout.user_dir).unwrap_or_default(),
                );
                providers.set_anthropic_signed_in(crate::llm::anthropic_oauth::signed_in(
                    &app.layout.user_dir,
                ));
                providers.set_oauth_accounts(crate::llm::oauth::accounts_by_provider(
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
        let overlay = if let Some(tab) = activity_tab {
            let mut activity = ActivityOverlay::new(tab, overlay).with_session(&self.session_id);
            if tab == ActivityTab::Schedule {
                activity.load_schedule(app.layout.clone());
            }
            ActiveOverlay::Activity(Box::new(activity))
        } else {
            overlay
        };
        Ok(overlay)
    }

    pub(in crate::app) fn run_overlay(
        &mut self,
        app: &App,
        mut agent: Option<&mut Agent<'_>>,
        which: Which,
    ) -> Result<OverlayOutcome> {
        let mut overlay = self.make_overlay(app, which)?;
        let mut history = Vec::new();
        let mut latest = None;
        loop {
            let mut outcome =
                self.run_overlay_panel(app, agent.as_deref_mut(), &mut overlay, &mut history)?;
            if outcome.latest.is_some() {
                latest = outcome.latest.take();
            }
            if outcome.next.as_deref() == Some("navigate:back")
                || (outcome.next.is_none() && !history.is_empty())
            {
                if let Some(previous) = history.pop() {
                    overlay = previous;
                    continue;
                }
                outcome.next = None;
            } else if let Some(destination) = outcome.next.as_deref().and_then(overlay_destination)
            {
                // Editors with drop-flushed drafts must settle before constructing a sibling.
                let retain = overlay.retains_navigation();
                if retain {
                    // Keep navigation memory bounded even when a panel links back to itself.
                    if history.len() == 16 {
                        history.remove(0);
                    }
                    history.push(overlay);
                } else {
                    drop(overlay);
                }
                overlay = self.make_overlay(app, destination)?;
                continue;
            }
            outcome.latest = latest;
            return Ok(outcome);
        }
    }

    fn run_overlay_panel(
        &mut self,
        app: &App,
        agent: Option<&mut Agent<'_>>,
        overlay: &mut ActiveOverlay,
        history: &mut Vec<ActiveOverlay>,
    ) -> Result<OverlayOutcome> {
        let compute = matches!(overlay.content_mut(), ActiveOverlay::Compute(_));
        let first_run = agent.is_none();
        let peers = matches!(overlay.content_mut(), ActiveOverlay::Peers(_));
        self.term.set_overlay_mouse(self.ui.mouse);
        let Shell {
            term,
            themes,
            keys,
            status,
            catalog,
            scope,
            dark,
            auth_job,
            last_setup_failure,
            background_control,
            pty_approval,
            extensions_pending,
            extensions_job,
            live_tasks,
            runtime,
            todo,
            model_downloads,
            ..
        } = self;
        // Browsing panels keep resident models loaded; only an explicit compute action releases them.
        // The peers board and the compute release both need the agent, and no panel uses both.
        let (release_agent, peer_agent) = if peers { (None, agent) } else { (agent, None) };
        let mut release = overlay_compute_release(app, release_agent);
        let mut peer_source =
            peer_agent.map(|agent| move || crate::app::commands::peers::snapshot(agent));
        let mut env = OverlayEnv {
            app,
            history,
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
            peer_source: peer_source.as_mut().map(|source| {
                source as &mut dyn FnMut() -> Option<crate::ui::overlay::peers::PeersView>
            }),
            seen_generation: 0,
            auth_job,
            last_setup_failure,
            background_control: background_control.as_ref(),
            pty_approval: *pty_approval,
            live_tasks: live_tasks.as_ref(),
            runtime,
            todo,
            model_downloads: model_downloads.as_ref(),
            extensions_pending,
            extensions_job,
            compute_monitor: if compute {
                Some(ComputeMonitor::new(app))
            } else {
                None
            },
            first_run,
        };
        env.seen_generation = env.catalog.generation;
        crate::app::overlay::env::refresh_navigation(
            app,
            env.catalog,
            overlay,
            env.status.phase != Phase::Idle,
        );
        let result = term.with_overlay(|ot| run_overlay(ot, &mut env, overlay));
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
            history: Vec::new(),
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
            last_setup_failure,
            background_control,
            pty_approval,
            extensions_pending,
            extensions_job,
            live_tasks,
            runtime,
            todo,
            model_downloads,
            ..
        } = self;
        let mut env = OverlayEnv {
            compute_monitor: state.compute_monitor.take(),
            first_run: false,
            verify_lora: state.verify_lora.take(),
            latest: state.latest.take(),
            next: state.next.take(),
            seen_generation: state.seen_generation,
            auth_job,
            last_setup_failure,
            background_control: background_control.as_ref(),
            pty_approval: *pty_approval,
            live_tasks: live_tasks.as_ref(),
            runtime,
            todo,
            model_downloads: model_downloads.as_ref(),
            extensions_pending,
            extensions_job,
            app,
            history: &mut state.history,
            themes,
            keys,
            status,
            catalog,
            scope,
            dark: *dark,
            release_compute: None,
            peer_source: None,
        };
        refresh_overlay(&mut env, &mut state.overlay);
        let redraw = event.is_none();
        let closed = env.next.is_some()
            || event.is_some_and(|event| {
                overlay_input(&mut env, &mut state.overlay, &mut state.paste_guard, event)
            });
        let draw_result = if redraw && !closed {
            // An inline prompt may have temporarily yielded the screen while retaining this menu.
            term.open_overlay().and_then(|()| {
                term.draw_overlay_placed(|frame| {
                    draw_overlay_frame(frame, &env, &mut state.overlay)
                })
            })
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
        let Some(mut state) = self.busy_overlay.take() else {
            return Ok(false);
        };
        self.themes.restore();
        self.term.close_overlay()?;
        let next_panel = state.next.as_deref().and_then(overlay_destination);
        let returning = state.next.as_deref() == Some("navigate:back")
            || (state.next.is_none() && !state.history.is_empty());
        let previous = returning.then(|| state.history.pop()).flatten();
        if next_panel.is_some() && state.overlay.retains_navigation() {
            if state.history.len() == 16 {
                state.history.remove(0);
            }
            state.history.push(state.overlay);
        } else {
            // Flush a worker draft before constructing its sibling or reopened view.
            drop(state.overlay);
        }
        if let Some(config) = state.latest {
            self.apply_display(app, &config);
            self.adopt_after_turn(app, Step::Rebuild(Box::new(config.for_coding())));
        }
        if let Some(selection) = state.verify_lora {
            self.pending_verification = Some(selection);
            self.note(
                "LoRA verification",
                "Verification runs after the current turn releases compute.",
            );
        }
        if let Some(mut overlay) = previous {
            crate::app::overlay::env::refresh_navigation(
                app,
                &self.catalog,
                &mut overlay,
                self.status.phase != Phase::Idle,
            );
            self.term.open_overlay()?;
            self.busy_overlay = Some(BusyOverlay {
                overlay,
                history: state.history,
                paste_guard: Default::default(),
                compute_monitor: None,
                verify_lora: None,
                latest: None,
                next: None,
                seen_generation: 0,
            });
        } else if let Some(which) = next_panel {
            self.start_busy_overlay(app, which)?;
            if let Some(next) = self.busy_overlay.as_mut() {
                next.history = state.history;
            }
        } else if let Some(command) = state
            .next
            .as_deref()
            .and_then(|next| next.strip_prefix("command:"))
        {
            return Ok(self.route_busy_command(app, command.to_owned()));
        } else if let Some(id) = state
            .next
            .as_deref()
            .and_then(|next| next.strip_prefix("test:"))
        {
            self.catalog.refresh_config(app, &app.reload()?);
            self.note(
                "Metadata connection check",
                format!("Checking {id}. Cached endpoint metadata appears in Providers; this does not verify a model response."),
            );
            self.start_busy_overlay(app, Which::Providers)?;
        } else if let Some(payload) = state
            .next
            .as_deref()
            .and_then(|next| next.strip_prefix("subagent-resume:"))
        {
            let id = payload.split('\n').next().unwrap_or_default().to_owned();
            self.pending_resume = Some(payload.to_owned());
            self.note(
                "Workers",
                "The worker continues after the current turn finishes.",
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

#[cfg(test)]
mod tests;
