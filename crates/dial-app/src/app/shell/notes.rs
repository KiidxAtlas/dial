//! Transcript notes, live monitors, extension queues and persisting shell state.

use crate::agent::Agent;
use crate::app::core::App;
use crate::app::display::config_key;
use crate::app::drive::Step;
use crate::app::overlay::{ActiveOverlay, start_streaming_job};
use crate::app::shell::Shell;
use crate::config::settings_store::{Change, Scope};
use crate::config::types::Config;
use crate::storage::store::Store;
use anyhow::Result;

/// The command view stays ahead of the immutable config borrowed by a running agent.
pub(in crate::app) fn settings_config<'a>(
    active: &'a Config,
    pending: &'a Option<Config>,
) -> &'a Config {
    pending.as_ref().unwrap_or(active)
}

/// A write that returns to the active config must discard an earlier pending change.
pub(in crate::app) fn stage_settings(
    active: &Config,
    pending: &mut Option<Config>,
    fresh: Config,
) -> bool {
    let changed = config_key(&fresh) != config_key(active);
    *pending = changed.then_some(fresh);
    changed
}

impl Shell {
    pub(in crate::app) fn note(&mut self, title: &str, text: impl AsRef<str>) {
        self.transcript.notice(title, text.as_ref());
    }

    pub(in crate::app) fn take_runtime_update_report(&mut self) -> Option<String> {
        self.runtime_updates.take_report()
    }

    /// Opens a background job's panel; its progress and result fill it in place.
    pub(crate) fn note_pending(&mut self, title: &str) {
        self.transcript.notice_pending(title);
    }

    /// Runs the live rollback monitor on a background job. Its adapter digest hashes whole weight
    /// files, which must not block the UI thread. The job opens its own store connection; a quiet
    /// monitor produces an empty result, which is not noted.
    pub(super) fn start_live_monitor(&mut self, label: &'static str, config: Config) {
        let started = start_streaming_job(&mut self.auth_job, label, move |_cancel, _progress| {
            let store = Store::open(
                &config.state_dir,
                &config.model,
                &config.use_case,
                &config.harness,
            )?;
            Ok(match crate::learn::improve::monitor_now(&store, &config)? {
                Some(rollback) => format!("{rollback}; it applies from the next turn"),
                None => String::new(),
            })
        });
        if !started {
            self.note(
                label,
                "live monitor skipped: another background operation is still running",
            );
        }
    }

    /// Queue an `extensions:` action from the browser. The main loop starts it, because starting
    /// needs the app's config and layout.
    pub(in crate::app) fn queue_extensions(
        pending: &mut Option<crate::app::commands::extensions_browser::Pending>,
        scope: Scope,
        payload: &str,
        overlay: &mut ActiveOverlay,
    ) {
        let intent =
            match serde_json::from_str::<crate::ui::overlay::extensions::model::Intent>(payload) {
                Ok(intent) => intent,
                Err(error) => {
                    overlay.banner(format!("could not read that action: {error}"), true);
                    return;
                }
            };
        let progress = crate::app::commands::extensions_browser::queue(pending, intent, scope);
        if let ActiveOverlay::Extensions(panel) = overlay {
            panel.attach(progress);
        }
    }

    /// Persist settings writes, reload the effective config and apply it live.
    pub(crate) fn persist(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        changes: &[Change],
        scope: Scope,
    ) -> Result<Step> {
        app.save_settings(scope, changes, None)?;
        let fresh = app.reload()?;
        Ok(self.config_changed(app, agent, fresh))
    }

    /// Settings commands plan against the latest saved snapshot, not the config borrowed by
    /// the running agent. This also lets a later command find entries saved during this turn.
    pub(in crate::app) fn settings_config<'a>(&'a self, app: &'a App) -> &'a Config {
        settings_config(&app.config, &self.pending_config)
    }

    /// [`Shell::persist`] when the agent is at hand (idle). During a turn (`None`) the write lands
    /// now and display settings apply now; the agent adopts the rest when the turn finishes.
    pub(crate) fn save(
        &mut self,
        app: &App,
        agent: Option<&mut Agent<'_>>,
        changes: &[Change],
        scope: Scope,
    ) -> Result<Step> {
        if let Some(agent) = agent {
            return self.persist(app, agent, changes, scope);
        }
        app.save_settings(scope, changes, None)?;
        let fresh = app.reload()?.for_coding();
        self.apply_display(app, &fresh);
        self.adopt_after_turn(app, Step::Rebuild(Box::new(fresh)));
        Ok(Step::Done)
    }

    /// Holds a rebuild that a command asked for during a turn until the turn finishes: a changed
    /// config through `pending_config`, a same-config rebuild (plugins, resources) through
    /// `pending_resource_rebuild`. The idle loop applies both at its next boundary.
    pub(in crate::app) fn adopt_after_turn(&mut self, app: &App, step: Step) {
        let Step::Rebuild(config) = step else {
            return;
        };
        if stage_settings(&app.config, &mut self.pending_config, *config) {
            self.note(
                "Settings",
                "Saved; the agent adopts it when the current turn finishes.",
            );
        } else {
            self.pending_resource_rebuild = true;
            self.note("Settings", "Reloads when the current turn finishes.");
        }
    }
}
