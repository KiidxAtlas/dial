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
