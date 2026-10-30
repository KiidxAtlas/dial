//! Immutable research snapshots resume by authenticated citation, not process-local state.
use super::evidence::publish;
use crate::config::types::Config;
use crate::runtime::evidence::{
    Evidence, EvidenceContext, EvidenceKind, Location, Method, Representation,
};
use crate::storage::artifact::ArtifactStore;
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::{ToolOutput, Workspace};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};
const PRODUCER: &str = "dial/research-notebook-v1";
const MAX_BYTES: usize = 128 * 1024;
const MAX_HISTORY: usize = 32;
const MAX_REFERENCE_BYTES: usize = 16 * 1024 * 1024;
const MAX_AUTHENTICATED_BYTES: usize = 32 * 1024 * 1024;

/// One authentication allowance shared by snapshots, history and all linked payloads.
struct Authentication<'a> {
    store: &'a ArtifactStore,
    remaining: usize,
    seen: BTreeSet<String>,
}
impl Authentication<'_> {
    fn bytes(&mut self, reference: &str, limit: usize) -> Result<Vec<u8>> {
        let record = self.store.evidence_record(reference)?;
        let bytes = self
            .store
            .evidence_bytes(&record, limit.min(self.remaining))?;
        self.remaining -= bytes.len();
        self.seen.insert(reference.into());
        Ok(bytes)
    }
    fn reference(&mut self, reference: &str) -> Result<()> {
        if !self.seen.contains(reference) {
            self.bytes(reference, MAX_REFERENCE_BYTES)?;
        }
        Ok(())
    }
}

