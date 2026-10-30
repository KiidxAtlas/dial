//! Citation-backed comparisons and falsifiable experiment records, never an execution authority.
use super::evidence::publish;
use crate::config::types::Config;
use crate::runtime::evidence::{
    Evidence, EvidenceContext, EvidenceKind, Location, Method, Representation,
};
use crate::storage::artifact::ArtifactStore;
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::{ToolOutput, Workspace};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

const MAX_INPUT: usize = 256 * 1024;
const MAX_RECORD: usize = 128 * 1024;
const MAX_ITEMS: usize = 16;

/// Register the bounded investigation tool.
pub fn tools(_: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Investigation)]
}
struct Investigation;
impl Tool for Investigation {
    fn name(&self) -> &str {
        "investigate"
    }
    fn schema(&self) -> Value {
        crate::tools::tool_schema(
            "investigate",
            "Compare retained inputs and record semantic interpretations with exact quoted highlights; plan competing falsifiable hypotheses and discriminating experiments; attach results from separately authorized tools. Never executes a proposed test, calls a model, establishes causality, or grants verification credit. compare returns bounded text inputs/diff for the agent to interpret; interpret retains the agent's attributed claims, not independent semantic proof. Actions: compare, interpret, plan, result.",
            json!({
                "action":{"type":"string","enum":["compare","interpret","plan","result"]},
                "reference":{"type":"string"},"other":{"type":"string"},
                "question":{"type":"string","maxLength":4096},
                "findings":{"type":"array","maxItems":16,"items":{"type":"object","properties":{"claim":{"type":"string"},"left_quote":{"type":"string"},"right_quote":{"type":"string"}},"required":["claim","left_quote","right_quote"],"additionalProperties":false}},
                "hypotheses":{"type":"array","minItems":2,"maxItems":16,"items":{"type":"object","properties":{"id":{"type":"string"},"explanation":{"type":"string"},"test":{"type":"string"},"prediction":{"type":"string"},"falsifier":{"type":"string"}},"required":["id","explanation","test","prediction","falsifier"],"additionalProperties":false}},
                "parents":{"type":"array","maxItems":32,"items":{"type":"string"}},
                "hypothesis":{"type":"string"},"observation":{"type":"string","maxLength":8192},
                "assessment":{"type":"string","enum":["supports","contradicts","inconclusive"]}
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
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Finding {
    claim: String,
    left_quote: String,
    right_quote: String,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Hypothesis {
    id: String,
    explanation: String,
    test: String,
    prediction: String,
    falsifier: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    reference: Option<String>,
    other: Option<String>,
    question: Option<String>,
    findings: Option<Vec<Finding>>,
    hypotheses: Option<Vec<Hypothesis>>,
    parents: Option<Vec<String>>,
    hypothesis: Option<String>,
    observation: Option<String>,
    assessment: Option<String>,
}
fn text<'a>(text: &'a Option<String>, name: &str, limit: usize) -> Result<&'a str> {
    let value = text
        .as_deref()
        .with_context(|| format!("{name} is required"))?;
    bounded(value, name, limit)?;
    Ok(value)
}
fn bounded(value: &str, name: &str, limit: usize) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= limit,
        "{name} must contain 1..={limit} bytes"
    );
    Ok(())
}
fn retained_text(store: &ArtifactStore, reference: &str) -> Result<String> {
    let record = store.evidence_record(reference)?;
    let media = &record.evidence.representation.media_type;
    ensure!(
        media.starts_with("text/") || media == "application/json",
        "comparison requires text/JSON citations; use evidence inspect/crop on image inputs and derive an explicitly attributed interpretation"
    );
    String::from_utf8(store.evidence_bytes(&record, MAX_INPUT)?)
        .context("retained representation is not UTF-8")
}
fn retain(
    ws: &Workspace,
    parents: Vec<String>,
    summary: &str,
    value: Value,
    method: Method,
) -> Result<ToolOutput> {
    let bytes = serde_json::to_vec(&value)?;
    ensure!(
        bytes.len() <= MAX_RECORD,
        "investigation record exceeds 128 KiB; narrow the selection"
    );
    let source = parents
        .first()
        .cloned()
        .unwrap_or_else(|| "dial:investigation".into());
    let item = Evidence {
        source, location: Location::Whole, method, producer: "dial/investigate-v1".into(), kind: EvidenceKind::Inference,
        summary: summary.into(), representation: Representation::of_bytes("application/json", &bytes),
        limitations: vec!["Agent/caller-authored research record, not independent semantic proof, a causal conclusion, approval, or successful check. Proposed tests must use separately authorized execution tools; supports/contradicts are caller assessments.".into()],
        context: EvidenceContext { parents, ..Default::default() },
    };
    let mut out = ToolOutput::structured(value);
    out.text = format!(
        "UNTRUSTED INVESTIGATION (not verification):\n{}",
        String::from_utf8(bytes.clone())?
    );
    publish(ws, out, item, &bytes)
}

/// Run an investigation operation against immutable retained representations only.
pub fn run(ws: &Workspace, value: &Value) -> Result<ToolOutput> {
    let args: Args = serde_json::from_value(value.clone())?;
    let store = ws
        .artifacts()
        .context("investigation requires the project's artifact store")?;
    let mut parents = args.parents.unwrap_or_default();
    ensure!(parents.len() <= 32, "at most 32 parent citations");
    for parent in &parents {
        store.evidence_record(parent)?;
    }
    match args.action.as_str() {
        "compare" | "interpret" => {
            let left = text(&args.reference, "reference", 80)?;
            let right = text(&args.other, "other", 80)?;
            let a = retained_text(store, left)?;
            let b = retained_text(store, right)?;
            parents = vec![left.into(), right.into()];
            if args.action == "compare" {
                // Bound diff work and output before invoking the line diff algorithm.
                ensure!(
                    a.len() + b.len() <= 48 * 1024 && a.lines().count() + b.lines().count() <= 2000,
                    "comparison exceeds 48 KiB or 2,000 lines; capture narrower inputs first"
                );
                let diff = similar::TextDiff::from_lines(&a, &b)
                    .unified_diff()
                    .context_radius(2)
                    .to_string();
                retain(
                    ws,
                    parents,
                    "Retained inputs and textual change highlights for semantic investigation",
                    json!({
                        "type":"comparison_inputs","left":left,"right":right,"left_text":a,"right_text":b,"text_diff":diff,
                        "next":"Interpret functional/content changes using these inputs, then call interpret with claims and exact left/right quotes. Diff alone does not establish meaning. For image comparison inspect both image citations, then use evidence derive with both parents."
                    }),
                    Method::Calculation,
                )
            } else {
                let findings = args.findings.context("findings are required")?;
                ensure!(
                    !findings.is_empty() && findings.len() <= MAX_ITEMS,
                    "supply 1..=16 findings"
                );
                for finding in &findings {
                    bounded(&finding.claim, "claim", 4096)?;
                    ensure!(
                        finding.left_quote.len() <= 4096 && finding.right_quote.len() <= 4096,
                        "quotes must be at most 4 KiB each"
                    );
                    ensure!(
                        !finding.left_quote.is_empty() || !finding.right_quote.is_empty(),
                        "each finding needs at least one exact highlight"
                    );
                    ensure!(
                        a.contains(&finding.left_quote) && b.contains(&finding.right_quote),
                        "highlight is not present verbatim in its retained input"
                    );
                }
                retain(
                    ws,
                    parents,
                    "Caller-interpreted semantic comparison with retained input highlights",
                    json!({"type":"semantic_comparison","left":left,"right":right,"findings":findings,"attribution":"caller/agent; not independently verified"}),
                    Method::ModelInterpretation,
                )
            }
        }
        "plan" => {
            let question = text(&args.question, "question", 4096)?;
            let hypotheses = args.hypotheses.context("hypotheses are required")?;
            ensure!(
                (2..=MAX_ITEMS).contains(&hypotheses.len()),
                "supply 2..=16 competing hypotheses"
            );
            let mut ids = BTreeSet::new();
            for h in &hypotheses {
                bounded(&h.id, "hypothesis id", 64)?;
                ensure!(ids.insert(&h.id), "hypothesis ids must be unique");
                for (label, field) in [
                    ("explanation", &h.explanation),
                    ("test", &h.test),
                    ("prediction", &h.prediction),
                    ("falsifier", &h.falsifier),
                ] {
                    bounded(field, label, 4096)?;
                }
            }
            retain(
                ws,
                parents,
                "Competing hypotheses and proposed discriminating tests; no tests executed",
                json!({"type":"experiment_plan","question":question,"hypotheses":hypotheses,"execution":"not executed; use the normal separately authorized tool, then attach its evidence with result"}),
                Method::ModelInterpretation,
            )
        }
        "result" => {
            let reference = text(&args.reference, "reference", 80)?;
            let plan: Value = serde_json::from_str(&retained_text(store, reference)?)?;
            ensure!(
                plan["type"] == "experiment_plan",
                "reference must be an experiment plan"
            );
            let id = text(&args.hypothesis, "hypothesis", 64)?;
            ensure!(
                plan["hypotheses"]
                    .as_array()
                    .is_some_and(|hs| hs.iter().any(|h| h["id"] == id)),
                "hypothesis is absent from retained plan"
            );
            let observation = text(&args.observation, "observation", 8192)?;
            let assessment = text(&args.assessment, "assessment", 32)?;
            ensure!(
                ["supports", "contradicts", "inconclusive"].contains(&assessment),
                "unknown assessment"
            );
            ensure!(
                parents.iter().filter(|p| p.as_str() != reference).any(|p| {
                    store
                        .evidence_record(p)
                        .is_ok_and(|r| r.evidence.kind == EvidenceKind::Observation)
                }),
                "result requires separately retained observation evidence in parents, not just a plan or inference"
            );
            if !parents.iter().any(|p| p == reference) {
                parents.push(reference.into());
            }
            retain(
                ws,
                parents,
                "Experiment observation and caller assessment linked to a prior hypothesis",
                json!({"type":"experiment_result","plan":reference,"hypothesis":id,"observation":observation,"assessment":assessment,"assessment_author":"caller/agent","verification_credit":false}),
                Method::ModelInterpretation,
            )
        }
        _ => bail!("unknown investigation action"),
    }
}

#[cfg(test)]
mod tests;
