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
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn explicit_project_wins_over_wandb_variables() {
        let e = env(&[
            ("COREWEAVE_PROJECT", " team/proj "),
            ("WANDB_INFERENCE_PROJECT", "other/inf"),
            ("WANDB_PROJECT", "wb"),
            ("WANDB_ENTITY", "ent"),
        ]);
        assert_eq!(resolve_project(e).as_deref(), Some("team/proj"));
    }

    #[test]
    fn inference_project_is_the_second_choice() {
        let e = env(&[
            ("WANDB_INFERENCE_PROJECT", "other/inf"),
            ("COREWEAVE_PROJECT", "  "),
        ]);
        assert_eq!(resolve_project(e).as_deref(), Some("other/inf"));
    }

    #[test]
    fn wandb_project_with_slash_is_used_verbatim() {
        let e = env(&[("WANDB_PROJECT", "team/proj"), ("WANDB_ENTITY", "ignored")]);
        assert_eq!(resolve_project(e).as_deref(), Some("team/proj"));
    }

    #[test]
    fn wandb_project_is_prefixed_with_entity() {
        let e = env(&[("WANDB_PROJECT", "proj"), ("WANDB_ENTITY", "ent")]);
        assert_eq!(resolve_project(e).as_deref(), Some("ent/proj"));
    }

    #[test]
    fn missing_entity_or_blank_values_yield_no_header() {
        assert_eq!(resolve_project(env(&[("WANDB_PROJECT", "proj")])), None);
        assert_eq!(
            resolve_project(env(&[("WANDB_PROJECT", " "), ("WANDB_ENTITY", "ent")])),
            None
        );
        assert_eq!(resolve_project(env(&[])), None);
    }

    #[test]
    fn endpoint_match_ignores_trailing_slashes_only() {
        assert!(is_endpoint("https://api.inference.wandb.ai/v1"));
        assert!(is_endpoint("https://api.inference.wandb.ai/v1/"));
        assert!(!is_endpoint("https://api.inference.wandb.ai"));
        assert!(!is_endpoint("https://api.inference.wandb.ai/v2"));
        assert!(!is_endpoint("https://example.com/v1"));
    }

    #[test]
    fn header_name_matches_omp() {
        assert_eq!(PROJECT_HEADER, "OpenAI-Project");
    }
}
