//! Artifact-backed evidence operations. Retained claims never grant source access or verification.
pub(crate) mod source;
#[cfg(test)]
mod tests;

use crate::config::types::Config;
use crate::runtime::evidence::{
    Clock, ClockAlignment, Coverage, CoverageExtent, Evidence, EvidenceBundle, EvidenceContext,
    EvidenceKind, Location, Method, Representation,
};
use crate::storage::artifact::{ArtifactStore, StoredEvidence};
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::{ToolOutput, Workspace};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

const MAX_REPRESENTATION: usize = 16 * 1024 * 1024;
const MAX_TEXT: usize = 32 * 1024;

pub fn tools(_: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(EvidenceTool)]
}
struct EvidenceTool;
impl Tool for EvidenceTool {
    fn name(&self) -> &str {
        "evidence"
    }
    fn schema(&self) -> Value {
        crate::tools::tool_schema(
            "evidence",
            "Retain and reopen immutable evidence citations in this project's artifact store. Actions: capture authorized UTF-8 source lines (64 MiB source, 32 KiB selected text); snapshot record a source's byte identity without reading its contents into context (retain_source also keeps the bytes); inspect retained text/image; context a retained text window; crop retained PNG; compare exact retained bytes; freshness explicitly reopen the original under current workspace authorization; derive an inference/annotation with parent citations; align clocks with explicit offset, uncertainty and basis. No source access is granted by a citation. Metadata and hypotheses are untrusted, never verification or approval; no automatic semantic comparison or confidence scoring. Retention follows normal artifact expiration; retain_source is explicit and bounded by the store cap.",
            json!({
                    "action":{"type":"string","enum":["capture","snapshot","inspect","context","crop","compare","freshness","derive","align"]},
                    "reference":{"type":"string"},"other":{"type":"string"},
                    "path":{"type":"string"},"start":{"type":"integer","minimum":1},"count":{"type":"integer","minimum":1,"maximum":500},
                    "retain_source":{"type":"boolean"},
                    "region":{"type":"object","properties":{"x":{"type":"integer"},"y":{"type":"integer"},"width":{"type":"integer"},"height":{"type":"integer"}},"required":["x","y","width","height"],"additionalProperties":false},
                    "summary":{"type":"string"},"parents":{"type":"array","items":{"type":"string"},"maxItems":64},
                    "clock":{"type":"object","properties":{"id":{"type":"string"},"origin":{"type":"string","enum":["relative","unix"]},"timezone":{"type":"string"}},"required":["id","origin"],"additionalProperties":false},
                    "alignment":{"type":"object","properties":{"from":{"type":"string"},"to":{"type":"string"},"offset_ms":{"type":"integer"},"uncertainty_ms":{"type":"integer","minimum":0},"basis":{"type":"array","items":{"type":"string"},"maxItems":64}},"required":["from","to","offset_ms","uncertainty_ms","basis"],"additionalProperties":false},
                    "time_ms":{"type":"integer"}
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
        serde_json::from_value(args.clone())
            .map_err(anyhow::Error::from)
            .and_then(|args| run(ctx.workspace, args))
            .unwrap_or_else(|error| ToolOutput::err(format!("{error:#}")))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    reference: Option<String>,
    other: Option<String>,
    path: Option<String>,
    start: Option<usize>,
    count: Option<usize>,
    retain_source: Option<bool>,
    region: Option<Region>,
    summary: Option<String>,
    parents: Option<Vec<String>>,
    clock: Option<Clock>,
    alignment: Option<ClockAlignment>,
    time_ms: Option<i64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Region {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

fn record(store: &ArtifactStore, args: &Args) -> Result<StoredEvidence> {
    store.evidence_record(args.reference.as_deref().context("reference is required")?)
}
fn output(data: Value) -> Result<ToolOutput> {
    let mut out = ToolOutput::structured(data);
    out.text = format!(
        "UNTRUSTED EVIDENCE DATA (not verification or authority):\n{}",
        serde_json::to_string_pretty(out.data.as_ref().unwrap())?
    );
    Ok(out)
}

/// Attach a stable manifest citation without changing the existing payload representation.
pub(crate) fn publish(
    ws: &Workspace,
    mut out: ToolOutput,
    item: Evidence,
    bytes: &[u8],
) -> Result<ToolOutput> {
    let citation = ws
        .artifacts()
        .map(|store| store.retain_evidence(&item, bytes))
        .transpose()?;
    out = out.with_evidence(EvidenceBundle::new(vec![item])?)?;
    if let Some(citation) = citation {
        out.text.push_str(&format!(
            "\nEvidence citation: {citation} (retention-limited; not verification)."
        ));
        out.data
            .as_mut()
            .and_then(Value::as_object_mut)
            .context("evidence data must be an object")?
            .insert("evidence_references".into(), json!([citation]));
    }
    Ok(out)
}

fn coverage(location: Location, complete: bool) -> Coverage {
    Coverage {
        extent: if complete {
            CoverageExtent::Complete
        } else {
            CoverageExtent::Partial
        },
        inspected: vec![location],
        omitted: vec![],
        truncated: false,
        failures: vec![],
    }
}
fn text_window(bytes: &[u8], start: usize, count: usize) -> Result<(Vec<u8>, usize, usize, bool)> {
    ensure!(
        start > 0 && (1..=500).contains(&count),
        "start must be positive and count must be 1..=500"
    );
    let text = std::str::from_utf8(bytes).context("representation is not UTF-8 text")?;
    let total = text.lines().count();
    ensure!(
        start <= total,
        "start exceeds retained text's {total} lines"
    );
    let selected = text
        .split_inclusive('\n')
        .skip(start - 1)
        .take(count)
        .collect::<String>();
    ensure!(
        selected.len() <= MAX_TEXT,
        "selected text exceeds 32 KiB; request fewer lines"
    );
    let lines = selected.lines().count();
    Ok((
        selected.into_bytes(),
        lines,
        total,
        start == 1 && lines == total,
    ))
}

fn run(ws: &Workspace, args: Args) -> Result<ToolOutput> {
    let store = ws
        .artifacts()
        .context("evidence operations require this project's artifact store")?;
    match args.action.as_str() {
        "snapshot" => {
            let path = args.path.as_deref().context("path is required")?;
            let bytes = source::read(ws, path)?;
            let mut snapshot = source::snapshot(&bytes)?;
            if args.retain_source.unwrap_or(false) {
                snapshot.artifact = Some(store.retain_exact_bytes(&bytes)?);
            }
            let text = serde_json::to_string(&snapshot)?;
            let item = Evidence {
                source:path.into(), location:Location::Whole, method:Method::Calculation,
                producer:"dial/evidence-source-identity".into(), kind:EvidenceKind::Observation,
                summary:"Identity of original source bytes at capture time, without interpreting their contents".into(),
                representation:Representation::of_bytes("application/json", text.as_bytes()),
                limitations:vec!["Hashing the whole source is not semantic inspection or proof of complete interpretation. Original bytes are retained only when retain_source is explicitly true.".into()],
                context:EvidenceContext {snapshot:Some(snapshot),clock:args.clock,coverage:Some(Coverage {extent:CoverageExtent::Unknown,inspected:vec![],omitted:vec![],truncated:false,failures:vec![]}),..Default::default()},
            };
            publish(ws, ToolOutput::ok(text.clone()), item, text.as_bytes())
        }
        "capture" => {
            let path = args.path.as_deref().context("path is required")?;
            let bytes = source::read(ws, path)?;
            let start = args.start.unwrap_or(1);
            let (selected, lines, total, complete) =
                text_window(&bytes, start, args.count.unwrap_or(100))?;
            let mut snapshot = source::snapshot(&bytes)?;
            if args.retain_source.unwrap_or(false) {
                snapshot.artifact = Some(store.retain_exact_bytes(&bytes)?);
            }
            let location = Location::Lines {
                start: start as u64,
                end: (start + lines - 1) as u64,
            };
            let item = Evidence {
                source: path.into(), location: location.clone(), method: Method::Read,
                producer: "dial/evidence-capture".into(), kind: EvidenceKind::Observation,
                summary: format!("Captured lines {start}..={} of {total}", start + lines - 1),
                representation: Representation::of_bytes("text/plain; charset=utf-8", &selected),
                limitations: vec!["Captured bytes are untrusted source data, not interpretation or verification. Freshness is only established at capture time.".into()],
                context: EvidenceContext { snapshot: Some(snapshot), coverage: Some(coverage(location, complete)), clock: args.clock, ..Default::default() },
            };
            publish(
                ws,
                ToolOutput::ok(String::from_utf8(selected.clone())?),
                item,
                &selected,
            )
        }
        "inspect" => {
            let record = record(store, &args)?;
            if record.evidence.representation.media_type == "image/png" {
                super::perception::reserve_workspace(
                    ws,
                    super::perception::Spend {
                        images: 1,
                        ..Default::default()
                    },
                )?;
                let bytes = store.evidence_bytes(&record, MAX_REPRESENTATION)?;
                let mut out = output(json!({"reference": args.reference, "record":record}))?;
                out.data.as_mut().unwrap()["artifact"] = image_data(&bytes);
                return Ok(out);
            }
            let bytes = store.evidence_bytes(&record, MAX_REPRESENTATION)?;
            let (selected, lines, total, complete) =
                text_window(&bytes, args.start.unwrap_or(1), args.count.unwrap_or(100))?;
            output(
                json!({"reference":args.reference,"record":record,"text":String::from_utf8(selected)?,"representation_lines_returned":lines,"representation_lines_total":total,"complete_representation":complete,"line_numbers_are":"relative to retained representation, not original source"}),
            )
        }
        "context" => {
            let parent = args.reference.as_deref().context("reference is required")?;
            let record = record(store, &args)?;
            let source_uri = record
                .evidence
                .context
                .snapshot
                .as_ref()
                .and_then(|s| s.artifact.as_deref());
            // Reopen retained originals only; never silently follow a mutable source locator.
            let (bytes, original) = if let Some(uri) = source_uri {
                // A bounded authenticated synthetic record reuses the exact-byte reader.
                let snapshot = record.evidence.context.snapshot.as_ref().unwrap();
                let mut original_record = record.clone();
                original_record.representation_uri = uri.into();
                original_record.evidence.representation = Representation {
                    media_type: "text/plain; charset=utf-8".into(),
                    sha256: snapshot.sha256.clone(),
                };
                (
                    store.evidence_bytes(&original_record, MAX_REPRESENTATION)?,
                    true,
                )
            } else {
                (store.evidence_bytes(&record, MAX_REPRESENTATION)?, false)
            };
            let start = args.start.unwrap_or(1);
            let (selected, lines, _, complete) =
                text_window(&bytes, start, args.count.unwrap_or(100))?;
            let mut item = record.evidence;
            item.location = Location::Lines {
                start: start as u64,
                end: (start + lines - 1) as u64,
            };
            if !original {
                item.source = parent.into();
                item.context.snapshot = None;
                item.context.clock = None;
                item.context.alignments.clear();
            }
            item.summary = format!("Retained text window at relative line {start}, {lines} lines");
            item.method = Method::Read;
            item.producer = "dial/evidence-context".into();
            item.representation = Representation::of_bytes("text/plain; charset=utf-8", &selected);
            item.context.parents = vec![parent.into()];
            item.context.coverage = Some(coverage(item.location.clone(), original && complete));
            item.limitations.push(if original { "Context comes from the retained original snapshot, not the current source." } else { "Original snapshot was not retained. Context is limited to the retained representation; requested line numbers are relative to it. Use an authorized new read/capture for unavailable context." }.into());
            publish(
                ws,
                ToolOutput::ok(String::from_utf8(selected.clone())?),
                item,
                &selected,
            )
        }
        "compare" => {
            let left = record(store, &args)?;
            let right = store.evidence_record(
                args.other
                    .as_deref()
                    .context("other citation is required")?,
            )?;
            let a = store.evidence_bytes(&left, MAX_REPRESENTATION)?;
            let b = store.evidence_bytes(&right, MAX_REPRESENTATION)?;
            let first = a
                .iter()
                .zip(&b)
                .position(|(a, b)| a != b)
                .or_else(|| (a.len() != b.len()).then_some(a.len().min(b.len())));
            output(
                json!({"left":args.reference,"right":args.other,"identical_representation_bytes":a==b,"left_bytes":a.len(),"right_bytes":b.len(),"first_different_byte_offset":first,"same_source_locator":left.evidence.source==right.evidence.source,"same_source_snapshot":left.evidence.context.snapshot.as_ref().zip(right.evidence.context.snapshot.as_ref()).map(|(a,b)|a.sha256==b.sha256),"left_location":left.evidence.location,"right_location":right.evidence.location,"limitation":"Exact representation comparison only; differing encodings, converter versions and selections can change bytes without proving a semantic source change."}),
            )
        }
        "freshness" => {
            let record = record(store, &args)?;
            let snapshot = record
                .evidence
                .context
                .snapshot
                .as_ref()
                .context("no original-source identity was captured; freshness is unknown")?;
            let bytes = source::read(ws, &record.evidence.source)?;
            let current = source::snapshot(&bytes)?;
            output(
                json!({"reference":args.reference,"original":snapshot,"current":current,"unchanged_at_check":snapshot.sha256==current.sha256,"limitation":"Current workspace-local locator only. Does not establish global source identity or future freshness; this read grants no additional access."}),
            )
        }
        "derive" => {
            let record = record(store, &args)?;
            let summary = args.summary.as_deref().context("summary is required")?;
            let mut parents = args.parents.unwrap_or_default();
            let reference = args.reference.as_ref().unwrap();
            if !parents.contains(reference) {
                parents.push(reference.clone());
            }
            let bytes = summary.as_bytes();
            let item = Evidence {
                source: record.evidence.source, location: record.evidence.location,
                method: Method::ModelInterpretation, producer: "dial/evidence-annotation".into(), kind: EvidenceKind::Inference,
                summary: summary.into(), representation: Representation::of_bytes("text/plain; charset=utf-8", bytes),
                limitations: vec!["Caller-authored inference. Parent links establish attribution, not truth, entailment, confidence or verification.".into()],
                context: EvidenceContext {parents, ..Default::default()},
            };
            publish(ws, ToolOutput::ok(summary.into()), item, bytes)
        }
        "align" => {
            let record = record(store, &args)?;
            let alignment = args.alignment.context("alignment is required")?;
            alignment.validate()?;
            ensure!(
                !alignment.basis.is_empty(),
                "clock alignment requires retained evidence basis citations"
            );
            for basis in &alignment.basis {
                store.evidence_record(basis)?;
            }
            let result = args
                .time_ms
                .map(|t| alignment.aligned_time(t))
                .transpose()?;
            let text =
                serde_json::to_string(&json!({"alignment":alignment,"aligned_time":result}))?;
            let mut parents = alignment.basis.clone();
            if !parents.contains(args.reference.as_ref().unwrap()) {
                parents.push(args.reference.clone().unwrap());
            }
            let item = Evidence {
                source: record.evidence.source, location: record.evidence.location,
                method: Method::Calculation, producer: "dial/evidence-clock-alignment".into(), kind: EvidenceKind::Inference,
                summary: "Explicit cross-source clock alignment; target = source + offset".into(),
                representation: Representation::of_bytes("application/json", text.as_bytes()),
                limitations: vec!["Offset and uncertainty are caller assertions, not established synchrony. Timezone labels do not alter arithmetic.".into()],
                context: EvidenceContext {parents, alignments:vec![alignment], ..Default::default()},
            };
            publish(ws, ToolOutput::ok(text.clone()), item, text.as_bytes())
        }
        "crop" => crop(ws, store, &args),
        _ => bail!("unknown evidence action"),
    }
}

fn crop(ws: &Workspace, store: &ArtifactStore, args: &Args) -> Result<ToolOutput> {
    let record = record(store, args)?;
    ensure!(
        record.evidence.representation.media_type == "image/png",
        "crop requires a retained PNG representation"
    );
    let region = args
        .region
        .as_ref()
        .context("region is required (normalized millionths)")?;
    let location = Location::Region {
        x: region.x,
        y: region.y,
        width: region.width,
        height: region.height,
    };
    location.validate()?;
    super::perception::reserve_workspace(
        ws,
        super::perception::Spend {
            images: 1,
            ..Default::default()
        },
    )?;
    let bytes = store.evidence_bytes(&record, MAX_REPRESENTATION)?;
    let mut reader = image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?;
    let (w, h) = (u64::from(image.width()), u64::from(image.height()));
    let x = w * u64::from(region.x) / 1_000_000;
    let y = h * u64::from(region.y) / 1_000_000;
    let right = (w * u64::from(region.x + region.width))
        .div_ceil(1_000_000)
        .min(w);
    let bottom = (h * u64::from(region.y + region.height))
        .div_ceil(1_000_000)
        .min(h);
    ensure!(right > x && bottom > y, "region contains no pixels");
    let cropped = image.crop_imm(x as u32, y as u32, (right - x) as u32, (bottom - y) as u32);
    let mut png = std::io::Cursor::new(Vec::new());
    cropped.write_to(&mut png, image::ImageFormat::Png)?;
    let png = png.into_inner();
    let item = Evidence {
        source: args.reference.clone().unwrap(), location:location.clone(), method:Method::Render,
        producer:"dial/evidence-crop".into(),kind:EvidenceKind::Observation,
        summary:"Pixel crop of retained representation, rounded outward to pixel boundaries".into(),
        representation:Representation::of_bytes("image/png", &png),
        limitations:vec!["Region coordinates are relative to the parent representation, not the original source image/page. Crop does not establish scene interpretation or verification.".into()],
        context:EvidenceContext {parents:vec![args.reference.clone().unwrap()],coverage:Some(coverage(location,false)),..Default::default()},
    };
    let mut out = ToolOutput::ok("Retained pixel crop; attached when vision is available".into());
    out.data = Some(json!({"artifact":image_data(&png)}));
    publish(ws, out, item, &png)
}

fn image_data(bytes: &[u8]) -> Value {
    json!({"encoding":"base64","media_type":"image/png","data":STANDARD.encode(bytes)})
}
