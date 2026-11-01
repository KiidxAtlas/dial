//! Optional request execution resources; the ordinary callback remains the sequential fallback.
use crate::{
    agent::Ui,
    llm::{Chat, provider::Backend},
};
use anyhow::Result;
use std::sync::{Arc, atomic::AtomicBool};

pub(super) type Completion =
    dyn FnMut(&mut Backend, &Chat, &mut dyn Ui, Arc<AtomicBool>) -> Result<String> + Send;

/// An independently owned cloud route and guidance loop, never a borrowed resident local model.
pub(in crate::agent) struct SummaryWorker {
    pub backend: Backend,
    pub complete: Box<Completion>,
}

#[derive(Default)]
pub(in crate::agent) struct SummaryExecution {
    pub live_handoff: bool,
    /// Consumed only for the independent turn-prefix fold, not for dependent incremental windows.
    pub parallel: Option<SummaryWorker>,
}
