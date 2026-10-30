//! Replayable expectation-versus-observation checks, not a browser or execution controller.
use super::evidence::publish;
use crate::config::types::Config;
use crate::runtime::evidence::{
    Evidence, EvidenceContext, EvidenceKind, Location, Method, Representation,
};
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::{ToolOutput, Workspace};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn tools(_: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Reality)]
}
struct Reality;
impl Tool for Reality {
    fn name(&self) -> &str {
        "reality"
    }
    fn schema(&self) -> Value {
        crate::tools::tool_schema(
            "reality",
            "Compare a repository/document expectation with separately captured runtime evidence. Use normal authorized browser/execution/evidence tools to observe first; this tool never executes the application. check computes an exact literal occurrence count or JSON-pointer scalar from retained text/JSON observation, comparing it to a caller-interpreted expected scalar. Exact expectation quote is validated, but the mapping from prose/code to expected scalar is NOT independently verified. assess records an explicitly caller/model-attributed visual/behavioral mismatch with both inputs, never a measured verdict. Observation must be observation-kind evidence; its producer classification alone is not successful-check proof. Retains both parents for replay, no mutable source reopening or verification credit.",
            json!({
                "action":{"type":"string","enum":["check","assess"]},
                "expectation":{"type":"string"},"observation":{"type":"string"},
                "expectation_quote":{"type":"string","maxLength":4096},
                "claim":{"type":"string","maxLength":4096},
                "expected":{},
                "selector":{"type":"object","properties":{"kind":{"type":"string","enum":["count","pointer"]},"value":{"type":"string","maxLength":4096}},"required":["kind","value"],"additionalProperties":false},
                "operator":{"type":"string","enum":["eq","le","ge"]},
                "assessment":{"type":"string","enum":["matches","mismatch","inconclusive"]}
            }),
            &[
                "action",
                "expectation",
                "observation",
                "expectation_quote",
                "claim",
            ],
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
struct Selector {
    kind: String,
    value: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    expectation: String,
    observation: String,
    expectation_quote: String,
    claim: String,
    #[serde(default)]
    expected: Value,
    selector: Option<Selector>,
    operator: Option<String>,
    assessment: Option<String>,
}
fn bounded(text: &str) -> Result<()> {
    ensure!(
        !text.trim().is_empty() && text.len() <= 4096,
        "claim/quote/selector must contain 1..=4096 bytes"
    );
    Ok(())
}
fn numeric(value: &Value) -> Result<f64> {
    // Retain the data tool's lexical range guard before conversion, including exponent spellings.
    super::data::safe_number(value)
}
/// Compare retained expectation and observed evidence without executing or reopening either source.
pub fn run(ws: &Workspace, value: &Value) -> Result<ToolOutput> {
    let args: Args = serde_json::from_value(value.clone())?;
    bounded(&args.claim)?;
    bounded(&args.expectation_quote)?;
    ensure!(
        args.expectation != args.observation,
        "expectation and observation must be separate citations"
    );
    let store = ws
        .artifacts()
        .context("reality comparison requires artifact storage")?;
    let expected_record = store.evidence_record(&args.expectation)?;
    let observed_record = store.evidence_record(&args.observation)?;
    ensure!(
        observed_record.evidence.kind == EvidenceKind::Observation,
        "observation must be captured observation evidence, not an inference/plan"
    );
    ensure!(
        expected_record
            .evidence
            .representation
            .media_type
            .starts_with("text/")
            || expected_record.evidence.representation.media_type == "application/json",
        "expectation must be retained text/JSON"
    );
    let expected_bytes = store.evidence_bytes(&expected_record, 256 * 1024)?;
    let expected_text = std::str::from_utf8(&expected_bytes)?;
    ensure!(
        expected_text.contains(&args.expectation_quote),
        "expectation quote is not present verbatim"
    );
    let observed_bytes = store.evidence_bytes(&observed_record, 2 * 1024 * 1024)?;
    let mut result = json!({"type":"reality_comparison","expectation":args.expectation,"observation":args.observation,"expectation_quote":args.expectation_quote,"claim":args.claim,"expectation_attribution":"caller interprets source; quote presence does not establish entailment","verification_credit":false,"execution":"none; separately captured observations only"});
    match args.action.as_str() {
        "check" => {
            let selector = args.selector.context("check requires selector")?;
            ensure!(selector.value.len() <= 4096, "selector exceeds 4096 bytes");
            ensure!(
                observed_record
                    .evidence
                    .representation
                    .media_type
                    .starts_with("text/")
                    || observed_record.evidence.representation.media_type == "application/json",
                "check requires text/JSON observation; use assess for visual interpretation"
            );
            let text = std::str::from_utf8(&observed_bytes)?;
            let actual = match selector.kind.as_str() {
                "count" => {
                    bounded(&selector.value)?;
                    json!(text.matches(&selector.value).count())
                }
                "pointer" => {
                    ensure!(
                        selector.value.is_empty() || selector.value.starts_with('/'),
                        "JSON pointer must be empty or start with /"
                    );
                    let document: Value = serde_json::from_str(text)?;
                    document
                        .pointer(&selector.value)
                        .context("JSON pointer absent from observation")?
                        .clone()
                }
                _ => anyhow::bail!("unknown observation selector"),
            };
            ensure!(
                value.get("expected").is_some(),
                "check requires expected scalar"
            );
            let expected = args.expected;
            ensure!(
                !expected.is_array()
                    && !expected.is_object()
                    && !actual.is_array()
                    && !actual.is_object(),
                "comparison requires scalar values"
            );
            let op = args.operator.as_deref().unwrap_or("eq");
            let matches = match op {
                "eq" if actual.is_number() && expected.is_number() => {
                    numeric(&actual)? == numeric(&expected)?
                }
                "eq" => actual == expected,
                "le" => numeric(&actual)? <= numeric(&expected)?,
                "ge" => numeric(&actual)? >= numeric(&expected)?,
                _ => anyhow::bail!("unknown comparison operator"),
            };
            result["measurement"] = json!({"selector":{"kind":selector.kind,"value":selector.value},"actual":actual,"expected":expected,"operator":op,"matches_caller_expectation":matches,"method":"literal non-overlapping substring count or exact JSON pointer; no semantic log/event parser"});
        }
        "assess" => {
            let assessment = args.assessment.context("assess requires assessment")?;
            ensure!(
                ["matches", "mismatch", "inconclusive"].contains(&assessment.as_str()),
                "unknown assessment"
            );
            result["assessment"] = json!({"value":assessment,"attribution":"caller/model interpretation of cited observation; not independently measured or verified","observation_location":observed_record.evidence.location});
        }
        _ => anyhow::bail!("unknown reality action"),
    }
    let bytes = serde_json::to_vec(&result)?;
    let item = Evidence {
        source:args.expectation.clone(), location:Location::Whole, method:Method::ModelInterpretation,
        producer:"dial/reality-v1".into(), kind:EvidenceKind::Inference,
        summary:"Expectation-versus-observation comparison; expectation mapping and semantic assessments are caller-authored".into(),
        representation:Representation::of_bytes("application/json", &bytes),
        limitations:vec!["No application execution or source freshness claim. Observation-kind metadata is not proof; comparisons to interpreted expectations are not independent verification or causality.".into()],
        context:EvidenceContext { parents:vec![args.expectation,args.observation], ..Default::default() },
    };
    publish(ws, ToolOutput::structured(result), item, &bytes)
}
#[cfg(test)]
mod tests;
