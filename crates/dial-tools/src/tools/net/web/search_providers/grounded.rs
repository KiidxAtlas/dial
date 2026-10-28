//! Shared pieces of the grounded providers: a model with its own web search tool, asked the question
//! directly, whose answer carries the sources it cited. None of these APIs takes a date filter, so the
//! recency window travels in the prompt.

use super::super::search::SearchResult;
use super::Query;
use std::path::Path;

/// Each grounded provider's key variable and the origin dial's saved credential for it is keyed by.
const KEYS: [(&str, &str); 4] = [
    ("OPENAI_API_KEY", "https://api.openai.com/v1"),
    ("ANTHROPIC_API_KEY", "https://api.anthropic.com"),
    (
        "GEMINI_API_KEY",
        "https://generativelanguage.googleapis.com",
    ),
    ("OPENROUTER_API_KEY", "https://openrouter.ai/api/v1"),
];

/// The saved credential for a grounded provider's key variable, through dial's catalog owner (the same
/// precedence inference uses). Only the variables in [`KEYS`] are looked up; any other name gives `None`.
/// A credential that cannot be read gives `None` too, so the provider reports its variable as unset.
pub fn saved_key(user_dir: &Path, variable: &str) -> Option<String> {
    let (_, base_url) = KEYS.iter().find(|(name, _)| *name == variable)?;
    crate::llm::catalog::resolve_credential(user_dir, base_url, Some(variable))
        .ok()
        .flatten()
}

/// The question as the model sees it: the query, a citation cap, and the recency window as an
/// instruction.
pub(super) fn prompt(query: &Query) -> String {
    let window = query
        .recency
        .map(|recency| format!(" Prefer sources from the past {}.", recency.name()))
        .unwrap_or_default();
    format!(
        "Search the web for: {}\nAnswer concisely and cite at most {} sources.{window}",
        query.text,
        query.limit.clamp(1, 20)
    )
}

/// Append a source with an http(s) URL that is not already listed.
pub(super) fn add_source(results: &mut Vec<SearchResult>, found: Option<SearchResult>) {
    if let Some(found) = found
        && results.iter().all(|seen| seen.url != found.url)
    {
        results.push(found);
    }
}

#[cfg(test)]
mod tests;
