//! Bounded evidence inspection, lineage, conflict reporting and append-only human corrections.
use super::evidence::publish;
use crate::config::types::Config;
use crate::runtime::evidence::{
    Evidence, EvidenceContext, EvidenceKind, Location, Method, Representation,
};
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::{ToolOutput, Workspace};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};

const MAX_NODES: usize = 64;
const MAX_BYTES: usize = 1024 * 1024;
const PREVIEW_BYTES: usize = 4096;

/// Register model-facing access to the same workbench used by `/evidence`.
pub fn tools(_: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Workbench)]
}
struct Workbench;
impl Tool for Workbench {
    fn name(&self) -> &str {
        "workbench"
    }
    fn schema(&self) -> Value {
        crate::tools::tool_schema(
            "workbench",
            "Inspect retained citations, bounded parent lineage, coverage gaps, unavailable artifacts and conflicting captured source identities; append a correction without rewriting evidence. Corrections remain caller-authored, never human-approved unless the actual human supplied them. Never reopens mutable sources; source freshness is unknown until an authorized explicit evidence freshness call. Use investigate for semantic comparison.",
            json!({
                "action":{"type":"string","enum":["inspect","lineage","correct"]},
                "references":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"string"}},
                "depth":{"type":"integer","minimum":0,"maximum":8},
                "correction":{"type":"string","maxLength":8192}
            }),
            &["action", "references"],
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
        run(ctx.workspace, args).unwrap_or_else(|e| ToolOutput::err(format!("{e:#}")))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    references: Vec<String>,
    depth: Option<usize>,
    correction: Option<String>,
}

/// Inspect immutable evidence only; failure to find a node is visible rather than hidden.
pub fn run(ws: &Workspace, value: &Value) -> Result<ToolOutput> {
    let args: Args = serde_json::from_value(value.clone())?;
    ensure!(
        (1..=16).contains(&args.references.len()),
        "supply 1..=16 citations"
    );
    let depth = args.depth.unwrap_or(3);
    ensure!(depth <= 8, "lineage depth must be 0..=8");
    let store = ws
        .artifacts()
        .context("workbench requires the project's artifact store")?;
    if args.action == "correct" {
        let correction = args.correction.context("correction is required")?;
        ensure!(
            !correction.trim().is_empty() && correction.len() <= 8192,
            "correction must contain 1..=8192 bytes"
        );
        for reference in &args.references {
            store.evidence_record(reference)?;
        }
        let value = json!({"type":"evidence_correction","correction":correction,"attribution":"caller; no human approval inferred","supersedes_in_place":false,"references":args.references});
        let bytes = serde_json::to_vec(&value)?;
        let item = Evidence {
            source: args.references[0].clone(), location: Location::Whole, method: Method::ModelInterpretation,
            producer: "dial/evidence-correction".into(), kind: EvidenceKind::Inference,
            summary: "Append-only caller correction; original evidence remains unchanged".into(),
            representation: Representation::of_bytes("application/json", &bytes),
            limitations: vec!["Correction content is untrusted. A caller/model annotation is not automatically a human correction or independently verified fact.".into()],
            context: EvidenceContext { parents: args.references, ..Default::default() },
        };
        let mut out = ToolOutput::structured(value);
        out.text = String::from_utf8(bytes.clone())?;
        return publish(ws, out, item, &bytes);
    }
    ensure!(
        ["inspect", "lineage"].contains(&args.action.as_str()),
        "unknown workbench action"
    );
    let depth = if args.action == "inspect" { 0 } else { depth };
    let mut queue: VecDeque<_> = args.references.iter().cloned().map(|r| (r, 0)).collect();
    let mut seen = BTreeSet::new();
    let mut nodes = Vec::new();
    let mut conflicts: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut budget = MAX_BYTES;
    let mut omitted = BTreeSet::new();
    while let Some((reference, level)) = queue.pop_front() {
        if seen.contains(&reference) {
            continue;
        }
        if seen.len() == MAX_NODES {
            omitted.insert(reference);
            continue;
        }
        seen.insert(reference.clone());
        let manifest_retention = store
            .info(&reference)
            .unwrap_or_else(|error| json!({"error":format!("{error:#}")}));
        let record = match store.evidence_record(&reference) {
            Ok(record) => record,
            Err(error) => {
                nodes.push(json!({"reference":reference,"status":"unavailable_or_invalid","manifest_retention":manifest_retention,"error":format!("{error:#}"),"freshness":"unknown"}));
                continue;
            }
        };
        if let Some(snapshot) = &record.evidence.context.snapshot {
            conflicts
                .entry(record.evidence.source.clone())
                .or_default()
                .insert(snapshot.sha256.clone());
        }
        let mut preview = Value::Null;
        let mut representation_status = "not_read_budget_exhausted".to_owned();
        if budget > 0 {
            match store.evidence_bytes(&record, budget) {
                Ok(bytes) => {
                    budget -= bytes.len();
                    representation_status = "authenticated".into();
                    if let Ok(text) = std::str::from_utf8(&bytes) {
                        let end = text.floor_char_boundary(PREVIEW_BYTES.min(text.len()));
                        preview = json!({"text":&text[..end],"truncated":end < text.len(),"total_bytes":bytes.len()});
                    }
                }
                Err(error) => {
                    // A failed authentication may still have read the full allowance. Do not
                    // spend it again on another node and overstate the traversal's byte bound.
                    budget = 0;
                    representation_status =
                        format!("unavailable_or_exceeds_remaining_budget: {error:#}");
                }
            }
        }
        let mut links = record.evidence.context.parents.clone();
        for alignment in &record.evidence.context.alignments {
            links.extend(alignment.basis.iter().cloned());
        }
        links.sort();
        links.dedup();
        for link in links {
            if level < depth {
                queue.push_back((link, level + 1));
            } else if !seen.contains(&link) {
                omitted.insert(link);
            }
        }
        let snapshot_artifact = record
            .evidence
            .context
            .snapshot
            .as_ref()
            .and_then(|s| s.artifact.as_ref());
        let snapshot_status = snapshot_artifact.map(|uri| match store.info(uri) {
            Ok(info) => info,
            Err(error) => json!({"error":format!("{error:#}")}),
        });
        let representation_retention = store
            .info(&record.representation_uri)
            .unwrap_or_else(|error| json!({"error":format!("{error:#}")}));
        nodes.push(json!({"reference":reference,"status":"manifest_authenticated","manifest_retention":manifest_retention,"representation_retention":representation_retention,"record":record,"representation_status":representation_status,"preview":preview,"snapshot_retention":snapshot_status,"freshness":"unknown; mutable source not reopened"}));
    }
    for reference in &seen {
        omitted.remove(reference);
    }
    let conflicting_snapshots: Vec<_> = conflicts.into_iter().filter(|(_, hashes)| hashes.len() > 1).map(|(source, hashes)| json!({"source":source,"snapshots":hashes,"meaning":"different captured source bytes; may be revisions, not necessarily contradictory claims"})).collect();
    let value = json!({"nodes":nodes,"conflicting_snapshots":conflicting_snapshots,"omitted_references":omitted,"bounds":{"nodes":MAX_NODES,"depth":depth,"representation_bytes":MAX_BYTES},"limitations":["Untrusted retained data, not truth or permission. No source paths reopened.","Only supplied roots and reachable parents are inspected; corrections/children are not automatically discovered. Supply their citations as roots.","Unavailable citations may be expired, absent or invalid; absence does not prove expiration. Snapshot lifecycle presence is not content authentication. Semantic contradictions require interpretation."]});
    let mut out = ToolOutput::structured(value);
    out.text = render(out.data.as_ref().context("missing workbench data")?)?;
    Ok(out)
}

