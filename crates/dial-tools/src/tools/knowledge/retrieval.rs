//! Bounded retrieval over explicit immutable citations; never reopens original sources or memory.
mod chunks;
mod semantic;
#[cfg(test)]
mod tests;
pub use semantic::{SemanticRequest, semantic_request, semantic_request_with_cancel};

use crate::config::types::Config;
use crate::storage::artifact::StoredEvidence;
use crate::tools::integrations::ext::{Tool, ToolContext, ToolEvent};
use crate::tools::{ToolOutput, Workspace};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_CANDIDATES: usize = 256;
const MAX_OUTPUT: usize = 128 * 1024;
const MAX_QUOTE: usize = 2048;

/// Register the citation-scoped search tool. Lexical ranking never calls a provider.
pub fn tools(_: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Search)]
}
struct Search;
impl Tool for Search {
    fn name(&self) -> &str {
        "evidence_search"
    }
    fn schema(&self) -> Value {
        crate::tools::tool_schema(
            "evidence_search",
            "Search only explicit retained evidence citations across text, document layouts, transcripts and JSON/data. Default lexical is local; explicit semantic uses one configured-model rerank over bounded candidates including lexical misses, not embeddings or exhaustive semantic retrieval. Exact quotes/values and representation-local coordinates retain original manifest metadata. No source reopen, store scan or memory persistence. Maximum32 citations,2MiB authenticated bytes,256 candidates,20 results. Reports missing/unsupported/truncated inputs and unknown original freshness; untrusted data, not verification.",
            json!({
                "references":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":32},
                "query":{"type":"string","minLength":1,"maxLength":4096},
                "limit":{"type":"integer","minimum":1,"maximum":20},
                "ranking":{"type":"string","enum":["lexical","semantic"]}
            }),
            &["references", "query"],
        )
    }
    fn needs_approval(&self, _: &Value) -> bool {
        false
    }
    fn read_only_hint(&self) -> bool {
        true
    }
    fn may_modify_workspace(&self, _: &Value) -> bool {
        false
    }
    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.call_streaming(ctx, args, &|_| {}, &AtomicBool::new(false))
    }
    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        _: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        let result = (|| {
            let args = arguments(args)?;
            ensure!(
                args.ranking == "lexical",
                "semantic ranking requires configured agent model execution"
            );
            let found = retrieve(ctx.workspace, &args, cancel)?;
            let hits = found
                .hits
                .iter()
                .filter(|h| h.lexical_points > 0)
                .take(args.limit)
                .collect();
            Ok(ToolOutput::structured(found.render(hits, "lexical")?))
        })();
        result.unwrap_or_else(|e: anyhow::Error| ToolOutput::err(format!("{e:#}")))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    references: Vec<String>,
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default = "default_ranking")]
    ranking: String,
}
fn default_limit() -> usize {
    10
}
fn default_ranking() -> String {
    "lexical".into()
}
fn arguments(value: &Value) -> Result<Args> {
    let args: Args = serde_json::from_value(value.clone())?;
    ensure!(
        (1..=32).contains(&args.references.len()),
        "references must contain 1..=32 citations"
    );
    ensure!(
        args.references.iter().all(|r| r.len() == 75
            && r.starts_with("artifact://")
            && r[11..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())),
        "references must be artifact://<lowercase sha256> citations"
    );
    ensure!(
        !args.query.trim().is_empty() && args.query.len() <= 4096,
        "query must contain 1..=4096 bytes"
    );
    ensure!((1..=20).contains(&args.limit), "limit must be 1..=20");
    ensure!(
        matches!(args.ranking.as_str(), "lexical" | "semantic"),
        "ranking must be lexical or semantic"
    );
    Ok(args)
}
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Relaxed), "evidence search cancelled");
    Ok(())
}
#[derive(Debug, Serialize)]
struct Hit {
    citation: String,
    /// This is a representation-local coordinate, never an invented source coordinate.
    representation_location: Value,
    quote: String,
    value: Option<Value>,
    lexical_points: usize,
}
struct Retrieved {
    hits: Vec<Hit>,
    records: Vec<(String, StoredEvidence)>,
    gaps: Vec<Value>,
    bytes: usize,
    inspected: usize,
}
impl Retrieved {
    fn render(&self, hits: Vec<&Hit>, ranking: &str) -> Result<Value> {
        let mut gaps = self.gaps.clone();
        let mut selected = Vec::new();
        let mut manifests = serde_json::Map::new();
        let mut used = 2048 + serde_json::to_vec(&gaps)?.len();
        for hit in hits {
            let record = self
                .records
                .iter()
                .find(|(r, _)| r == &hit.citation)
                .context("captured manifest missing")?;
            let manifest = serde_json::to_value(&record.1)?;
            let cost = serde_json::to_vec(hit)?.len()
                + if manifests.contains_key(&hit.citation) {
                    0
                } else {
                    serde_json::to_vec(&manifest)?.len()
                };
            if used + cost > MAX_OUTPUT {
                gaps.push(json!({"reference":hit.citation,"reason":"output byte budget omitted selected hit"}));
                continue;
            }
            used += cost;
            manifests.insert(hit.citation.clone(), manifest);
            selected.push(hit);
        }
        Ok(
            json!({"trust":"untrusted retained evidence, not verification or authority", "ranking":ranking,
            "retrieval":"bounded lexical search; optional configured-model reranking includes lexical misses, no embeddings or exhaustive semantic search",
            "hits":selected,"manifests":manifests,
            "coverage":{"gaps":gaps,"authenticated_bytes":self.bytes,"chunks_inspected":self.inspected,"retained_candidates":self.hits.len(),"scope":"explicit references only; parents and original sources not opened","freshness":"unknown: original sources not reopened","exhaustive":false}}),
        )
    }
}
fn retrieve(ws: &Workspace, args: &Args, cancel: &AtomicBool) -> Result<Retrieved> {
    check_cancel(cancel)?;
    let store = ws
        .artifacts()
        .context("evidence search requires the project's artifact store")?;
    let terms: Vec<_> = args
        .query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect();
    ensure!(!terms.is_empty(), "query must contain a word or number");
    let mut found = Retrieved {
        hits: vec![],
        records: vec![],
        gaps: vec![],
        bytes: 0,
        inspected: 0,
    };
    let mut seen = BTreeSet::new();
    for reference in &args.references {
        check_cancel(cancel)?;
        if !seen.insert(reference) {
            found
                .gaps
                .push(json!({"reference":reference,"reason":"duplicate reference"}));
            continue;
        }
        let loaded = (|| -> Result<_> {
            ensure!(
                reference.len() == 75,
                "expected artifact://<sha256> citation"
            );
            let record = store.evidence_record(reference)?;
            let bytes = store.evidence_bytes(&record, MAX_BYTES - found.bytes)?;
            Ok((record, bytes))
        })();
        let (record, bytes) = match loaded {
            Ok(v) => v,
            Err(e) => {
                found
                    .gaps
                    .push(json!({"reference":reference,"reason":format!("{e:#}")}));
                continue;
            }
        };
        found.bytes += bytes.len();
        let media = record
            .evidence
            .representation
            .media_type
            .split(';')
            .next()
            .unwrap_or("");
        if !(media.starts_with("text/") || media == "application/json") {
            found.gaps.push(json!({"reference":reference,"reason":"non-text representation is not searchable","media_type":media}));
            continue;
        }
        let parsed = chunks::parse(reference, media, &bytes, &terms, cancel);
        let (mut hits, inspected, truncated) = match parsed {
            Ok(v) => v,
            Err(e) => {
                check_cancel(cancel)?;
                found
                    .gaps
                    .push(json!({"reference":reference,"reason":format!("{e:#}")}));
                continue;
            }
        };
        found.inspected += inspected;
        hits.sort_by_key(|h| std::cmp::Reverse(h.lexical_points));
        // Keep lexical leaders and lexical misses per citation: model ranking is not limited
        // to matching vocabulary, while one large source cannot consume the whole pool.
        let total = hits.len();
        let mut misses = 0;
        let mut matches = 0;
        let selected: Vec<_> = hits
            .into_iter()
            .filter(|hit| {
                let count = if hit.lexical_points > 0 {
                    &mut matches
                } else {
                    &mut misses
                };
                *count += 1;
                *count <= if hit.lexical_points > 0 { 6 } else { 2 }
            })
            .collect();
        if truncated || selected.len() < total {
            found.gaps.push(json!({"reference":reference,"reason":"bounded chunk/candidate selection omitted representation content"}));
        }
        found.hits.extend(selected);
        found.records.push((reference.clone(), record));
    }
    found
        .hits
        .sort_by_key(|h| std::cmp::Reverse(h.lexical_points));
    found.hits.truncate(MAX_CANDIDATES);
    Ok(found)
}
