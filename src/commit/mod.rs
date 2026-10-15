//! Conventional commit message generation.
//!
//! Entry point: [`generate`]. It takes the staged diff, stat, and numstat plus repository
//! context, and returns a validated conventional commit.

pub(crate) mod agentic;
pub(crate) mod ai_stage;
pub(crate) mod cache;
pub(crate) mod changelog;
pub(crate) mod cli;
mod commit_types;
mod config;
pub(crate) mod diff;
mod execute;
mod inference;
mod map_reduce;
mod markdown;
mod model_selection;
mod normalization;
mod prompts;
pub(crate) mod repo_context;
pub(crate) mod run;
mod scope;
mod stage;
mod text;
mod types;
mod validation;
pub(crate) use commit_types::format_conventional_commit;

use crate::config::types::Config;

pub(crate) use config::GenerationConfig;
pub(crate) use stage::{GenerationInput, GenerationResult};

/// Generate a conventional commit for the captured Git inputs using the configured model roles.
pub(crate) fn generate(
    config: &Config,
    generation: &GenerationConfig,
    input: GenerationInput<'_>,
) -> Result<GenerationResult, String> {
    generate_with_backend(&run::ProviderBackend, config, generation, input)
}

/// Like [`generate`], with model completions routed through `backend`.
pub(crate) fn generate_with_backend(
    backend: &dyn run::CommitBackend,
    config: &Config,
    generation: &GenerationConfig,
    input: GenerationInput<'_>,
) -> Result<GenerationResult, String> {
    let inference = inference::Inference::with_backend(backend, config, generation);
    stage::generate_conventional_commit(&inference, generation, input)
}
