//! Background model discovery: the shared catalog and the probe lease that guards it.

use crate::app::core::App;
use crate::llm::catalog;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

// ---------------------------------------------------------------------------
// Discovery: one background scan, cached until the user asks for a refresh.
// ---------------------------------------------------------------------------

pub(super) struct Catalog {
    /// (newest request id, its result). A superseded scan never overwrites a newer one.
    pub(super) slot: Arc<Mutex<(u64, Option<catalog::Discovery>)>>,
    pub(super) request: u64,
    pub(super) worker: Option<JoinHandle<()>>,
    /// Only the newest refresh survives while the active scan finishes.
    pending: Option<Box<dyn FnOnce() -> catalog::Discovery + Send>>,
    pub(super) generation: u64,
}

pub(super) struct ProbeLease(pub(in crate::app) std::fs::File);

impl Drop for ProbeLease {
    fn drop(&mut self) {
        // Closing our handle alone leaves flock held by any duplicate inherited during spawn.
        let _ = std::fs::File::unlock(&self.0);
    }
}

pub(super) fn shared<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Catalog {
    pub(super) fn new() -> Self {
        Self {
            slot: Arc::new(Mutex::new((0, None))),
            request: 0,
            worker: None,
            pending: None,
            generation: 0,
        }
    }

    pub(super) fn get(&self) -> Option<catalog::Discovery> {
        shared(&self.slot).1.clone()
    }

    /// Reap a finished scan and start at most one coalesced refresh.
    pub(super) fn collect(&mut self) {
        if let Some(worker) = self.worker.as_ref() {
            if !worker.is_finished() {
                return;
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            self.generation = self.generation.wrapping_add(1);
        }
        if let Some(discover) = self.pending.take() {
            self.spawn(discover);
        }
    }

    pub(super) fn start(&mut self, app: &App) {
        self.collect();
        if self.worker.is_some() || self.get().is_some() {
            return;
        }
        self.scan(app);
    }

    pub(super) fn refresh(&mut self, app: &App) {
        self.refresh_config(app, &app.config);
    }

    /// Probe saved settings without changing the configuration used by an in-flight agent.
    pub(super) fn refresh_config(&mut self, app: &App, config: &crate::config::types::Config) {
        self.collect();
        // Config changes must not opt an idle chat into provider discovery. Once the
        // user has opened a model/provider panel, keep that requested catalog fresh.
        if self.request != 0 {
            self.scan_config(app, config);
        }
    }

    fn scan(&mut self, app: &App) {
        self.scan_config(app, &app.config);
    }

    fn scan_config(&mut self, app: &App, config: &crate::config::types::Config) {
        let layout = app.layout.clone();
        let configured = config.providers.clone();
        let mut selections = vec![config.selection()];
        selections.extend(config.model_roles.values().cloned());
        self.launch(move || catalog::discover(&layout, &configured, &selections));
    }

    pub(super) fn launch(
        &mut self,
        discover: impl FnOnce() -> catalog::Discovery + Send + 'static,
    ) {
        self.request = self.request.wrapping_add(1);
        shared(&self.slot).0 = self.request;
        self.pending = Some(Box::new(discover));
        self.collect();
    }

    fn spawn(&mut self, discover: Box<dyn FnOnce() -> catalog::Discovery + Send>) {
        let request = self.request;
        let slot = self.slot.clone();
        self.worker = Some(std::thread::spawn(move || {
            let found = discover();
            let mut slot = shared(&slot);
            if slot.0 == request {
                slot.1 = Some(found);
            }
        }));
    }
}