/// Plain-text presentation for the scrollable terminal transcript, never ANSI/HTML source rendering.
pub fn render(value: &Value) -> Result<String> {
    let Some(nodes) = value["nodes"].as_array() else {
        bail!("missing workbench nodes")
    };
    let mut text = String::from("EVIDENCE WORKBENCH — untrusted data, not verification\n");
    for node in nodes {
        text.push_str(&format!(
            "\n{}\nStatus: {}\n",
            node["reference"].as_str().unwrap_or("?"),
            node["status"].as_str().unwrap_or("?")
        ));
        text.push_str(&format!(
            "Manifest lifecycle: {}\nRepresentation lifecycle: {}\nSnapshot lifecycle: {}\n",
            node["manifest_retention"],
            node["representation_retention"],
            node["snapshot_retention"]
        ));
        if let Some(record) = node.get("record") {
            text.push_str(&format!("Source: {}\nProducer: {} · {}\nSummary: {}\nLocation: {}\nCoverage: {}\nParents: {}\nRepresentation: {}\nFreshness: unknown (source not reopened)\n", record["evidence"]["source"].as_str().unwrap_or("?"), record["evidence"]["producer"].as_str().unwrap_or("?"), record["evidence"]["kind"].as_str().unwrap_or("?"), record["evidence"]["summary"].as_str().unwrap_or("?"), record["evidence"]["location"], record["evidence"]["context"]["coverage"], record["evidence"]["context"]["parents"], node["representation_status"].as_str().unwrap_or("?")));
            if let Some(preview) = node["preview"]["text"].as_str() {
                text.push_str(&format!(
                    "Preview (truncated={}):\n{preview}\n",
                    node["preview"]["truncated"]
                ));
            }
            text.push_str(&format!(
                "Limitations: {}\n",
                record["evidence"]["limitations"]
            ));
        } else {
            text.push_str(&format!(
                "{}\n",
                node["error"].as_str().unwrap_or("unavailable")
            ));
        }
    }
    text.push_str(&format!(
        "\nConflicting source snapshots: {}\nUnvisited citations: {}\n",
        value["conflicting_snapshots"], value["omitted_references"]
    ));
    text.push_str(&format!(
        "Bounds: {}\nLimitations: {}\n",
        value["bounds"], value["limitations"]
    ));
    // Prevent embedded terminal controls or bidi overrides from impersonating UI chrome.
    Ok(text
        .chars()
        .map(|c| {
            if (c.is_control() && c != '\n' && c != '\t')
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect())
}

#[cfg(test)]
mod tests;
