//! `public`: searches the keyless engines (Startpage, DuckDuckGo, Ecosia, Mojeek) and merges their
//! results. A result several engines return ranks first, then the best position any engine gave it.
//! It is explicit only: `auto` never selects it, because it contacts four engines per search.

use super::super::fetch::Client;
use super::super::search::SearchResult;
use super::{Env, Parsed, Provider, Query, attempt};
use std::collections::{HashMap, HashSet};

const ENGINES: [Provider; 4] = [
    Provider::Startpage,
    Provider::DuckDuckGo,
    Provider::Ecosia,
    Provider::Mojeek,
];

/// Runs each engine in turn and merges the replies. It fails only when every engine fails, and the
/// error names each engine's failure.
pub fn search(
    client: &Client,
    query: &Query,
    env: Env,
    user_dir: &std::path::Path,
) -> Result<Parsed, String> {
    let mut replies = Vec::new();
    let mut failures = Vec::new();
    for engine in ENGINES {
        match attempt(client, engine, query, env, user_dir) {
            Ok(parsed) => replies.push(parsed.results),
            Err(error) => failures.push(format!("{}: {error}", engine.name())),
        }
    }
    if replies.is_empty() {
        return Err(failures.join("; "));
    }
    Ok(Parsed {
        answer: None,
        results: merge(replies),
    })
}

/// A host without `www.`, the path without a trailing slash, and the query string. Two URLs with the
/// same key are the same page for ranking purposes.
fn dedup_key(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return url.to_owned();
    };
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let path = parsed.path().trim_end_matches('/');
    match parsed.query() {
        Some(query) => format!("{host}{path}?{query}"),
        None => format!("{host}{path}"),
    }
}

struct Merged {
    result: SearchResult,
    engines: usize,
    best_rank: usize,
}

/// Each engine's list in its own order. Duplicates within one engine count once.
fn merge(replies: Vec<Vec<SearchResult>>) -> Vec<SearchResult> {
    let mut merged: Vec<Merged> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for reply in replies {
        let mut seen = HashSet::new();
        for (rank, result) in reply.into_iter().enumerate() {
            let key = dedup_key(&result.url);
            if !seen.insert(key.clone()) {
                continue;
            }
            match index.get(&key) {
                Some(&position) => {
                    let entry = &mut merged[position];
                    entry.engines += 1;
                    entry.best_rank = entry.best_rank.min(rank);
                    if entry.result.snippet.is_empty() {
                        entry.result.snippet = result.snippet;
                    }
                }
                None => {
                    index.insert(key, merged.len());
                    merged.push(Merged {
                        result,
                        engines: 1,
                        best_rank: rank,
                    });
                }
            }
        }
    }
    merged.sort_by(|a, b| {
        b.engines
            .cmp(&a.engines)
            .then(a.best_rank.cmp(&b.best_rank))
    });
    merged.into_iter().map(|entry| entry.result).collect()
}

#[cfg(test)]
mod tests;
