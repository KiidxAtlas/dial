//! Bounded, replayable table reasoning; no SQL, scripts, network or database execution.
mod interpret;
mod investigate;
mod join;
mod query;
mod table;
#[cfg(test)]
mod tests;

use crate::config::types::Config;
use crate::runtime::evidence::{
    Coverage, CoverageExtent, Evidence, EvidenceContext, EvidenceKind, Location, Method,
    Representation,
};
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::{ToolOutput, Workspace};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_ROWS: usize = 10_000;
const MAX_COLUMNS: usize = 128;
const MAX_RESULT: usize = 2 * 1024 * 1024;

/// Register the bounded structured-data tool.
pub fn tools(_: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(DataTool)]
}
struct DataTool;
impl Tool for DataTool {
    fn name(&self) -> &str {
        "data"
    }
    fn schema(&self) -> Value {
        let dictionary_column = json!({"type":"object","properties":{"meaning":{"type":"string","maxLength":2048},"unit":{"type":"string","maxLength":128},"missing_values":{"type":"array","maxItems":16,"items":{}},"nullable":{"type":"boolean"},"identifier":{"type":"boolean"},"unique":{"type":"boolean"},"min":{"type":"number"},"max":{"type":"number"},"allowed_values":{"type":"array","maxItems":100,"items":{}}},"required":["meaning"],"additionalProperties":false});
        let dictionary = json!({"type":"object","properties":{"origin":{"type":"string","enum":["caller_declared","model_inferred"]},"row_meaning":{"type":"string","maxLength":2048},"columns":{"type":"object","maxProperties":128,"additionalProperties":dictionary_column}},"required":["origin","row_meaning","columns"],"additionalProperties":false});
        let join = json!({"type":"object","properties":{"path":{"type":"string"},"format":{"type":"string","enum":["json","jsonl","csv","xlsx"]},"sheet":{"type":"string"},"left_on":{"type":"array","minItems":1,"maxItems":4,"items":{"type":"string"}},"right_on":{"type":"array","minItems":1,"maxItems":4,"items":{"type":"string"}},"kind":{"type":"string","enum":["inner","left","full"]},"max_rows":{"type":"integer","minimum":1,"maximum":1000}},"required":["path","left_on","right_on","kind"],"additionalProperties":false});
        let conversions = json!({"type":"array","maxItems":16,"items":{"type":"object","properties":{"column":{"type":"string"},"output":{"type":"string"},"from":{"type":"string"},"to":{"type":"string"}},"required":["column","output","from","to"],"additionalProperties":false}});
        let dates = json!({"type":"array","maxItems":16,"items":{"type":"object","properties":{"column":{"type":"string"},"output":{"type":"string"},"format":{"type":"string","enum":["date","rfc3339"]}},"required":["column","output","format"],"additionalProperties":false}});
        crate::tools::tool_schema(
            "data",
            "Bounded replayable CSV/JSON/JSONL/XLSX investigation. profile/aggregate/select/chart filter then offset/limit (default100); whole_source:true explicitly removes window (max10,000), conflicts offset/limit. histogram/trend/correlation preserve descriptive row-linked plots, no causal/confidence claims. json_pointers:true flattens nested JSON using RFC6901 paths; JSONL IDs are physical lines. dictionary records caller_declared/model_inferred row meaning, units, missing conventions and constraints without authenticating approval or recoding values. conversions create new columns with compatible dimensions; f64 rounding disclosed. dates: date produces YYYY-MM-DD, rfc3339 requires explicit offset and produces Unix seconds (f64, fractional rounding possible), no guessed time zones. decimal_columns computes exact bounded decimal string sums instead of f64 stats; use string values to preserve input precision. join reads one bounded second source, all duplicate pairs or error, reports unmatched/duplicates and both revisions; keys compare typed lexical JSON without coercion. inspect reads XLSX structure/formulas/cached lexical values only, never evaluates formulas. calculation_reference reuses retained used_rows without reopening source; path only label. Nulls never zero. Maximum8MiB/source,10,000 rows/source,128 columns,1000 default-window/join output rows,100 bins/groups,2MiB result; exceeding limits errors, never silently truncates. No SQL/scripts/network. Untrusted observation, not verification.",
            json!({
                "path":{"type":"string"},"format":{"type":"string","enum":["json","jsonl","csv","xlsx"]},
                "sheet":{"type":"string"},"action":{"type":"string","enum":["profile","select","aggregate","chart","histogram","trend","correlation","inspect"]},
                "columns":{"type":"array","items":{"type":"string"},"maxItems":128},
                "filters":{"type":"array","maxItems":16,"items":{"type":"object","properties":{"column":{"type":"string"},"op":{"type":"string","enum":["eq","ne","lt","le","gt","ge","is_null","not_null"]},"value":{}},"required":["column","op"],"additionalProperties":false}},
                "group_by":{"type":"array","items":{"type":"string"},"maxItems":4},
                "row_ids":{"type":"array","maxItems":1000,"items":{"type":"integer","minimum":1}},
                "brush":{"type":"object","properties":{"column":{"type":"string"},"min":{"type":"number"},"max":{"type":"number"}},"required":["column"],"additionalProperties":false},
                "calculation_reference":{"type":"string"},"bins":{"type":"integer","minimum":1,"maximum":100},"order_by":{"type":"string"},
                "whole_source":{"type":"boolean"},"json_pointers":{"type":"boolean"},
                "decimal_columns":{"type":"array","maxItems":128,"items":{"type":"string"}},
                "conversions":conversions,"dates":dates,"dictionary":dictionary,"join":join,
                "offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":1000},"retain_source":{"type":"boolean"}
            }),
            &["path"],
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
        _: &(dyn Fn(crate::tools::integrations::ext::ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        serde_json::from_value(args.clone())
            .map_err(anyhow::Error::from)
            .and_then(|args| run(ctx.workspace, args, cancel))
            .unwrap_or_else(|e| ToolOutput::err(format!("{e:#}")))
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    format: Option<String>,
    sheet: Option<String>,
    action: Option<String>,
    columns: Option<Vec<String>>,
    #[serde(default)]
    filters: Vec<query::Filter>,
    #[serde(default)]
    group_by: Vec<String>,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
    #[serde(default)]
    retain_source: bool,
    row_ids: Option<Vec<usize>>,
    brush: Option<investigate::Brush>,
    calculation_reference: Option<String>,
    bins: Option<usize>,
    order_by: Option<String>,
    #[serde(default)]
    whole_source: bool,
    #[serde(default)]
    json_pointers: bool,
    dictionary: Option<interpret::Dictionary>,
    #[serde(default)]
    conversions: Vec<interpret::Conversion>,
    #[serde(default)]
    dates: Vec<interpret::DateColumn>,
    #[serde(default)]
    decimal_columns: Vec<String>,
    join: Option<join::Join>,
}

/// Shared lexical magnitude and finite-f64 guard for descriptive evidence calculations.
pub(crate) fn safe_number(value: &Value) -> Result<f64> {
    query::number(value)
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Relaxed), "data operation cancelled");
    Ok(())
}
fn source_format(ws: &Workspace, input: &str, explicit: Option<&str>) -> Result<String> {
    if let Some(format) = explicit {
        return Ok(format.to_owned());
    }
    let expanded = ws.expand_file_handle(input).map_err(anyhow::Error::msg)?;
    Ok(std::path::Path::new(&expanded)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase())
}

fn run(ws: &Workspace, args: Args, cancel: &AtomicBool) -> Result<ToolOutput> {
    use super::evidence::{publish, source};
    check_cancel(cancel)?;
    if args.action.as_deref() == Some("inspect") {
        return inspect(ws, &args, cancel);
    }
    let (bytes, mut table, format) = if let Some(reference) = &args.calculation_reference {
        let store = ws
            .artifacts()
            .context("calculation_reference requires artifact storage")?;
        let record = store.evidence_record(reference)?;
        ensure!(
            record.evidence.producer == "dial/data-v1",
            "reference must be a data calculation"
        );
        let bytes = store.evidence_bytes(&record, MAX_RESULT)?;
        let retained: Value = serde_json::from_slice(&bytes)?;
        let table = investigate::replay(&retained)?;
        (bytes, table, "retained-calculation".to_owned())
    } else {
        let bytes = source::read(ws, &args.path)?;
        ensure!(bytes.len() <= MAX_BYTES, "data source exceeds 8 MiB");
        let format = source_format(ws, &args.path, args.format.as_deref())?;
        let table = table::parse_with_paths(
            &bytes,
            &format,
            args.sheet.as_deref(),
            args.json_pointers,
            cancel,
        )?;
        (bytes, table, format)
    };
    check_cancel(cancel)?;
    let source_sheet = table.sheet.clone();
    let joined = args
        .join
        .as_ref()
        .map(|join| {
            join::load(
                ws,
                &mut table,
                join,
                args.json_pointers,
                args.retain_source,
                cancel,
            )
        })
        .transpose()?;
    interpret::apply(&mut table, &args)?;
    let mut result = query::calculate(&table, &args)?;
    if let Some(joined) = joined {
        result["join"] = joined;
    }
    if let Some(dictionary) = &args.dictionary {
        result["dictionary"] = interpret::dictionary(&table, dictionary)?;
    }
    check_cancel(cancel)?;
    let mut snapshot = source::snapshot(&bytes)?;
    if args.retain_source {
        snapshot.artifact = Some(
            ws.artifacts()
                .context("retain_source requires artifact storage")?
                .retain_exact_bytes(&bytes)?,
        );
    }
    let source_locator = args.calculation_reference.as_deref().unwrap_or(&args.path);
    result["source"] =
        json!({"path":source_locator,"snapshot":snapshot,"format":format,"sheet":source_sheet});
    result["parent_calculation"] = json!(args.calculation_reference);
    result["query"] = serde_json::to_value(&args)?;
    result["calculation_version"] = json!("dial/data-v1");
    result["limitations"] = json!([
        "Default statistics use finite IEEE-754 f64 arithmetic; decimal rounding is possible. Explicit decimal_columns instead sum bounded lexical decimals exactly and return strings, with no f64 statistics.",
        "Nulls excluded from distinct/numeric statistics; count includes nulls. Missing and empty CSV cells are null, never zero. Mixed numeric/non-numeric columns have null numeric statistics.",
        "Filter first, then offset/limit (default100); whole_source explicitly removes the window, not filters. All statistics use only used_row_ids. Source identity is not freshness beyond capture time. No causal/statistical inference.",
        "XLSX: one selected worksheet only; first nonblank row is headers, blank rows skipped, actual worksheet row IDs retained. No formula evaluation, date/style rendering, merged-cell expansion or hidden-row filtering; lexical values only. Workbook-wide coverage is always partial.",
        "Conversions use explicit compatible dimensions and finite f64, not exact decimal arithmetic. RFC3339 outputs Unix seconds with possible fractional rounding; date-only values remain calendar dates, never assigned a timezone. Unsupported/unknown offsets and leap seconds are rejected. Dictionary declarations are unverified and never authorize execution. Joins use full bounded inputs before filtering/window."
    ]);
    let rendered = serde_json::to_vec_pretty(&result)?;
    ensure!(
        rendered.len() <= MAX_RESULT,
        "result exceeds 2 MiB; select fewer rows/columns"
    );
    let complete = args.calculation_reference.is_none()
        && args.join.is_none()
        && format != "xlsx"
        && result["coverage"]["used_rows"] == result["coverage"]["source_rows"]
        && args
            .columns
            .as_ref()
            .is_none_or(|columns| columns.len() == table.columns.len());
    let item = Evidence {
        source:source_locator.to_owned(), location:Location::Whole, method:Method::Calculation,
        producer:"dial/data-v1".into(), kind:EvidenceKind::Observation,
        summary:"Bounded deterministic table calculation with exact query and source row identifiers".into(),
        representation:Representation::of_bytes("application/json", &rendered),
        limitations:vec!["Only selected rows and columns contribute; see query, coverage and used_row_ids in retained representation. Floating point rounding is possible. Not verification.".into()],
        context:EvidenceContext { parents:args.calculation_reference.iter().cloned().collect(), snapshot:Some(snapshot), coverage:Some(Coverage { extent:if complete { CoverageExtent::Complete } else { CoverageExtent::Partial }, inspected:vec![Location::Whole], omitted:vec![], truncated:!complete, failures:vec![] }), ..Default::default() },
    };
    let mut out = ToolOutput::structured(result);
    out.text = format!(
        "UNTRUSTED DATA CALCULATION:\n{}",
        String::from_utf8(rendered.clone())?
    );
    check_cancel(cancel)?;
    out = publish(ws, out, item, &rendered)?;
    if matches!(
        args.action.as_deref(),
        Some("chart" | "histogram" | "trend" | "correlation")
    ) {
        let parent = out
            .data
            .as_ref()
            .and_then(|d| d["evidence_references"][0].as_str())
            .context("chart requires artifact storage for its linked calculation")?
            .to_owned();
        let mut plotted = out.data.as_ref().context("missing calculation")?.clone();
        plotted["calculation_reference"] = json!(parent);
        let svg = if args.action.as_deref() == Some("chart") {
            query::chart(&plotted)?
        } else {
            investigate::chart(&plotted)?
        };
        ensure!(
            svg.len() <= MAX_RESULT,
            "chart exceeds 2 MiB; shorten labels or reduce groups"
        );
        let item = Evidence {
            source:parent.clone(), location:Location::Whole, method:Method::Render, producer:"dial/data-chart-v1".into(), kind:EvidenceKind::Observation,
            summary:"SVG marks linked to exact contributing source rows and retained calculation".into(), representation:Representation::of_bytes("image/svg+xml", svg.as_bytes()),
            limitations:vec!["Chart represents selected rows only; null sums have no bar. Labels are untrusted source text. Not a statistical inference.".into()],
            context:EvidenceContext { parents:vec![parent.clone()], ..Default::default() },
        };
        let chart_out = publish(
            ws,
            ToolOutput::structured(json!({"calculation_reference":parent,"svg":svg})),
            item,
            svg.as_bytes(),
        )?;
        out.data.as_mut().context("missing result")?["chart"] =
            chart_out.data.context("missing chart result")?;
        out.text.push_str(&format!("\n{}", chart_out.text));
    }
    Ok(out)
}

fn inspect(ws: &Workspace, args: &Args, cancel: &AtomicBool) -> Result<ToolOutput> {
    use super::evidence::{publish, source};
    ensure!(
        source_format(ws, &args.path, args.format.as_deref())? == "xlsx",
        "inspect requires xlsx"
    );
    ensure!(
        args.sheet.is_none()
            && args.calculation_reference.is_none()
            && args.join.is_none()
            && args.dictionary.is_none()
            && args.columns.is_none()
            && args.filters.is_empty()
            && args.group_by.is_empty()
            && args.offset == 0
            && args.limit.is_none()
            && !args.whole_source
            && !args.json_pointers
            && args.row_ids.is_none()
            && args.brush.is_none()
            && args.bins.is_none()
            && args.order_by.is_none()
            && args.conversions.is_empty()
            && args.dates.is_empty()
            && args.decimal_columns.is_empty(),
        "inspect accepts only path, format, action and retain_source; workbook-wide structural observation is separate from table calculation"
    );
    let bytes = source::read(ws, &args.path)?;
    ensure!(bytes.len() <= MAX_BYTES, "data source exceeds 8 MiB");
    let mut result = crate::tools::fs::documents::inspect_workbook_bytes(&bytes, cancel)
        .map_err(anyhow::Error::msg)?;
    let mut snapshot = source::snapshot(&bytes)?;
    if args.retain_source {
        snapshot.artifact = Some(
            ws.artifacts()
                .context("retain_source requires artifact storage")?
                .retain_exact_bytes(&bytes)?,
        );
    }
    result["source"] = json!({"path":args.path,"format":"xlsx","snapshot":snapshot});
    result["query"] = serde_json::to_value(args)?;
    result["action"] = json!("inspect");
    let rendered = serde_json::to_vec_pretty(&result)?;
    ensure!(
        rendered.len() <= MAX_RESULT,
        "workbook inspection exceeds 2 MiB; no partial result returned"
    );
    let item = Evidence {
        source:args.path.clone(), location:Location::Whole, method:Method::Calculation,
        producer:"dial/data-inspect-v1".into(), kind:EvidenceKind::Observation,
        summary:"Workbook structure and formula text; no execution or cached-value validation".into(),
        representation:Representation::of_bytes("application/json", &rendered),
        limitations:vec!["Formula text and cached lexical values only; never formula execution or verified results. Structural workbook observation is not a semantic calculation.".into()],
        context:EvidenceContext { snapshot:Some(snapshot), ..Default::default() },
    };
    let mut out = ToolOutput::structured(result);
    out.text = format!(
        "UNTRUSTED WORKBOOK INSPECTION:\n{}",
        String::from_utf8(rendered.clone())?
    );
    check_cancel(cancel)?;
    publish(ws, out, item, &rendered)
}
