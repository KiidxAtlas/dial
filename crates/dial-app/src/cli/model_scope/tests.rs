use super::*;
use crate::llm::catalog::ModelMetadata;

fn entry(provider: &str, model: &str) -> ModelEntry {
    let selection: ModelSelection = serde_json::from_value(serde_json::json!({
        "provider": provider,
        "model": model,
    }))
    .unwrap();
    ModelEntry {
        selection,
        label: model.to_owned(),
        source: "test".to_owned(),
        available: true,
        metadata: ModelMetadata::default(),
    }
}

fn catalog() -> Vec<ModelEntry> {
    vec![
        entry("anthropic", "claude-sonnet-4-5"),
        entry("openrouter", "claude-sonnet-4-5"),
        entry("openai", "gpt-5"),
        entry("openai", "gpt-5-mini"),
        entry("ollama", "llama3:8b"),
    ]
}

fn patterns(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

fn run(list: &[&str]) -> Resolved {
    resolve(&patterns(list), &catalog())
}

fn ids(resolved: &Resolved) -> Vec<(String, String, Option<CliThinking>)> {
    resolved
        .entries
        .iter()
        .map(|e| {
            (
                e.selection.provider.clone(),
                e.selection.model.clone(),
                e.thinking,
            )
        })
        .collect()
}

fn row(
    provider: &str,
    model: &str,
    thinking: Option<CliThinking>,
) -> (String, String, Option<CliThinking>) {
    (provider.to_owned(), model.to_owned(), thinking)
}

#[test]
fn bare_id_ties_keep_list_order() {
    let resolved = run(&["claude-sonnet-4-5"]);
    assert_eq!(
        ids(&resolved),
        vec![row("anthropic", "claude-sonnet-4-5", None)]
    );
    assert!(resolved.warnings.is_empty());
}

#[test]
fn provider_qualified_exact_selects_that_provider() {
    let resolved = run(&["openrouter/claude-sonnet-4-5"]);
    assert_eq!(
        ids(&resolved),
        vec![row("openrouter", "claude-sonnet-4-5", None)]
    );
}

#[test]
fn exact_provider_id_does_not_absorb_sibling() {
    let resolved = run(&["openai/gpt-5-mini", "openai/gpt-5"]);
    assert_eq!(
        ids(&resolved),
        vec![
            row("openai", "gpt-5-mini", None),
            row("openai", "gpt-5", None)
        ]
    );
}

#[test]
fn suffix_is_split_and_recorded() {
    let resolved = run(&["openai/gpt-5-mini:high"]);
    assert_eq!(
        ids(&resolved),
        vec![row("openai", "gpt-5-mini", Some(CliThinking::High))]
    );
}

#[test]
fn max_suffix_is_split_for_bare_patterns() {
    let resolved = run(&["claude-sonnet-4-5:max"]);
    assert_eq!(
        ids(&resolved),
        vec![row(
            "anthropic",
            "claude-sonnet-4-5",
            Some(CliThinking::Max)
        )]
    );
}

#[test]
fn auto_suffix_records_no_thinking() {
    let resolved = run(&["openai/gpt-5:auto"]);
    assert_eq!(ids(&resolved), vec![row("openai", "gpt-5", None)]);
    assert!(resolved.warnings.is_empty());
}

#[test]
fn provider_scoped_fuzzy_selects_best_score() {
    let resolved = run(&["openai/gpt5"]);
    assert_eq!(ids(&resolved), vec![row("openai", "gpt-5", None)]);
    assert!(resolved.warnings.is_empty());
}

#[test]
fn glob_with_concrete_level_commits_to_base() {
    let resolved = run(&["gpt-*:high"]);
    assert_eq!(
        ids(&resolved),
        vec![
            row("openai", "gpt-5", Some(CliThinking::High)),
            row("openai", "gpt-5-mini", Some(CliThinking::High)),
        ]
    );
}

#[test]
fn glob_max_suffix_uses_base_when_literal_misses() {
    let resolved = run(&["*claude*:max"]);
    assert_eq!(
        ids(&resolved),
        vec![
            row("anthropic", "claude-sonnet-4-5", Some(CliThinking::Max)),
            row("openrouter", "claude-sonnet-4-5", Some(CliThinking::Max)),
        ]
    );
}

#[test]
fn glob_auto_suffix_records_no_thinking() {
    let resolved = run(&["gpt-*:auto"]);
    assert_eq!(
        ids(&resolved),
        vec![
            row("openai", "gpt-5", None),
            row("openai", "gpt-5-mini", None)
        ]
    );
}

#[test]
fn glob_keeps_colon_inside_literal_id() {
    let resolved = run(&["llama*:8b"]);
    assert_eq!(ids(&resolved), vec![row("ollama", "llama3:8b", None)]);
}

#[test]
fn glob_brace_alternatives_expand() {
    let resolved = run(&["{gpt-5,llama3*}"]);
    assert_eq!(
        ids(&resolved),
        vec![
            row("openai", "gpt-5", None),
            row("ollama", "llama3:8b", None)
        ]
    );
}

#[test]
fn unmatched_pattern_warns_and_continues() {
    let resolved = run(&["nope", "gpt-5"]);
    assert_eq!(ids(&resolved), vec![row("openai", "gpt-5", None)]);
    assert_eq!(
        resolved.warnings,
        vec!["No models match pattern \"nope\"".to_owned()]
    );
}

#[test]
fn unmatched_glob_warns_not_errors() {
    let resolved = run(&["zzz*"]);
    assert!(resolved.entries.is_empty());
    assert_eq!(
        resolved.warnings,
        vec!["No models match pattern \"zzz*\"".to_owned()]
    );
}

#[test]
fn invalid_suffix_warns_and_matches_base() {
    let resolved = run(&["claude-sonnet-4-5:bogus"]);
    assert_eq!(
        ids(&resolved),
        vec![row("anthropic", "claude-sonnet-4-5", None)]
    );
    assert_eq!(
        resolved.warnings,
        vec![
            "Invalid thinking level \"bogus\" in pattern \"claude-sonnet-4-5:bogus\". Using default instead."
                .to_owned()
        ]
    );
}

#[test]
fn duplicate_selections_are_skipped() {
    let resolved = run(&["gpt-5", "openai/gpt-5"]);
    assert_eq!(ids(&resolved), vec![row("openai", "gpt-5", None)]);
}

#[test]
fn glob_class_and_question_mark_match_one_character() {
    let resolved = run(&["gpt-?", "gpt-[5m]*"]);
    assert_eq!(
        ids(&resolved),
        vec![
            row("openai", "gpt-5", None),
            row("openai", "gpt-5-mini", None)
        ]
    );
}
