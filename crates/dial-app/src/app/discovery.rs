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
    pub(super) generation: u64,
}

pub(super) struct ProbeLease(pub(in crate::app) std::fs::File);

impl Drop for ProbeLease {
    fn drop(&mut self) {
        // Closing our handle alone leaves flock held by any duplicate inherited during spawn.
        let _ = fs2::FileExt::unlock(&self.0);
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
            generation: 0,
        }
    }

    pub(super) fn get(&self) -> Option<catalog::Discovery> {
        shared(&self.slot).1.clone()
    }

    /// Reap a finished scan so a new request can start; bumps `generation` when one completed.
    pub(super) fn collect(&mut self) {
        if !self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            return;
        }
        self.worker = None;
        self.generation = self.generation.wrapping_add(1);
    }

    pub(super) fn start(&mut self, app: &App) {
        self.collect();
        if self.worker.is_some() || self.get().is_some() {
            return;
        }
        self.scan(app);
    }

    pub(super) fn refresh(&mut self, app: &App) {
        self.collect();
        // Config changes must not opt an idle chat into provider discovery. Once the
        // user has opened a model/provider panel, keep that requested catalog fresh.
        if self.request != 0 {
            self.scan(app);
        }
    }

    fn scan(&mut self, app: &App) {
        let layout = app.layout.clone();
        let configured = app.config.providers.clone();
        let mut selections = vec![app.config.selection()];
        selections.extend(app.config.model_roles.values().cloned());
        self.launch(move || catalog::discover(&layout, &configured, &selections));
    }

    pub(super) fn launch(
        &mut self,
        discover: impl FnOnce() -> catalog::Discovery + Send + 'static,
    ) {
        self.request += 1;
        let request = self.request;
        shared(&self.slot).0 = request;
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
