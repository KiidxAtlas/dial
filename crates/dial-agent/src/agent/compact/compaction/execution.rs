//! Optional request execution resources; the ordinary callback remains the sequential fallback.
use crate::{
    agent::Ui,
    llm::{Chat, provider::Backend},
};
use anyhow::Result;
use std::sync::{Arc, atomic::AtomicBool};

pub(super) type Completion =
    dyn FnMut(&mut Backend, &Chat, &mut dyn Ui, Arc<AtomicBool>) -> Result<String> + Send;

/// A borrowed completion may capture caller state and need not be transferable to another thread.
pub(super) type RequestCompletion<'a> =
    dyn FnMut(&mut Backend, &Chat, &mut dyn Ui, Arc<AtomicBool>) -> Result<String> + 'a;

/// An independently owned cloud route and guidance loop, never a borrowed resident local model.
pub(in crate::agent) struct SummaryWorker {
    pub backend: Backend,
    pub complete: Box<Completion>,
}

#[derive(Default)]
pub(in crate::agent) struct SummaryExecution {
    /// Soft and Handoff may summarize over the live request prefix (same route, model, tools
    /// and cache namespace), reading the prompt cache instead of re-serializing the history.
    pub live_prefix: bool,
    /// Consumed only for the independent turn-prefix fold, not for dependent incremental windows.
    pub parallel: Option<SummaryWorker>,
}
