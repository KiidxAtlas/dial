//! Approved, bounded file-audio clips and timestamped transcription evidence.
use crate::config::types::Config;
use crate::runtime::evidence::{
    Coverage, CoverageExtent, Evidence, EvidenceContext, EvidenceKind, Location, Method,
    Representation,
};
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::knowledge::evidence::{publish, source};
use crate::tools::{ToolOutput, Workspace};
use crate::voice::{
    VoiceEnv,
    process::{self, CommandSpec},
    stt,
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

mod signal;

pub fn tools(_: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Audio)]
}
struct Audio;
impl Tool for Audio {
    fn name(&self) -> &str {
        "audio"
    }
    fn schema(&self) -> Value {
        crate::tools::tool_schema(
            "audio",
            "Interpret an explicit file audio interval (maximum 120 seconds, 64 MiB source). analyze measures channel-preserving decoded PCM peak/RMS, threshold clipping/silence and approximate energy events, not sound identity or diarization; window_ms defaults to 1000 (maximum 256 windows). transcribe/refine use installed whisper.cpp timestamps. replay extracts a retained downmixed WAV clip, not device playback. External processes require confirmation. retain_source keeps original bytes. No installs or automatic whole-file transcription.",
            json!({
                "action":{"type":"string","enum":["transcribe","refine","replay","analyze"]},
                "path":{"type":"string"},"start_ms":{"type":"integer","minimum":0},"end_ms":{"type":"integer","minimum":1},
                "retain_source":{"type":"boolean"},"parents":{"type":"array","items":{"type":"string"},"maxItems":63},
                "window_ms":{"type":"integer","minimum":100,"maximum":10000},
                "silence_dbfs":{"type":"number","minimum":-120,"maximum":0},
                "clip_threshold":{"type":"number","exclusiveMinimum":0,"maximum":1},
                "transient_db":{"type":"number","minimum":0,"maximum":120}
            }),
            &["action", "path", "start_ms", "end_ms"],
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
        execute(ctx, args, cancel)
    }
}

pub(crate) fn execute(ctx: &ToolContext, args: &Value, cancel: &AtomicBool) -> ToolOutput {
    serde_json::from_value(args.clone())
        .map_err(anyhow::Error::from)
        .and_then(|a| run(ctx, a, cancel))
        .unwrap_or_else(|e| ToolOutput::err(format!("{e:#}")))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    path: String,
    start_ms: u64,
    end_ms: u64,
    #[serde(default)]
    retain_source: bool,
    #[serde(default)]
    parents: Vec<String>,
    #[serde(default)]
    window_ms: Option<u64>,
    #[serde(default)]
    silence_dbfs: Option<f64>,
    #[serde(default)]
    clip_threshold: Option<f64>,
    #[serde(default)]
    transient_db: Option<f64>,
}
pub(super) fn interval(start: u64, end: u64) -> Result<()> {
    ensure!(
        end > start && end - start <= 120_000 && end <= 86_400_000,
        "request an explicit increasing interval of at most 120 seconds within the first 24 hours"
    );
    Ok(())
}
pub(super) fn authorize(ctx: &ToolContext, name: &str, summary: &str) -> Result<()> {
    ensure!(
        ctx.confirm.is_some_and(|confirm| confirm(summary)),
        "external media processes require explicit user confirmation"
    );
    ctx.authorize_mutation(name, &[], true)
        .map_err(anyhow::Error::msg)
}
pub(super) fn snapshot(
    ws: &Workspace,
    bytes: &[u8],
    retain: bool,
) -> Result<crate::runtime::evidence::SourceSnapshot> {
    let mut snapshot = source::snapshot(bytes)?;
    if retain {
        snapshot.artifact = Some(
            ws.artifacts()
                .context("retaining source requires an artifact store")?
                .retain_exact_bytes(bytes)?,
        );
    }
    Ok(snapshot)
}
pub(super) fn ffmpeg() -> Result<std::path::PathBuf> {
    process::which(Path::new("ffmpeg")).context(
        "ffmpeg is not installed; configure an installed converter (no automatic installation)",
    )
}
fn run(ctx: &ToolContext, mut args: Args, cancel: &AtomicBool) -> Result<ToolOutput> {
    ensure!(
        !cancel.load(std::sync::atomic::Ordering::Acquire),
        "audio investigation cancelled"
    );
    interval(args.start_ms, args.end_ms)?;
    ensure!(
        args.parents.len() <= 63,
        "at most 63 parent citations allowed"
    );
    ensure!(
        ["transcribe", "refine", "replay", "analyze"].contains(&args.action.as_str()),
        "unknown audio action"
    );
    let params = signal::Params::from_args(&args)?;
    let env = VoiceEnv::from_config(ctx.config);
    let backend = if matches!(args.action.as_str(), "transcribe" | "refine") {
        let backend = stt::resolve_backend(&env)?;
        ensure!(
            matches!(backend, stt::SttBackend::WhisperCpp { .. }),
            "timestamped file transcription currently requires whisper.cpp; selected backend has no timeline adapter"
        );
        Some(backend)
    } else {
        None
    };
    authorize(
        ctx,
        "audio",
        "Run installed ffmpeg and (for transcription) whisper.cpp outside the shell sandbox on a copied, bounded source? No network transcription is performed.",
    )?;
    crate::tools::knowledge::perception::reserve(
        ctx,
        crate::tools::knowledge::perception::Spend {
            audio_ms: args.end_ms - args.start_ms,
            ..Default::default()
        },
    )?;
    let converter = ffmpeg()?;
    let bytes = source::read(ctx.workspace, &args.path)?;
    let snapshot = snapshot(ctx.workspace, &bytes, args.retain_source)?;
    let location = Location::Time {
        start_ms: args.start_ms,
        end_ms: args.end_ms,
    };
    let mut item = Evidence {
        source: args.path.clone(), location: location.clone(), method: Method::Transcription,
        producer: "dial/audio-whisper.cpp".into(), kind: EvidenceKind::Observation,
        summary: format!("Audio {} interval {}..{} ms", args.action, args.start_ms, args.end_ms),
        representation: Representation::of_bytes("application/json", b""),
        limitations: vec!["Speech timestamps are backend estimates, not verified word alignment. Speaker labels unavailable; transcription may hallucinate or omit speech. Intervals outside the selected clip are uninspected. Parent links alone do not align independent clocks.".into()],
        context: EvidenceContext { snapshot: Some(snapshot), parents: std::mem::take(&mut args.parents), coverage: Some(Coverage { extent: CoverageExtent::Partial, inspected: vec![location], omitted: vec![], truncated: false, failures: vec![] }), ..Default::default() },
    };
    item.validate()?;
    if args.action == "analyze" {
        return signal::run(ctx, &args, params, &bytes, item, cancel);
    }
    let dir = tempfile::tempdir()?;
    let input = dir.path().join("source");
    let clip = dir.path().join("clip.wav");
    std::fs::write(&input, bytes)?;
    let spec = CommandSpec::new(converter)
        .arg("-nostdin")
        .arg("-v")
        .arg("error")
        .arg("-format_whitelist")
        .arg("mov,matroska,webm,avi,wav,mp3,flac,ogg,aac,aiff")
        .arg("-protocol_whitelist")
        .arg("file,pipe")
        .arg("-i")
        .arg(&input)
        .arg("-ss")
        .arg(format!("{:.3}", args.start_ms as f64 / 1000.0))
        .arg("-t")
        .arg(format!(
            "{:.3}",
            (args.end_ms - args.start_ms) as f64 / 1000.0
        ))
        .arg("-vn")
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg("16000")
        .arg("-c:a")
        .arg("pcm_s16le")
        .arg("-f")
        .arg("s16le")
        .arg("pipe:1");
    let pcm = process::run(&spec, None, cancel, Duration::from_secs(60))?.stdout;
    ensure!(
        !pcm.is_empty() && pcm.len() <= 3_840_000 && pcm.len() % 2 == 0,
        "converter returned empty or oversized audio clip"
    );
    let duration_ms = pcm.len() as u64 / 32;
    ensure!(
        duration_ms > 0 && duration_ms <= args.end_ms - args.start_ms,
        "decoded clip duration outside requested interval"
    );
    let actual_end_ms = args.start_ms + duration_ms;
    let mut samples = Vec::new();
    crate::voice::wav::pcm16le_into(&pcm, &mut samples);
    let clip_bytes = crate::voice::wav::encode_pcm16(&samples, 16_000);
    std::fs::write(&clip, &clip_bytes)?;
    item.location = Location::Time {
        start_ms: args.start_ms,
        end_ms: actual_end_ms,
    };
    item.context.coverage.as_mut().unwrap().inspected = vec![item.location.clone()];
    if actual_end_ms < args.end_ms {
        item.context.coverage.as_mut().unwrap().omitted = vec![Location::Time {
            start_ms: actual_end_ms,
            end_ms: args.end_ms,
        }];
    }
    let data = if let Some(backend) = backend {
        let mut segments = stt::transcribe_timeline(&env, &backend, &clip, duration_ms, cancel)?;
        for segment in &mut segments {
            segment.start_ms += args.start_ms;
            segment.end_ms += args.start_ms;
        }
        json!({"segments":segments,"speaker_labels":"unavailable","source_interval_ms":[args.start_ms,actual_end_ms],"requested_interval_ms":[args.start_ms,args.end_ms],"alignment":"source-relative milliseconds; approximate backend speech boundaries"})
    } else {
        item.method = Method::Render;
        item.producer = "dial/audio-ffmpeg".into();
        let store = ctx
            .workspace
            .artifacts()
            .context("replay requires artifact retention")?;
        let mut clip_item = item.clone();
        clip_item.representation = Representation::of_bytes("audio/wav", &clip_bytes);
        let reference = store.retain_evidence(&clip_item, &clip_bytes)?;
        item.context.parents.push(reference.clone());
        json!({"clip":reference,"media_type":"audio/wav","source_interval_ms":[args.start_ms,actual_end_ms],"requested_interval_ms":[args.start_ms,args.end_ms],"playback":"extracted only; no device playback performed"})
    };
    let text = serde_json::to_string_pretty(&data)?;
    ensure!(
        text.len() <= 64 * 1024,
        "transcript exceeds 64 KiB; refine a shorter interval"
    );
    item.representation = Representation::of_bytes("application/json", text.as_bytes());
    let mut out = ToolOutput::ok(text.clone());
    out.data = Some(data);
    publish(ctx.workspace, out, item, text.as_bytes())
}
#[cfg(test)]
mod tests;
