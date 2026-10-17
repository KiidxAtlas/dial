//! CoreWeave Serverless Inference project routing.
//!
//! CoreWeave authenticates with a plain API key, but every request must also carry an
//! `OpenAI-Project` header naming the `<team>/<project>` pair. It is derived from the environment;
//! the resolution below determines which project a request bills against.

/// Header CoreWeave reads the Serverless Inference project from.
pub const PROJECT_HEADER: &str = "OpenAI-Project";

/// Serverless Inference endpoint; the project header is only attached to requests for it.
pub const BASE_URL: &str = "https://api.inference.wandb.ai/v1";

fn clean(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// Resolve `<team>/<project>` from the environment lookup `env`.
///
/// Precedence: `COREWEAVE_PROJECT`, then `WANDB_INFERENCE_PROJECT`, then `WANDB_PROJECT`
/// (used verbatim when it already contains `/`, otherwise prefixed with `WANDB_ENTITY`).
pub fn resolve_project(env: impl Fn(&str) -> Option<String>) -> Option<String> {
    if let Some(project) =
        clean(env("COREWEAVE_PROJECT")).or_else(|| clean(env("WANDB_INFERENCE_PROJECT")))
    {
        return Some(project);
    }
    let project = clean(env("WANDB_PROJECT"))?;
    if project.contains('/') {
        return Some(project);
    }
    let entity = clean(env("WANDB_ENTITY"))?;
    Some(format!("{entity}/{project}"))
}

/// The process environment's project header value, if one is configured.
pub fn project_from_process_env() -> Option<String> {
    resolve_project(|key| std::env::var(key).ok())
}

/// Whether `base_url` is CoreWeave's Serverless Inference endpoint (trailing slashes ignored).
pub fn is_endpoint(base_url: &str) -> bool {
    base_url.trim_end_matches('/') == BASE_URL
}

#[cfg(test)]
mod tests;
