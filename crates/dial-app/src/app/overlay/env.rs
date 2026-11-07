//! The environment an overlay reads and writes: settings tables and favorite routes.

use crate::app::core::App;
use crate::app::discovery::Catalog;
use crate::app::overlay::auth::AuthJob;
use crate::app::overlay::compute::ComputeMonitor;
use crate::app::overlay::settings::{
    apply_settings_hints, setting_setup_offers, settings_value_choices,
};
use crate::app::overlay::{ActiveOverlay, start_streaming_job};
use crate::app::selection::change_path;
use crate::config::settings_store::{self, Change, Scope};
use crate::config::types::{Approval, Config};
use crate::llm::model_config::ModelSelection;
use crate::ui::keys::Keymap;
use crate::ui::status::StatusData;
use crate::ui::theme::ThemeManager;
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub(in crate::app) struct OverlayEnv<'a> {
    pub(in crate::app) runtime: &'a crate::runtime::Tree,
    pub(in crate::app) todo: &'a crate::agent::workflow::todo::TodoList,
    pub(in crate::app) model_downloads:
        Option<&'a crate::ui::overlay::models::SharedModelDownloads>,
    pub(in crate::app) compute_monitor: Option<ComputeMonitor>,
    pub(in crate::app) live_tasks: Option<&'a crate::agent::LiveTaskControl>,
    pub(in crate::app) verify_lora: Option<ModelSelection>,
    pub(in crate::app) auth_job: &'a mut Option<AuthJob>,
    pub(in crate::app) last_setup_failure: &'a mut Option<super::auth::SetupFailure>,
    pub(in crate::app) extensions_pending:
        &'a mut Option<crate::app::commands::extensions_browser::Pending>,
    pub(in crate::app) extensions_job:
        &'a mut Option<crate::app::commands::extensions_browser::InstallJob>,
    pub(in crate::app) background_control:
        Option<&'a Arc<dyn crate::tools::integrations::ext::Tool>>,
    pub(in crate::app) pty_approval: Approval,
    pub(in crate::app) app: &'a App,
    pub(in crate::app) themes: &'a mut ThemeManager,
    pub(in crate::app) keys: &'a mut Keymap,
    pub(in crate::app) status: &'a StatusData,
    pub(in crate::app) catalog: &'a mut Catalog,
    pub(in crate::app) scope: &'a mut Scope,
    pub(in crate::app) dark: Option<bool>,
    pub(in crate::app) latest: Option<Config>,
    pub(in crate::app) next: Option<String>,
    /// The shell's existing navigation stack; direct model reviews must retain their editor too.
    pub(in crate::app) history: &'a mut Vec<ActiveOverlay>,
    pub(in crate::app) seen_generation: u64,
    /// Unloads resident models before a compute action starts training. Absent in busy panels,
    /// where the chat loop's compute handoff runs while the panel is open.
    pub(in crate::app) release_compute: Option<&'a mut dyn FnMut()>,
    /// Builds a fresh `/peers` snapshot (absent unless that panel is open with an agent), so the
    /// board follows sessions that start or stop while it is visible.
    pub(in crate::app) peer_source:
        Option<&'a mut dyn FnMut() -> Option<crate::ui::overlay::peers::PeersView>>,
}

pub(in crate::app) fn settings_tables(
    app: &App,
    effective: &Config,
) -> Result<(toml::Table, toml::Table, toml::Table)> {
    Ok((
        effective
            .settings_values()
            .context("serializing effective settings")?,
        settings_store::load_layer(&app.layout, Scope::Global)
            .context("reading global settings")?,
        settings_store::load_layer(&app.layout, Scope::Project)
            .context("reading project settings")?,
    ))
}

/// The App snapshot belongs to the active turn; disk reloads may already contain staged saves.
pub(in crate::app) fn refresh_settings_application(
    settings: &mut crate::ui::overlay::settings::SettingsOverlay,
    app: &App,
    saved: &Config,
    busy: bool,
) {
    if let (Ok(active), Ok(projected)) = (
        app.config.settings_values(),
        saved.clone().for_coding().settings_values(),
    ) {
        settings.set_projected_application_state(&active, Some(&projected), busy);
    }
}

