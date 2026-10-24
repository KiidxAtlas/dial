//! The `App` controller: configuration, session, store and the cached status every surface reads.

use crate::agent::Agent;
use crate::config::paths::Layout;
use crate::config::session_models::SessionModels;
use crate::config::settings_store::{self, Change, Scope};
use crate::config::types::Config;
use crate::platform;
use crate::storage::session::Session;
use crate::storage::store::Store;
use anyhow::{Context, Result, ensure};
use std::cell::RefCell;
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

// ---------------------------------------------------------------------------
// Application state. The agent borrows `config` and `store` for its whole lifetime, so changes
// to either are applied between agents (see `drive`).
// ---------------------------------------------------------------------------

pub(super) struct App {
    pub(super) layout: Layout,
    pub(super) config: Config,
    pub(super) overlay: toml::Table,
    /// This session's own model choices: other processes' model writes never replace them.
    pub(super) models: RefCell<Option<SessionModels>>,
    pub(super) store: Option<Store>,
    pub(super) store_error: Option<String>,
    pub(super) interrupt: Arc<AtomicBool>,
    pub(super) compute: RefCell<Option<File>>,
    /// CLI launch overrides applied to every agent built here.
    pub(super) launch: crate::agent::modes::Launch,
    /// `--add-dir` roots added to every agent built here.
    pub(super) add_dirs: Vec<std::path::PathBuf>,
    /// `--plan` / `--prewalk` startup state; the startup arm runs once for the first agent.
    pub(super) mode_flags: crate::cli::mode_flags::ModeFlags,
}

impl App {
    pub(super) fn new(
        layout: Layout,
        config: Config,
        overlay: toml::Table,
        launch: crate::agent::modes::Launch,
        add_dirs: Vec<std::path::PathBuf>,
        mode_flags: crate::cli::mode_flags::ModeFlags,
    ) -> Result<Self> {
        let models = Some(SessionModels::capture(
            &settings_store::load_merged(&layout, &overlay).context("reading session settings")?,
        ));
        let mut app = Self {
            layout,
            config: config.for_coding(),
            overlay,
            models: RefCell::new(models),
            store: None,
            store_error: None,
            interrupt: platform::interrupt::install_sigint(),
            compute: RefCell::new(None),
            launch,
            add_dirs,
            mode_flags,
        };
        app.reopen_store();
        Ok(app)
    }

    pub(super) fn open_store(config: &Config) -> Result<Store> {
        ensure!(config.improve.enabled, "automatic improvement is disabled");
        Store::open(
            &config.state_dir,
            &config.model,
            &config.use_case,
            &config.harness,
        )
    }

    pub(super) fn make_agent(&self, session: Session) -> Result<Agent<'_>> {
        let resumed = session.additional_dirs().to_vec();
        let mut serving = self.config.clone();
        if let Some(store) = &self.store {
            crate::learn::improve::apply_pinned_model(&mut serving, store)?;
        }
        let backend = crate::agent::make_session_backend(&serving, &session)?;
        let mut agent = Agent::interactive(
            &self.config,
            backend,
            self.layout.clone(),
            session,
            None,
            self.store.as_ref(),
            self.interrupt.clone(),
        )?;
        agent.set_launch(self.launch.clone())?;
        agent.set_launch_modes(self.mode_flags.advisor, self.mode_flags.external_thinking);
        let mut dirs = self.add_dirs.clone();
        for dir in resumed {
            if !dir.is_dir() {
                eprintln!(
                    "dial: skipping missing workspace directory {}",
                    dir.display()
                );
            } else if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        if !dirs.is_empty() {
            agent.add_workspace_roots(&dirs)?;
            agent.persist_workspace_roots()?;
        }
        Ok(agent)
    }

    /// Commit the authoritative project at the agent-free rebuild boundary.
    pub(super) fn adopt_project(&mut self, layout: Layout, config: Config) -> bool {
        let changed = self.layout.project_root != layout.project_root;
        self.layout = layout;
        self.adopt(config);
        changed
    }

    /// Adopt a freshly loaded, validated configuration and keep the per-model store in step.
    pub(super) fn adopt(&mut self, config: Config) {
        self.config = config.for_coding();
        if !self.config.requires_compute_lock()
            && let Some(lock) = self.compute.get_mut().take()
        {
            crate::learn::compute::clear_owner(&self.layout.user_dir);
            let _ = fs2::FileExt::unlock(&lock);
        }
        self.reopen_store();
    }

    fn reopen_store(&mut self) {
        // Coding opens the learning store only for "Teach this model"; everything else enters
        // learning through explicit improvement operations.
        self.store = None;
        self.store_error = None;
        if self.config.teaching() {
            match Self::open_store(&self.config) {
                Ok(store) => self.store = Some(store),
                Err(error) => self.store_error = Some(format!("{error:#}")),
            }
        }
    }

    /// The effective configuration as the files and the command-line flags define it right now,
    /// keeping this session's own model choices.
    pub(crate) fn reload(&self) -> Result<Config> {
        settings_store::load_session_config(
            &self.layout,
            &self.overlay,
            self.models.borrow().as_ref(),
        )
    }

    /// Persist a settings write made in this session. Model keys it writes (or the model `role`
    /// it assigns) become this session's model choice; other sessions keep their own.
    pub(crate) fn save_settings(
        &self,
        scope: Scope,
        changes: &[Change],
        role: Option<&str>,
    ) -> Result<()> {
        match role {
            Some(role) => settings_store::save_model_config(&self.layout, scope, changes, role)?,
            None => settings_store::save_config(&self.layout, scope, changes)?,
        };
        self.adopt_settings(changes, role)
    }

    pub(crate) fn save_settings_checked(
        &self,
        scope: Scope,
        changes: &[Change],
        check: impl Fn(&Config) -> Result<()>,
    ) -> Result<()> {
        settings_store::save_config_checked(&self.layout, scope, changes, &self.overlay, check)?;
        self.adopt_settings(changes, None)
    }

    fn adopt_settings(&self, changes: &[Change], role: Option<&str>) -> Result<()> {
        if let Some(models) = self.models.borrow_mut().as_mut() {
            let merged = settings_store::load_merged(&self.layout, &self.overlay)
                .context("settings saved, but this session could not re-read them")?;
            models.adopt_paths(
                &merged,
                changes.iter().map(|change| match change {
                    Change::Set { path, .. }
                    | Change::Insert { path, .. }
                    | Change::Remove { path } => path.as_str(),
                }),
            );
            if let Some(role) = role {
                models.adopt_role(&merged, role);
            }
        }
        Ok(())
    }

    /// The directory layout this session runs in.
    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
    }

    pub(super) fn release_idle_compute(&self, agent: &mut Agent<'_>) -> bool {
        if !agent.release_idle_models() {
            return false;
        }
        // Stop model processes before allowing the training worker to load its weights.
        if let Some(lock) = self.compute.borrow_mut().take() {
            crate::learn::compute::clear_owner(&self.layout.user_dir);
            // Closing only our descriptor can leave the lock alive in a duplicate inherited
            // during a concurrent process spawn. Explicitly release the shared lock first.
            let _ = fs2::FileExt::unlock(&lock);
        }
        true
    }
}
