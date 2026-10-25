//! Remembers endpoints that rejected the reasoning-effort parameter.
//!
//! Some OpenAI-compatible servers answer HTTP 400 when a request carries `reasoning_effort` (or
//! `reasoning.effort`) for a model that does not take it. The request is retried once without the
//! parameter and the endpoint+model is remembered for the life of the process, so later requests
//! omit it up front instead of failing first.
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::LazyLock;

#[derive(Default)]
pub(crate) struct Memory(Mutex<HashSet<(String, String)>>);

impl Memory {
    pub(crate) fn remember(&self, base_url: &str, model: &str) {
        self.0
            .lock()
            .insert((base_url.to_owned(), model.to_owned()));
    }

    pub(crate) fn remembered(&self, base_url: &str, model: &str) -> bool {
        self.0
            .lock()
            .contains(&(base_url.to_owned(), model.to_owned()))
    }
}

static PROCESS: LazyLock<Memory> = LazyLock::new(Memory::default);

/// Endpoints remembered by every backend in this process.
pub(crate) fn process_memory() -> &'static Memory {
    &PROCESS
}

/// Whether `error` is an HTTP 400 whose body says the reasoning/effort parameter is unsupported.
pub(crate) fn rejects_reasoning(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<super::HttpError>()
            .is_some_and(|http| http.status == 400 && names_reasoning_parameter(&http.to_string()))
    })
}

fn names_reasoning_parameter(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("reasoning_effort")
        || text.contains("reasoning.effort")
        || (text.contains("unsupported parameter") && text.contains("reasoning"))
}

/// The profile with every reasoning request parameter removed.
pub(crate) fn without_reasoning(
    mut profile: super::profiles::ModelProfile,
) -> super::profiles::ModelProfile {
    profile.reasoning_effort = None;
    if profile.thinking == Some(false) {
        profile.thinking = None;
    }
    profile
}

#[cfg(test)]
mod tests;