/// Re-enter retained configuration pages with fresh values, not fresh navigation/editor state.
pub(in crate::app) fn refresh_navigation(
    app: &App,
    catalog: &Catalog,
    overlay: &mut ActiveOverlay,
    busy: bool,
) {
    let overlay = overlay.content_mut();
    if !matches!(
        overlay,
        ActiveOverlay::Settings(_) | ActiveOverlay::Models(_) | ActiveOverlay::Providers(_)
    ) {
        return;
    }
    let result = (|| -> Result<()> {
        let config = app.reload()?;
        match overlay {
            ActiveOverlay::Settings(settings) => {
                let (values, global, project) = settings_tables(app, &config)?;
                settings.refresh_values(values, project, global, config.setting_sources.clone());
                refresh_settings_application(settings, app, &config, busy);
                apply_settings_hints(settings, &config);
                settings.set_setup_offers(setting_setup_offers(&app.layout.user_dir, &config));
                settings.set_value_choices(settings_value_choices(
                    &config,
                    catalog.get().as_ref(),
                    &app.layout.user_dir,
                ));
            }
            ActiveOverlay::Models(models) => {
                models.set_roles(config.role_statuses());
                models.set_favorites(favorite_routes(&config));
                models.set_laya_checkpoint(crate::config::laya::expected_checkpoint(&config.laya));
                models.set_memory_required(crate::config::memory_backend::requires_memory_role(
                    &config.memory,
                ));
                models.set_memory_optional(
                    crate::config::memory_backend::auto_retain_uses_memory_role(&config.memory),
                );
                models.set_memory_backend(
                    crate::config::memory_backend::active_backend(&config.memory).as_str(),
                );
            }
            ActiveOverlay::Providers(providers) => providers.set_configured(config.providers),
            _ => {}
        }
        Ok(())
    })();
    if let Err(error) = result {
        overlay.banner(
            format!("Could not refresh settings; showing last good values: {error:#}"),
            true,
        );
    }
}

pub(in crate::app) fn favorite_routes(
    config: &Config,
) -> Vec<crate::ui::overlay::models::FavoriteRoute> {
    config
        .model_favorites
        .iter()
        .map(|selection| {
            let effective = match config.selection_endpoint(selection) {
                Some(endpoint) => ModelSelection {
                    provider: selection.provider.clone(),
                    model: selection.model.clone(),
                    base_url: Some(endpoint.base_url),
                    api_key_env: endpoint.api_key_env,
                    allow_insecure: endpoint.allow_insecure,
                    protocol: endpoint.protocol,
                    headers: endpoint.headers,
                    account_id: endpoint.account_id,
                    context_window: selection.context_window,
                    thinking: selection.thinking.clone(),
                },
                None => selection.clone(),
            };
            crate::ui::overlay::models::FavoriteRoute {
                selection: selection.clone(),
                effective,
            }
        })
        .collect()
}

/// Validate the frozen review and remove all its exact paths in one canonical transaction.
/// The comparison runs under the settings store's layer locks, not before acquiring them.
fn save_reviewed_reset(
    app: &App,
    paths: &[String],
    scope: Scope,
    expected: &BTreeMap<String, toml::Value>,
) -> Result<()> {
    let selected: BTreeSet<&str> = paths.iter().map(String::as_str).collect();
    ensure!(
        !selected.is_empty()
            && selected.len() == paths.len()
            && selected.len() == expected.len()
            && selected.iter().all(|path| expected.contains_key(*path)),
        "Reset paths do not match the reviewed overrides; review again."
    );
    ensure!(
        selected
            .iter()
            .all(|path| crate::ui::overlay::settings::resettable_path(path)),
        "Reset includes a setting that cannot be removed in bulk; review again."
    );
    let changes: Vec<_> = paths
        .iter()
        .map(|path| Change::Remove { path: path.clone() })
        .collect();
    app.save_settings_checked(scope, &changes, |_| {
        let layer = settings_store::load_layer(&app.layout, scope)?;
        for (path, value) in expected {
            ensure!(
                crate::config::schema::get(&layer, path) == Some(value),
                "Reviewed override {path} changed; review again before resetting."
            );
            // Single-key reset clears embedding pairs. Bulk reset must review every stored half,
            // never silently expand a preview to remove an unreviewed partner.
            if let Some(partner) = crate::config::schema::pair_partner(path) {
                ensure!(
                    crate::config::schema::get(&layer, partner).is_none()
                        || expected.contains_key(partner),
                    "Embedding override partner {partner} was not reviewed; review again."
                );
            }
        }
        Ok(())
    })
}

