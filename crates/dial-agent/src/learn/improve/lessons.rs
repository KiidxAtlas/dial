//! Recurring-failure lesson proposals: evidence records for review, never active guidance.
//!
//! Only closed failure categories from `practice_category` qualify, and only when they recur across
//! distinct trajectories. A proposal stores the category, a fixed summary, and trajectory IDs. It
//! never stores prompts, code, outputs, or gate results. Proposals stay `proposed`, require review,
//! and never write AGENTS.md or CLAUDE.md. Nothing in this module activates a lesson.

use super::{Signals, now};
use crate::storage::store::Store;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(super) const META_KEY: &str = "lesson_proposals";
/// Matches the recurrence threshold used by `targeted_selfplay_priorities`.
const MIN_DISTINCT_TRAJECTORIES: usize = 2;
const MAX_CANDIDATES: usize = 16;
pub(super) const MAX_TRAJECTORY_REFS: usize = 32;

/// Fixed wording per closed category. Unknown categories are never proposed.
pub(super) fn summary(category: &str) -> Option<&'static str> {
    Some(match category {
        "semantic_validation" => {
            "Verify behavior with a behavioral check before reporting a change as done."
        }
        "config_precedence" => {
            "Check configuration layering and precedence before editing settings."
        }
        "negative_search" => {
            "Treat an empty search as a result to confirm: broaden or verify the path before concluding."
        }
        _ => return None,
    })
}

pub(super) fn load(store: &Store) -> Result<Vec<Value>> {
    let Some(raw) = store.get_meta(META_KEY)? else {
        return Ok(Vec::new());
    };
    ensure!(
        raw.len() <= 64 * 1024,
        "lesson proposals exceed the metadata limit"
    );
    let candidates: Vec<Value> =
        serde_json::from_str(&raw).context("invalid lesson proposal metadata")?;
    ensure!(
        candidates.len() <= MAX_CANDIDATES,
        "too many lesson proposals"
    );
    for candidate in &candidates {
        ensure!(
            candidate["category"].as_str().and_then(summary).is_some()
                && candidate["status"] == "proposed"
                && candidate["requires_review"] == true
                && candidate["trajectories"].as_array().is_some_and(|refs| {
                    refs.len() <= MAX_TRAJECTORY_REFS && refs.iter().all(|id| id.as_i64().is_some())
                }),
            "invalid lesson proposal record"
        );
    }
    Ok(candidates)
}

/// Record or refresh one proposal per recurring category and return a bounded report for the cycle
/// steps. Does not change the active version and never activates a proposal.
pub(super) fn record(store: &Store, signals: &Signals) -> Result<Value> {
    let mut candidates = load(store)?;
    let mut proposed = Vec::new();
    for (category, ids) in &signals.recurring_trajectories {
        let distinct: BTreeSet<i64> = ids.iter().copied().collect();
        let Some(text) = summary(category) else {
            continue;
        };
        if distinct.len() < MIN_DISTINCT_TRAJECTORIES {
            continue;
        }
        let index = match candidates
            .iter()
            .position(|c| c["category"].as_str() == Some(category.as_str()))
        {
            Some(index) => index,
            None if candidates.len() < MAX_CANDIDATES => {
                candidates.push(json!({"category": category, "summary": text}));
                candidates.len() - 1
            }
            None => continue,
        };
        // Union with earlier references: deduplicated, newest first, bounded.
        let mut refs: BTreeSet<i64> = candidates[index]["trajectories"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_i64)
            .collect();
        refs.extend(distinct);
        let refs: Vec<i64> = refs.into_iter().rev().take(MAX_TRAJECTORY_REFS).collect();
        let entry = &mut candidates[index];
        entry["status"] = json!("proposed");
        entry["requires_review"] = json!(true);
        entry["distinct_trajectories"] = json!(refs.len());
        entry["trajectories"] = json!(refs);
        entry["updated"] = json!(now());
        proposed.push(category.clone());
    }
    if !proposed.is_empty() {
        store.set_meta(META_KEY, &Value::Array(candidates.clone()).to_string())?;
    }
    Ok(json!({"proposed": proposed, "candidates": candidates.len()}))
}
