//! The model-call seam shared by the agentic and legacy commit flows.

use crate::config::types::Config;
use crate::llm::{Chat, Reply};

/// One completion: the model reply plus its provider price, when the backend can price it.
pub struct Completion {
    pub reply: Reply,
    pub cost_usd: Option<f64>,
}

/// Model access for commit generation and the agentic tool loop.
///
/// `complete` runs one chat turn against the model selected by `config`. The production
/// implementation is [`ProviderBackend`]; tests substitute a fake.
pub trait CommitBackend: Send + Sync {
    fn complete(&self, config: &Config, request: &Chat) -> Result<Completion, String>;
}

/// Production backend: a one-shot provider call through the agent backend factory.
pub struct ProviderBackend;

impl CommitBackend for ProviderBackend {
    fn complete(&self, cfg: &Config, request: &Chat) -> Result<Completion, String> {
        let mut backend = crate::agent::make_backend(cfg).map_err(|error| error.to_string())?;
        let result = backend
            .chat(None, request, cfg.worker_timeout_secs, None)
            .map_err(|error| error.to_string());
        // Price must come from the backend that made the call, so read it before releasing.
        let cost_usd = result
            .as_ref()
            .ok()
            .and_then(|reply| backend.reply_cost(reply));
        // One-shot commit calls must not keep a local model server resident.
        backend.release();
        let reply = result?;
        if reply.text.trim().is_empty() {
            return Err("Model returned an empty reply".to_owned());
        }
        Ok(Completion { reply, cost_usd })
    }
}