impl OverlayEnv<'_> {
    pub(super) fn start_auth(
        &mut self,
        overlay: &mut ActiveOverlay,
        label: &'static str,
        action: impl FnOnce(Arc<AtomicBool>) -> Result<String> + Send + 'static,
    ) {
        if self.auth_job.is_some() {
            self.report(
                overlay,
                Err(anyhow::anyhow!(
                    "An authentication action is already running"
                )),
            );
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (send, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = send.send(action(worker_cancel));
        });
        *self.auth_job = Some(AuthJob {
            label,
            receiver,
            cancel,
            lines: None,
            model: None,
        });
        if let ActiveOverlay::Providers(providers) = overlay {
            providers.set_chatgpt_pending(label == "ChatGPT");
        }
        self.report(overlay, Ok(format!("Waiting for {label} sign-in in your browser; sign-in continues if you close this panel")));
    }

    pub(super) fn collect_auth(&mut self, overlay: &mut ActiveOverlay) {
        // The event loop collects Smithery sign-ins so it can resume the parked command.
        if self
            .auth_job
            .as_ref()
            .is_some_and(|job| job.label == "Smithery")
        {
            return;
        }
        for (_, line) in AuthJob::drain_lines(self.auth_job) {
            overlay.banner(line, false);
        }
        let result = AuthJob::collect_retained(self.auth_job, self.app, self.last_setup_failure);
        if let Some((provider, result)) = result {
            if let ActiveOverlay::Providers(providers) = overlay {
                providers.set_chatgpt_pending(false);
                providers.set_chatgpt_accounts(
                    crate::llm::chatgpt::accounts(&self.app.layout.user_dir).unwrap_or_default(),
                );
                providers.set_anthropic_signed_in(crate::llm::anthropic_oauth::signed_in(
                    &self.app.layout.user_dir,
                ));
                providers.set_oauth_accounts(crate::llm::oauth::accounts_by_provider(
                    &self.app.layout.user_dir,
                ));
            }
            let result = if matches!(provider, "Model" | "Voice") {
                result.and_then(|message| {
                    let fresh = self.reload_saved()?;
                    // A finished download saved a setting: show it, and offer the model as
                    // downloaded from now on.
                    if let ActiveOverlay::Settings(settings) = &mut *overlay
                        && let Ok((values, global, project)) = settings_tables(self.app, &fresh)
                    {
                        settings.replace_values(values, project, global);
                        settings.set_value_choices(settings_value_choices(
                            &fresh,
                            self.catalog.get().as_ref(),
                            &self.app.layout.user_dir,
                        ));
                        apply_settings_hints(settings, &fresh);
                        settings.set_setup_offers(setting_setup_offers(
                            &self.app.layout.user_dir,
                            &fresh,
                        ));
                    }
                    Ok(message)
                })
            } else {
                result
            };
            // A finished embedding setup while memory is not semantic: offer the switch on close.
            let result = match result {
                Ok(message)
                    if provider == "Setup"
                        && message.starts_with(crate::setup::embeddings::SUMMARY_PREFIX) =>
                {
                    if let Ok(fresh) = self.app.reload() {
                        self.latest = Some(fresh);
                    }
                    Ok(match self.queue_semantic_offer() {
                        Some(note) => format!("{message}\n{note}"),
                        None => message,
                    })
                }
                other => other,
            };
            if provider == "Model"
                && result.is_ok()
                && self.next.is_none()
                && matches!(overlay, ActiveOverlay::Models(_))
                && matches!(self.history.last(), Some(ActiveOverlay::Settings(_)))
            {
                self.next = Some("navigate:back".into());
            }
            self.refresh_roles(overlay);
            self.report(overlay, result);
            self.catalog.refresh(self.app);
        }
    }

    /// After an embedding model is set up or assigned while memory is not semantic, queue the
    /// single-key switch offer for when this panel closes (decision: never switch silently).
    /// Returns the line that tells the user so.
    pub(super) fn queue_semantic_offer(&mut self) -> Option<String> {
        use crate::config::memory_backend::MemoryBackendId;
        let config = self.latest.clone().or_else(|| self.app.reload().ok())?;
        if config.memory.enabled && config.memory.backend == MemoryBackendId::Semantic {
            return None;
        }
        if self.next.is_none() {
            self.next = Some(format!(
                "command:/setup memory {}",
                crate::setup::embeddings::OFFER_SEMANTIC_ARG
            ));
        }
        let current = if config.memory.enabled {
            config.memory.backend.as_str()
        } else {
            "off"
        };
        Some(format!(
            "Only semantic memory uses embeddings and memory is {current}: closing this panel asks whether to switch."
        ))
    }

    /// Persist, then reload the effective config (files plus command-line flags) and apply the UI part live.
    pub(in crate::app) fn save(&mut self, changes: &[Change], scope: Scope) -> Result<Config> {
        self.app.save_settings(scope, changes, None)?;
        self.reload_saved()
    }

    pub(super) fn reload_saved(&mut self) -> Result<Config> {
        let fresh = self.app.reload()?;
        if let Err(error) = self.themes.apply_config(&fresh.ui, self.dark) {
            // The setting was saved and is valid JSON-wise; the theme itself is what failed. Surface it.
            self.latest = Some(fresh.clone());
            return Err(anyhow::anyhow!(
                "saved, but the theme could not load: {error}"
            ));
        }
        self.latest = Some(fresh.clone());
        Ok(fresh)
    }

    fn effective_value(config: &Config, path: &str) -> toml::Value {
        let table = toml::Table::try_from(config).unwrap_or_default();
        crate::config::schema::get(&table, path)
            .cloned()
            .unwrap_or_else(|| toml::Value::String(String::new()))
    }

    /// Starts the dependency install and its settings save as a streaming background job.
    pub(super) fn start_setup(&mut self, overlay: &mut ActiveOverlay, id: String) {
        let layout = self.app.layout.clone();
        let started = start_streaming_job(self.auth_job, "Setup", move |cancel, progress| {
            // `embeddings:<model>` ids pull a local embedding model; every other id is a dependency.
            if let Some(model) = crate::setup::embeddings::model_from_id(&id) {
                return crate::setup::embeddings::setup(&layout, model, &cancel, progress);
            }
            let saved = crate::setup::deps::run::install_and_save(&layout, &id, &cancel, progress)?;
            Ok(format!("Set up and saved:\n{}", saved.join("\n")))
        });
        if let (true, ActiveOverlay::Settings(settings)) = (started, &mut *overlay) {
            settings.set_setup_pending(true);
        }
        if !started {
            self.report(
                overlay,
                Err(anyhow::anyhow!(
                    "Another background operation is still running; try again when it finishes."
                )),
            );
        }
    }

    pub(super) fn unset_many(
        &mut self,
        overlay: &mut ActiveOverlay,
        paths: &[String],
        scope: Scope,
    ) {
        let ActiveOverlay::Settings(settings) = overlay else {
            self.report(
                overlay,
                Err(anyhow::anyhow!("Bulk reset requires a settings review.")),
            );
            return;
        };
        let outcome = (|| -> Result<_> {
            ensure!(
                settings.batch_pending(),
                "Bulk reset has not been confirmed; review again."
            );
            ensure!(
                settings.batch_scope() == Some(scope),
                "Reset scope does not match the review; review again."
            );
            let expected = settings
                .batch_expected()
                .context("Bulk reset requires a settings review.")?;
            ensure!(
                self.auth_job.is_none() && !settings.setup_pending(),
                "A setup or authentication operation is still running; wait or cancel it before resetting."
            );
            save_reviewed_reset(self.app, paths, scope, expected)?;
            let fresh = self.reload_saved()?;
            let (values, global, project) = settings_tables(self.app, &fresh)?;
            Ok((fresh, (values, project, global)))
        })();
        match outcome {
            Ok((fresh, tables)) => {
                *self.scope = scope;
                settings.batch_settled(Ok(tables));
                refresh_settings_application(
                    settings,
                    self.app,
                    &fresh,
                    self.status.phase != crate::ui::status::Phase::Idle,
                );
                apply_settings_hints(settings, &fresh);
                settings.set_setup_offers(setting_setup_offers(&self.app.layout.user_dir, &fresh));
                settings.set_value_choices(settings_value_choices(
                    &fresh,
                    self.catalog.get().as_ref(),
                    &self.app.layout.user_dir,
                ));
            }
            Err(error) => settings.batch_settled(Err(format!("{error:#}"))),
        }
    }

    pub(super) fn setting(
        &mut self,
        overlay: &mut ActiveOverlay,
        path: &str,
        changes: &[Change],
        scope: Scope,
    ) {
        if let ActiveOverlay::ModelSettings(settings) = overlay {
            let mut updates = changes.to_vec();
            if settings.activates_profile() {
                updates.push(Change::Set {
                    path: "model_profile".into(),
                    value: toml::Value::String(settings.name.clone()),
                });
            }
            let result = self
                .save(&updates, scope)
                .map(|_| ())
                .map_err(|e| format!("{e:#}"));
            settings.settled(result);
            return;
        }
        if let ActiveOverlay::ImageSettings(settings) = overlay {
            // Image settings never switch the chat model's profile.
            let result = self
                .save(changes, scope)
                .map(|_| ())
                .map_err(|e| format!("{e:#}"));
            settings.settled(result);
            return;
        }
        let ActiveOverlay::Settings(settings) = overlay else {
            return;
        };
        // Do not let a pending setup overwrite a newer selection made in this panel.
        if self.auth_job.as_ref().is_some_and(|job| {
            job.label == "Voice"
                && job.model.as_ref().is_some_and(|prepared| {
                    prepared
                        .changes
                        .iter()
                        .any(|change| change_path(change) == path)
                })
        }) {
            settings.settled(path, Err("Voice setup is still running; wait for it to finish or close the panel to cancel.".into()));
            return;
        }
        match self
            .save(changes, scope)
            .and_then(|fresh| settings_tables(self.app, &fresh).map(|tables| (fresh, tables)))
        {
            Ok((fresh, (values, global, project))) => {
                // Paired changes (the embedding override) settle every path they wrote.
                for other in changes
                    .iter()
                    .map(change_path)
                    .filter(|other| *other != path)
                {
                    settings.settled(other, Ok(Self::effective_value(&fresh, other)));
                }
                settings.settled(path, Ok(Self::effective_value(&fresh, path)));
                settings.replace_values(values, project, global);
                refresh_settings_application(
                    settings,
                    self.app,
                    &fresh,
                    self.status.phase != crate::ui::status::Phase::Idle,
                );
                apply_settings_hints(settings, &fresh);
                settings.set_setup_offers(setting_setup_offers(&self.app.layout.user_dir, &fresh));
                settings.set_value_choices(settings_value_choices(
                    &fresh,
                    self.catalog.get().as_ref(),
                    &self.app.layout.user_dir,
                ));
            }
            Err(error) => {
                for other in changes
                    .iter()
                    .map(change_path)
                    .filter(|other| *other != path)
                {
                    settings.settled(other, Err(format!("{error:#}")));
                }
                settings.settled(path, Err(format!("{error:#}")));
                settings.banner(format!("{error:#}"), true);
            }
        }
    }

    /// Once a setup job this overlay started has settled, reload settings and re-probe
    /// dependencies so the open rows show the new state.
    pub(super) fn refresh_setup(&mut self, overlay: &mut ActiveOverlay) {
        let ActiveOverlay::Settings(settings) = overlay else {
            return;
        };
        if !settings.setup_pending() || self.auth_job.is_some() {
            return;
        }
        settings.set_setup_pending(false);
        let (config, (values, global, project)) =
            match self.app.reload().and_then(|config| {
                settings_tables(self.app, &config).map(|tables| (config, tables))
            }) {
                Ok(values) => values,
                Err(error) => {
                    settings.banner(format!("{error:#}"), true);
                    return;
                }
            };
        settings.replace_values(values, project, global);
        apply_settings_hints(settings, &config);
        settings.set_setup_offers(setting_setup_offers(&self.app.layout.user_dir, &config));
        settings.set_value_choices(settings_value_choices(
            &config,
            self.catalog.get().as_ref(),
            &self.app.layout.user_dir,
        ));
        self.latest = Some(config);
    }

    pub(in crate::app) fn report(&mut self, overlay: &mut ActiveOverlay, outcome: Result<String>) {
        match outcome {
            Ok(message) => overlay.banner(message, false),
            Err(error) => overlay.banner(format!("{error:#}"), true),
        }
    }

    pub(in crate::app) fn refresh_roles(&mut self, overlay: &mut ActiveOverlay) {
        if let (ActiveOverlay::Models(models), Some(config)) = (overlay, &self.latest) {
            models.set_roles(config.role_statuses());
            models.set_laya_checkpoint(crate::config::laya::expected_checkpoint(&config.laya));
            models.set_memory_required(crate::config::memory_backend::requires_memory_role(
                &config.memory,
            ));
            models.set_memory_optional(
                crate::config::memory_backend::auto_retain_uses_memory_role(&config.memory),
            );
            models.set_memory_backend(
                crate::config::memory_backend::active_backend(&config.memory).as_str(),
            );
            models.set_favorites(favorite_routes(config));
        }
    }
}

#[cfg(test)]
mod tests;
