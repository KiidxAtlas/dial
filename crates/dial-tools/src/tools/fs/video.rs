//! Opt-in bounded video seeks with retained frame citations, never automatic whole-video sampling.
use super::audio;
use crate::config::types::Config;
use crate::llm::images;
use crate::runtime::evidence::{
    Coverage, CoverageExtent, Evidence, EvidenceContext, EvidenceKind, Location, Method,
    Representation,
};
use crate::tools::ToolOutput;
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::knowledge::evidence::{publish, source};
use crate::voice::process::{self, CommandSpec};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    if config.experimental.video_analysis {
        vec![Arc::new(Video)]
    } else {
        vec![]
    }
}
struct Video;
impl Tool for Video {
    fn name(&self) -> &str {
        "video"
    }
    fn schema(&self) -> Value {
        crate::tools::tool_schema(
            "video",
            "Experimental bounded video investigation, off by default. Supply an explicit <=120s interval and 1..8 requested seeks within it, plus an explicit frame_budget. No automatic sampling. Runs installed ffmpeg after confirmation on a copied authorized source (64 MiB maximum). First frame is model-visible; reopen other frame citations with evidence inspect as needed. Frames can be expensive image tokens; exact cost depends on provider/resolution, not duration alone. No motion/audio interpretation or event-completeness guarantee. Requested seek times are NOT measured frame PTS. Link audio/log evidence with parents, then use evidence align with an explicit basis to relate clocks. Use audio on the same source for timestamped speech. Shorten interval and choose new seeks to refine gaps.",
            json!({
                "path":{"type":"string"},"start_ms":{"type":"integer","minimum":0},"end_ms":{"type":"integer","minimum":1},
                "seeks_ms":{"type":"array","items":{"type":"integer","minimum":0},"minItems":1,"maxItems":8},
                "frame_budget":{"type":"integer","minimum":1,"maximum":8},"retain_source":{"type":"boolean"},
                "parents":{"type":"array","items":{"type":"string"},"maxItems":56}
            }),
            &["path", "start_ms", "end_ms", "seeks_ms", "frame_budget"],
        )
    }
    fn needs_approval(&self, _: &Value) -> bool {
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
        if !ctx.config.experimental.video_analysis {
            return ToolOutput::err(
                "video investigation is disabled; enable experimental.video_analysis explicitly (image tokens may be expensive)",
            );
        }
        serde_json::from_value(args.clone())
            .map_err(anyhow::Error::from)
            .and_then(|args| run(ctx, args, cancel))
            .unwrap_or_else(|e| ToolOutput::err(format!("{e:#}")))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    start_ms: u64,
    end_ms: u64,
    seeks_ms: Vec<u64>,
    frame_budget: usize,
    #[serde(default)]
    retain_source: bool,
    #[serde(default)]
    parents: Vec<String>,
}
fn validate(args: &Args) -> Result<()> {
    audio::interval(args.start_ms, args.end_ms)?;
    ensure!(
        (1..=8).contains(&args.frame_budget)
            && !args.seeks_ms.is_empty()
            && args.seeks_ms.len() <= args.frame_budget,
        "provide 1..=frame_budget seeks; budget must be 1..=8"
    );
    ensure!(
        args.seeks_ms
            .iter()
            .all(|t| *t >= args.start_ms && *t <= args.end_ms),
        "all seeks must be inside the explicit interval"
    );
    ensure!(
        args.seeks_ms.windows(2).all(|p| p[0] < p[1]),
        "seeks must be strictly increasing (no duplicate frame spend)"
    );
    Ok(())
}
fn run(ctx: &ToolContext, args: Args, cancel: &AtomicBool) -> Result<ToolOutput> {
    ensure!(
        ctx.config.experimental.video_analysis,
        "experimental.video_analysis is disabled"
    );
    validate(&args)?;
    ensure!(
        args.parents.len() <= 56,
        "at most 56 parent citations allowed"
    );
    let bytes = source::read(ctx.workspace, &args.path)?;
    let snapshot = audio::snapshot(ctx.workspace, &bytes, args.retain_source)?;
    let interval = Location::Time {
        start_ms: args.start_ms,
        end_ms: args.end_ms,
    };
    let base = Evidence {
        source: args.path.clone(), location: interval.clone(), method: Method::Sample,
        producer: "dial/video-ffmpeg".into(), kind: EvidenceKind::Observation,
        summary: "Bounded video frame extraction, not continuous observation".into(),
        representation: Representation::of_bytes("application/json", b""),
        limitations: vec!["Only requested seeks were sampled. Seek timestamps are requests, not measured frame presentation times; exact temporal alignment unavailable. Motion, audio, intervening events and all outside intervals are uninspected. Parent links do not establish clock synchronization. Extraction is not semantic interpretation or verification.".into()],
        context: EvidenceContext { snapshot: Some(snapshot), parents: args.parents.clone(), coverage: Some(Coverage { extent: CoverageExtent::Partial, inspected: vec![], omitted: vec![interval], truncated: false, failures: vec![] }), ..Default::default() },
    };
    base.validate()?;
    let converter = audio::ffmpeg()?;
    let store = ctx
        .workspace
        .artifacts()
        .context("video frame citations require an artifact store")?;
    audio::authorize(
        ctx,
        "video",
        &format!(
            "Run installed ffmpeg outside the shell sandbox for {} explicitly requested frames? First frame is attached; inspecting more consumes additional image tokens, exact provider cost unknown. No network is requested.",
            args.seeks_ms.len()
        ),
    )?;
    crate::tools::knowledge::perception::reserve(
        ctx,
        crate::tools::knowledge::perception::Spend {
            frames: args.seeks_ms.len() as u64,
            ..Default::default()
        },
    )?;
    let dir = tempfile::tempdir()?;
    let input = dir.path().join("source");
    std::fs::write(&input, bytes)?;
    let mut frames = Vec::new();
    let mut first = None;
    let mut failures = Vec::new();
    let mut frame_parents = Vec::new();
    for requested_ms in &args.seeks_ms {
        let spec = CommandSpec::new(&converter)
            .arg("-nostdin")
            .arg("-v")
            .arg("error")
            .arg("-format_whitelist")
            .arg("mov,matroska,webm,avi,wav,mp3,flac,ogg,aac,aiff")
            .arg("-protocol_whitelist")
            .arg("file,pipe")
            .arg("-ss")
            .arg(format!("{:.3}", *requested_ms as f64 / 1000.0))
            .arg("-i")
            .arg(&input)
            .arg("-frames:v")
            .arg("1")
            .arg("-an")
            .arg("-vf")
            .arg("scale=1024:1024:force_original_aspect_ratio=decrease")
            .arg("-f")
            .arg("image2pipe")
            .arg("-vcodec")
            .arg("png")
            .arg("pipe:1");
        let result = process::run(&spec, None, cancel, Duration::from_secs(30))
            .and_then(|done| images::fit_png(&done.stdout));
        match result {
            Ok(png) => {
                let mut item = base.clone();
                // Location is the requested seek, explicitly uncertain, never an asserted measured PTS.
                item.location = Location::Time {
                    start_ms: *requested_ms,
                    end_ms: *requested_ms,
                };
                item.summary =
                    format!("Frame at requested seek {requested_ms} ms; measured PTS unavailable");
                item.context.coverage.as_mut().unwrap().inspected = vec![item.location.clone()];
                item.representation = Representation::of_bytes("image/png", &png);
                let citation = store.retain_evidence(&item, &png)?;
                frame_parents.push(citation.clone());
                frames.push(json!({"requested_seek_ms":requested_ms,"actual_pts_ms":null,"citation":citation}));
                if first.is_none() {
                    first = Some(png);
                }
            }
            Err(error) => failures.push(format!(
                "requested seek {requested_ms} ms failed: {}",
                error.to_string().chars().take(512).collect::<String>()
            )),
        }
    }
    ensure!(
        !frames.is_empty(),
        "no frames extracted: {}",
        failures.join("; ")
    );
    let data = json!({"frames":frames,"failures":failures,"interval_ms":[args.start_ms,args.end_ms],"frame_budget":args.frame_budget,"requested_frames":args.seeks_ms.len(),"coverage":"partial discrete requested seeks; all gaps uninspected","cost":"first frame attached; reopen remaining citations explicitly; image token cost provider-dependent and not estimated","audio":"not transcribed; use audio with an explicit interval","alignment":"parent links only; use evidence align with measured basis for independent clocks"});
    let text = serde_json::to_string_pretty(&data)?;
    let mut item = base;
    item.context.parents.extend(frame_parents);
    item.context.coverage.as_mut().unwrap().inspected = frames
        .iter()
        .filter_map(|frame| frame["requested_seek_ms"].as_u64())
        .map(|time| Location::Time {
            start_ms: time,
            end_ms: time,
        })
        .collect();
    item.context.coverage.as_mut().unwrap().failures = failures;
    item.representation = Representation::of_bytes("application/json", text.as_bytes());
    let mut out = ToolOutput::ok(text.clone());
    out.data = Some(data);
    if let Some(png) = first {
        out.data.as_mut().unwrap()["artifact"] =
            json!({"encoding":"base64","media_type":"image/png","data":STANDARD.encode(png)});
    }
    publish(ctx.workspace, out, item, text.as_bytes())
}
#[cfg(test)]
mod tests;