/// Register research notebooks (distinct from Jupyter editing).
pub fn tools(_: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Notebook)]
}
struct Notebook;
impl Tool for Notebook {
    fn name(&self) -> &str {
        "research"
    }
    fn schema(&self) -> Value {
        crate::tools::tool_schema(
            "research",
            "Create/update immutable evidence-driven research snapshots or resume by citation after restart. Summaries, support/contradiction and explanations are caller assessments, never verification. Shows retained prior plans/results to avoid duplicate tests; does not execute experiments. Use investigate to plan or attach experiment results. Updates append entries, preserve predecessors and explicitly replace only supplied working-state fields. Maximum 32 revisions, 128 entries, 128 KiB snapshot; fork a new bounded notebook when exhausted.",
            json!({
                "action":{"type":"string","enum":["create","update","resume"]},"reference":{"type":"string"},"question":{"type":"string","maxLength":4096},
                "entries":{"type":"array","maxItems":32,"items":{"type":"object","properties":{"kind":{"type":"string","enum":["observation","plan","result","correction"]},"summary":{"type":"string","maxLength":4096},"references":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"string"}},"supports":{"type":"array","maxItems":16,"items":{"type":"string"}},"contradicts":{"type":"array","maxItems":16,"items":{"type":"string"}}},"required":["kind","summary","references"],"additionalProperties":false}},
                "open_questions":{"type":"array","maxItems":16,"items":{"type":"string"}},"competing_explanations":{"type":"array","maxItems":16,"items":{"type":"string"}},"unknowns":{"type":"array","maxItems":16,"items":{"type":"string"}},"next_experiment":{"type":"string","maxLength":4096}
            }),
            &["action"],
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
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    kind: String,
    summary: String,
    references: Vec<String>,
    #[serde(default)]
    supports: Vec<String>,
    #[serde(default)]
    contradicts: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    reference: Option<String>,
    question: Option<String>,
    entries: Option<Vec<Entry>>,
    open_questions: Option<Vec<String>>,
    competing_explanations: Option<Vec<String>>,
    unknowns: Option<Vec<String>>,
    next_experiment: Option<String>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    #[serde(rename = "type")]
    kind: String,
    version: u32,
    revision: usize,
    predecessor: Option<String>,
    question: String,
    entries: Vec<Entry>,
    open_questions: Vec<String>,
    competing_explanations: Vec<String>,
    unknowns: Vec<String>,
    next_experiment: Option<String>,
    attribution: String,
}
fn bounded(text: &str) -> Result<()> {
    ensure!(
        !text.trim().is_empty() && text.len() <= 4096,
        "text must contain 1..4096 bytes"
    );
    Ok(())
}
fn links(snapshot: &Snapshot) -> BTreeSet<String> {
    snapshot
        .entries
        .iter()
        .flat_map(|e| e.references.iter().chain(&e.supports).chain(&e.contradicts))
        .chain(snapshot.predecessor.iter())
        .cloned()
        .collect()
}
fn validate(snapshot: &Snapshot) -> Result<()> {
    ensure!(
        snapshot.kind == "research_notebook"
            && snapshot.version == 1
            && (1..=MAX_HISTORY).contains(&snapshot.revision),
        "unsupported notebook identity or history"
    );
    ensure!(
        snapshot.entries.len() <= 128,
        "notebook exceeds 128 entries"
    );
    bounded(&snapshot.question)?;
    for list in [
        &snapshot.open_questions,
        &snapshot.competing_explanations,
        &snapshot.unknowns,
    ] {
        ensure!(list.len() <= 16, "working state exceeds 16 items");
        for item in list {
            bounded(item)?;
        }
    }
    if let Some(next) = &snapshot.next_experiment {
        bounded(next)?;
    }
    for entry in &snapshot.entries {
        ensure!(
            ["observation", "plan", "result", "correction"].contains(&entry.kind.as_str()),
            "unknown entry kind"
        );
        bounded(&entry.summary)?;
        ensure!(
            (1..=16).contains(&entry.references.len())
                && entry.supports.len() <= 16
                && entry.contradicts.len() <= 16,
            "entry reference limit exceeded"
        );
    }
    ensure!(
        links(snapshot).len() <= 64,
        "notebook exceeds 64 dependency citations"
    );
    Ok(())
}
fn read(auth: &mut Authentication<'_>, reference: &str) -> Result<Snapshot> {
    let record = auth.store.evidence_record(reference)?;
    ensure!(
        record.evidence.producer == PRODUCER
            && record.evidence.representation.media_type == "application/json"
            && record.evidence.method == Method::ModelInterpretation
            && record.evidence.kind == EvidenceKind::Inference,
        "reference is not a research notebook producer/type"
    );
    let snapshot: Snapshot = serde_json::from_slice(&auth.bytes(reference, MAX_BYTES)?)?;
    validate(&snapshot)?;
    let declared: BTreeSet<_> = record.evidence.context.parents.iter().cloned().collect();
    ensure!(
        links(&snapshot) == declared,
        "notebook dependency links disagree with manifest"
    );
    Ok(snapshot)
}
fn resume(auth: &mut Authentication<'_>, reference: &str) -> Result<Snapshot> {
    let snapshot = read(auth, reference)?;
    let mut previous = snapshot.predecessor.clone();
    let mut expected = snapshot.revision - 1;
    let mut seen = BTreeSet::from([reference.to_owned()]);
    while let Some(reference) = previous {
        ensure!(
            expected > 0 && seen.insert(reference.clone()),
            "invalid/cyclic notebook history"
        );
        let parent = read(auth, &reference)?;
        ensure!(
            parent.revision == expected && parent.question == snapshot.question,
            "notebook predecessor revision/question mismatch"
        );
        expected -= 1;
        previous = parent.predecessor;
    }
    ensure!(expected == 0, "notebook predecessor history is incomplete");
    for reference in links(&snapshot) {
        auth.reference(&reference)?;
    }
    Ok(snapshot)
}
fn output(snapshot: &Snapshot) -> Result<ToolOutput> {
    let mut data = serde_json::to_value(snapshot)?;
    data["verification_credit"] = json!(false);
    data["retained_tests_results"] = json!(snapshot.entries.iter().enumerate().filter(|(_,e)|e.kind == "plan" || e.kind == "result").map(|(index,entry)|json!({"entry":index,"record":entry,"status":"retained caller-attributed record; not rerun or verified"})).collect::<Vec<_>>());
    let text = format!(
        "UNTRUSTED RESEARCH NOTEBOOK (not verification; prior results are retained records only):\n{}",
        serde_json::to_string(&data)?
    );
    let mut out = ToolOutput::structured(data);
    out.text = text;
    Ok(out)
}
/// Create, append or resume bounded snapshots without executing proposed tests.
pub fn run(ws: &Workspace, value: &Value) -> Result<ToolOutput> {
    ensure!(
        serde_json::to_vec(value)?.len() <= MAX_BYTES,
        "notebook arguments exceed 128 KiB"
    );
    let args: Args = serde_json::from_value(value.clone())?;
    let store = ws
        .artifacts()
        .context("research requires an artifact store")?;
    let mut auth = Authentication {
        store,
        remaining: MAX_AUTHENTICATED_BYTES,
        seen: BTreeSet::new(),
    };
    ensure!(
        ["create", "update", "resume"].contains(&args.action.as_str()),
        "unknown research action"
    );
    let mut snapshot = if args.action == "create" {
        ensure!(
            args.reference.is_none(),
            "create cannot replace a predecessor; use update"
        );
        Snapshot {kind:"research_notebook".into(),version:1,revision:1,predecessor:None,question:args.question.context("question required")?,entries:vec![],open_questions:vec![],competing_explanations:vec![],unknowns:vec![],next_experiment:None,attribution:"caller/agent-authored summaries and assessments; no independent semantic proof or successful-check credit".into()}
    } else {
        let reference = args.reference.as_deref().context("reference required")?;
        let mut prior = resume(&mut auth, reference)?;
        if args.action == "resume" {
            return output(&prior);
        }
        ensure!(
            args.question.is_none(),
            "update preserves the original question"
        );
        ensure!(
            prior.revision < MAX_HISTORY,
            "notebook history exceeds 32 revisions"
        );
        prior.revision += 1;
        prior.predecessor = Some(reference.into());
        prior
    };
    if let Some(entries) = args.entries {
        ensure!(entries.len() <= 32, "append at most 32 entries");
        snapshot.entries.extend(entries);
    }
    if let Some(items) = args.open_questions {
        snapshot.open_questions = items;
    }
    if let Some(items) = args.competing_explanations {
        snapshot.competing_explanations = items;
    }
    if let Some(items) = args.unknowns {
        snapshot.unknowns = items;
    }
    if let Some(next) = args.next_experiment {
        snapshot.next_experiment = Some(next);
    }
    validate(&snapshot)?;
    let parents: Vec<_> = links(&snapshot).into_iter().collect();
    for reference in &parents {
        auth.reference(reference)?;
    }
    let bytes = serde_json::to_vec(&snapshot)?;
    ensure!(
        bytes.len() <= MAX_BYTES,
        "notebook snapshot exceeds 128 KiB"
    );
    let item = Evidence {source:"dial:research".into(),location:Location::Whole,method:Method::ModelInterpretation,producer:PRODUCER.into(),kind:EvidenceKind::Inference,summary:"Append-only research notebook snapshot with retained evidence dependencies".into(),representation:Representation::of_bytes("application/json",&bytes),limitations:vec!["Caller-authored research state; references authenticate retained bytes, not truth, execution authority, independent approval or successful verification. Retention expiry may make history unavailable.".into()],context:EvidenceContext{parents,..Default::default()}};
    publish(ws, output(&snapshot)?, item, &bytes)
}
#[cfg(test)]
mod tests;
