//! Bounded reconstruction from retained event coordinates, without inferred synchrony or causation.
use super::evidence::publish;
use crate::config::types::Config;
use crate::runtime::evidence::{
    Clock, ClockAlignment, Evidence, EvidenceContext, EvidenceKind, Location, Method,
    Representation,
};
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::{ToolOutput, Workspace};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

const MAX_EVENTS: usize = 128;
const MAX_BYTES: usize = 256 * 1024;

/// Register retained cross-source reconstruction.
pub fn tools(_: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Timeline)]
}
struct Timeline;
impl Tool for Timeline {
    fn name(&self) -> &str {
        "timeline"
    }
    fn schema(&self) -> Value {
        let clock = json!({"type":"object","properties":{"id":{"type":"string","maxLength":256},"origin":{"type":"string","enum":["relative","unix"]},"timezone":{"type":"string","maxLength":256}},"required":["id","origin"],"additionalProperties":false});
        crate::tools::tool_schema(
            "timeline",
            "Reconstruct bounded retained audio segments, video requested seeks, time locations or exact quoted log entries. Never reopens sources. Explicit clocks and single-hop alignment bases are required; no inferred synchrony, measured PTS, semantic causation or verification. Groups are incomparable across clock IDs; sorted coordinates do not resolve overlapping uncertainty.",
            json!({
                "action":{"type":"string","enum":["reconstruct"]},
                "sources":{"type":"array","maxItems":16,"items":{"type":"object","properties":{"reference":{"type":"string"},"clock":clock.clone()},"required":["reference"],"additionalProperties":false}},
                "events":{"type":"array","maxItems":128,"items":{"type":"object","properties":{"reference":{"type":"string"},"clock":clock,"start_ms":{"type":"integer"},"end_ms":{"type":"integer"},"uncertainty_ms":{"type":"integer","minimum":0},"quote":{"type":"string","maxLength":4096}},"required":["reference","start_ms","end_ms","quote"],"additionalProperties":false}},
                "alignments":{"type":"array","maxItems":16,"items":{"type":"object","properties":{"from":{"type":"string"},"to":{"type":"string"},"offset_ms":{"type":"integer"},"uncertainty_ms":{"type":"integer","minimum":0},"basis":{"type":"array","minItems":1,"maxItems":64,"items":{"type":"string"}}},"required":["from","to","offset_ms","uncertainty_ms","basis"],"additionalProperties":false}}
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    reference: String,
    clock: Option<Clock>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Quoted {
    reference: String,
    clock: Option<Clock>,
    start_ms: i64,
    end_ms: i64,
    uncertainty_ms: Option<u64>,
    quote: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    #[serde(default)]
    sources: Vec<Source>,
    #[serde(default)]
    events: Vec<Quoted>,
    #[serde(default)]
    alignments: Vec<ClockAlignment>,
}
#[derive(Serialize)]
struct Event {
    id: usize,
    reference: String,
    selection: Value,
    clock: Clock,
    clock_attribution: String,
    start_ms: i64,
    end_ms: i64,
    uncertainty_ms: u64,
    uncertainty_known: bool,
    timing_basis: String,
    comparable_clock: String,
    aligned_start_ms: i64,
    aligned_end_ms: i64,
    earliest_ms: Option<i64>,
    latest_ms: Option<i64>,
}
fn clock(retained: Option<&Clock>, supplied: Option<Clock>) -> Result<(Clock, String)> {
    if let Some(retained) = retained {
        ensure!(
            supplied.as_ref().is_none_or(|c| c == retained),
            "caller clock conflicts with retained clock"
        );
        Ok((retained.clone(), "retained producer assertion".into()))
    } else {
        let c = supplied.context("source lacks a clock; supply an explicit clock ID and origin")?;
        c.validate()?;
        Ok((
            c,
            "caller assertion; origin and synchronization not verified".into(),
        ))
    }
}
fn add(
    events: &mut Vec<Event>,
    reference: &str,
    c: &(Clock, String),
    interval: (i64, i64),
    selection: Value,
    basis: &str,
    uncertainty: Option<u64>,
) -> Result<()> {
    let (start, end) = interval;
    ensure!(
        events.len() < MAX_EVENTS,
        "timeline exceeds 128 events; narrow inputs"
    );
    ensure!(end >= start, "event end precedes start");
    events.push(Event {
        id: events.len(),
        reference: reference.into(),
        selection,
        clock: c.0.clone(),
        clock_attribution: c.1.clone(),
        start_ms: start,
        end_ms: end,
        uncertainty_ms: uncertainty.unwrap_or(0),
        uncertainty_known: uncertainty.is_some(),
        timing_basis: basis.into(),
        comparable_clock: c.0.id.clone(),
        aligned_start_ms: start,
        aligned_end_ms: end,
        earliest_ms: None,
        latest_ms: None,
    });
    Ok(())
}
fn signed(v: &Value, name: &str) -> Result<i64> {
    v.as_i64()
        .with_context(|| format!("{name} must fit signed milliseconds"))
}

/// Compose retained selections; all temporal mappings remain attributed assertions.
pub fn run(ws: &Workspace, value: &Value) -> Result<ToolOutput> {
    ensure!(
        serde_json::to_vec(value)?.len() <= MAX_BYTES,
        "timeline arguments exceed 256 KiB"
    );
    let args: Args = serde_json::from_value(value.clone())?;
    ensure!(args.action == "reconstruct", "unknown timeline action");
    ensure!(
        args.sources.len() <= 16 && args.events.len() <= MAX_EVENTS && args.alignments.len() <= 16,
        "timeline input limit exceeded"
    );
    let store = ws
        .artifacts()
        .context("timeline requires an artifact store")?;
    let mut parents = BTreeSet::new();
    let mut events = Vec::new();
    let mut budget = MAX_BYTES;
    for source in args.sources {
        let record = store.evidence_record(&source.reference)?;
        let c = clock(record.evidence.context.clock.as_ref(), source.clock)?;
        parents.insert(source.reference.clone());
        let bytes = store.evidence_bytes(&record, budget)?;
        budget -= bytes.len();
        let data: Option<Value> = serde_json::from_slice(&bytes).ok();
        if let Some(segments) = data
            .as_ref()
            .and_then(|d| d["segments"].as_array())
            .filter(|_| record.evidence.producer == "dial/audio-whisper.cpp")
        {
            for (index, segment) in segments.iter().enumerate() {
                add(
                    &mut events,
                    &source.reference,
                    &c,
                    (
                        signed(&segment["start_ms"], "segment start")?,
                        signed(&segment["end_ms"], "segment end")?,
                    ),
                    json!({"segment":index,"retained_segment":segment,"source_kind":record.evidence.kind,"source_method":record.evidence.method}),
                    "backend estimated speech boundaries; uncertainty unknown",
                    None,
                )?;
            }
        } else if let Some(frames) = data
            .as_ref()
            .and_then(|d| d["frames"].as_array())
            .filter(|_| record.evidence.producer == "dial/video-ffmpeg")
        {
            for (index, frame) in frames.iter().enumerate() {
                let reference = frame["citation"]
                    .as_str()
                    .context("frame citation missing")?;
                store.evidence_record(reference)?;
                parents.insert(reference.into());
                let t = signed(&frame["requested_seek_ms"], "requested seek")?;
                add(
                    &mut events,
                    reference,
                    &c,
                    (t, t),
                    json!({"video":source.reference,"frame":index,"source_kind":record.evidence.kind,"source_method":record.evidence.method}),
                    "requested seek only; actual frame PTS and uncertainty unknown",
                    None,
                )?;
            }
        } else if let Location::Time { start_ms, end_ms } = record.evidence.location {
            add(
                &mut events,
                &source.reference,
                &c,
                (i64::try_from(start_ms)?, i64::try_from(end_ms)?),
                json!({"location":record.evidence.location,"source_kind":record.evidence.kind,"source_method":record.evidence.method}),
                "retained producer time location; uncertainty unknown",
                None,
            )?;
        } else {
            anyhow::bail!(
                "citation has no supported timed selection; supply an exact quoted event with explicit caller times"
            );
        }
    }
    for event in args.events {
        let record = store.evidence_record(&event.reference)?;
        ensure!(
            record
                .evidence
                .representation
                .media_type
                .starts_with("text/")
                || record.evidence.representation.media_type == "application/json",
            "quoted events require retained text/JSON"
        );
        let bytes = store.evidence_bytes(&record, budget)?;
        budget -= bytes.len();
        let text = std::str::from_utf8(&bytes)?;
        ensure!(
            !event.quote.is_empty() && event.quote.len() <= 4096 && text.contains(&event.quote),
            "event quote must occur verbatim in retained input and contain 1..4096 bytes"
        );
        let c = clock(record.evidence.context.clock.as_ref(), event.clock)?;
        parents.insert(event.reference.clone());
        add(
            &mut events,
            &event.reference,
            &c,
            (event.start_ms, event.end_ms),
            json!({"quote":event.quote,"source_kind":record.evidence.kind,"source_method":record.evidence.method}),
            "caller supplied times, not parsed or independently verified",
            event.uncertainty_ms,
        )?;
    }
    ensure!(!events.is_empty(), "supply at least one timed event");
    let mut clocks = BTreeMap::new();
    for event in &events {
        if let Some(prior) = clocks.insert(event.clock.id.clone(), event.clock.clone()) {
            ensure!(
                prior == event.clock,
                "same clock ID has conflicting origins/timezones"
            );
        }
    }
    let mut mappings = BTreeMap::new();
    for alignment in &args.alignments {
        alignment.validate()?;
        ensure!(
            !alignment.basis.is_empty(),
            "alignment requires retained basis citations"
        );
        ensure!(
            clocks.contains_key(&alignment.from) && clocks.contains_key(&alignment.to),
            "alignment endpoints must be declared event clocks"
        );
        ensure!(
            mappings.insert(alignment.from.clone(), alignment).is_none(),
            "ambiguous multiple mappings for one clock"
        );
        for basis in &alignment.basis {
            store.evidence_record(basis)?;
            parents.insert(basis.clone());
        }
    }
    for alignment in &args.alignments {
        ensure!(
            !mappings.contains_key(&alignment.to),
            "chained/cyclic alignment is unsupported; provide direct mappings to one target"
        );
    }
    for event in &mut events {
        if let Some(mapping) = mappings.get(&event.clock.id) {
            event.aligned_start_ms = mapping.aligned_time(event.start_ms)?.time_ms;
            event.aligned_end_ms = mapping.aligned_time(event.end_ms)?.time_ms;
            event.uncertainty_ms = event
                .uncertainty_ms
                .checked_add(mapping.uncertainty_ms)
                .context("combined uncertainty overflow")?;
            event.comparable_clock = mapping.to.clone();
        }
        let u = i64::try_from(event.uncertainty_ms)
            .context("uncertainty exceeds signed milliseconds")?;
        let earliest = event
            .aligned_start_ms
            .checked_sub(u)
            .context("uncertainty lower endpoint overflow")?;
        let latest = event
            .aligned_end_ms
            .checked_add(u)
            .context("uncertainty upper endpoint overflow")?;
        event.earliest_ms = event.uncertainty_known.then_some(earliest);
        event.latest_ms = event.uncertainty_known.then_some(latest);
    }
    ensure!(
        parents.len() <= 64,
        "timeline exceeds 64 dependency citations"
    );
    let mut groups: BTreeMap<String, Vec<&Event>> = BTreeMap::new();
    for event in &events {
        groups
            .entry(event.comparable_clock.clone())
            .or_default()
            .push(event);
    }
    let mut overlaps = Vec::new();
    for group in groups.values_mut() {
        group.sort_by_key(|e| (e.aligned_start_ms, e.id));
        for (i, a) in group.iter().enumerate() {
            for b in &group[i + 1..] {
                let overlap = match (a.earliest_ms, a.latest_ms, b.earliest_ms, b.latest_ms) {
                    (Some(a_start), Some(a_end), Some(b_start), Some(b_end)) => {
                        a_start <= b_end && b_start <= a_end
                    }
                    _ => true, // Unknown timing error cannot establish non-overlap.
                };
                if overlap {
                    overlaps.push(json!([a.id, b.id]));
                }
            }
        }
    }
    let value = json!({"type":"cross_source_timeline","groups":groups,"overlapping_or_ambiguous_pairs":overlaps,"alignments":args.alignments,"ordering":"Only within groups under declared clock assertions. Different groups incomparable; overlapping intervals have no established order. Unknown native timing error is not zero error; numeric uncertainty is only the supplied alignment/caller bound.","causation":"not established","verification_credit":false});
    let bytes = serde_json::to_vec(&value)?;
    ensure!(bytes.len() <= MAX_BYTES, "timeline record exceeds 256 KiB");
    let item = Evidence {source:"dial:timeline".into(),location:Location::Whole,method:Method::Calculation,producer:"dial/timeline-v1".into(),kind:EvidenceKind::Inference,summary:"Retained event coordinates grouped by explicitly declared clocks".into(),representation:Representation::of_bytes("application/json",&bytes),limitations:vec!["Clock IDs, origins, mappings and quoted times are assertions. Native timing uncertainty may be unknown. No causation, synchrony, approval or successful check is established.".into()],context:EvidenceContext{parents:parents.into_iter().collect(),alignments:args.alignments,..Default::default()}};
    let mut out = ToolOutput::structured(value);
    out.text = format!(
        "UNTRUSTED TIMELINE (not verification):\n{}",
        String::from_utf8(bytes.clone())?
    );
    publish(ws, out, item, &bytes)
}
#[cfg(test)]
mod tests;
