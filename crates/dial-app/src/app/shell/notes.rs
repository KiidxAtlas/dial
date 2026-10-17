//! Transcript notes, live monitors, extension queues and persisting shell state.

use crate::agent::Agent;
use crate::app::core::App;
use crate::app::drive::Step;
use crate::app::overlay::{ActiveOverlay, start_streaming_job};
use crate::app::shell::Shell;
use crate::config::settings_store::{Change, Scope};
use crate::config::types::Config;
use crate::storage::store::Store;
use anyhow::Result;

impl Shell {
    pub(in crate::app) fn note(&mut self, title: &str, text: impl AsRef<str>) {
        self.transcript.notice(title, text.as_ref());
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
}
