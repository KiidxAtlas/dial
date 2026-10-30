//! Visual media reads that yield a model-visible PNG: SVG rasters, HEIC conversions, PDF page
//! renders and video metadata, preview grids and frames. Pixels travel on the structured
//! `data.artifact` channel (captured like screenshots); the text stays a short description.
//! External tools are detected at runtime and missing ones fail with an actionable error.
use crate::llm::{images, policy};
use crate::runtime::evidence::{
    Coverage, CoverageExtent, Evidence, EvidenceBundle, EvidenceContext, EvidenceKind, Location,
    Method, Representation,
};
use crate::runtime::tool_output::ToolOutput;
use crate::tools::Workspace;
use crate::tools::integrations::ext::ToolContext;
use crate::tools::knowledge::evidence::source;
use crate::voice::process::{CommandSpec, run, which};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use resvg::{tiny_skia, usvg};
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "m4v", "mov", "webm", "mkv", "avi"];
const FFMPEG_HINT: &str =
    "video reads need ffmpeg and ffprobe (install with `brew install ffmpeg`)";
/// Smallest longest edge for a rasterized SVG, so icons stay legible.
const MIN_SVG_EDGE: f32 = 1024.0;
const PREVIEW_FRAMES: u32 = 9;

/// Conservative approval classifier; execution expands handles before media detection.
pub(in crate::tools) fn needs_approval(path: &str) -> bool {
    let (file, selector) = split(path);
    let ext = Path::new(file)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    let ext = ext.to_ascii_lowercase();
    VIDEO_EXTENSIONS.contains(&ext.as_str())
        || matches!(ext.as_str(), "heic" | "heif")
        || (ext == "pdf" && pdf_page(selector).is_some())
}

/// Intercepts media targets before the generic file reader. `None` means the target is not a
/// media request and ordinary text or image handling applies.
pub(crate) fn read(
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
    context: Option<&ToolContext<'_>>,
) -> Option<Result<ToolOutput, String>> {
    let raw = args.get("path")?.as_str()?;
    let expanded = match ws.expand_file_handle(raw) {
        Ok(path) => path,
        Err(error) => return Some(Err(error)),
    };
    let path_text = expanded.as_str();
    if crate::tools::net::web::read::is_url(path_text) {
        return None;
    }
    // A literal file that exists wins over selector syntax.
    let (file, selector) = if ws.resolve(path_text).is_ok_and(|p| p.is_file()) {
        (path_text, "")
    } else {
        split(path_text)
    };
    let ext = Path::new(file).extension()?.to_str()?.to_ascii_lowercase();
    let external = match ext.as_str() {
        "pdf" => {
            pdf_page(selector)?;
            true
        }
        "heic" | "heif" => true,
        ext if VIDEO_EXTENSIONS.contains(&ext) => true,
        "svg" if selector == "img" => false,
        _ => return None,
    };
    let result = (|| {
        if !external {
            return svg(ws, file);
        }
        let context = context.context("external media reads require explicit confirmation and execution-controller authorization")?;
        super::audio::authorize(
            context,
            "read",
            "Run installed local media converters outside the shell sandbox on a copied, bounded authorized source? No installation or network requested.",
        )?;
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "media read cancelled"
        );
        if ext == "pdf" {
            return render_pdf_page(
                ws,
                file,
                pdf_page(selector).context("invalid PDF page")?,
                cancel,
            );
        }
        let bytes = source::read(ws, file)?;
        let snapshot = source::snapshot(&bytes)?;
        let dir = tempfile::tempdir().context("creating copied media source")?;
        let input = dir.path().join(format!("source.{ext}"));
        fs::write(&input, bytes)?;
        let output = match ext.as_str() {
            "heic" | "heif" if selector.is_empty() => heic(&input, file, cancel),
            "heic" | "heif" => Err(anyhow!("HEIC images take no selector")),
            _ => video(&input, file, selector, cancel),
        }?;
        with_snapshot(output, snapshot)
    })();
    Some(
        result
            .and_then(|output| retain(ws, output))
            .map_err(|error| format!("{error:#}")),
    )
}

fn with_snapshot(
    mut output: ToolOutput,
    snapshot: crate::runtime::evidence::SourceSnapshot,
) -> Result<ToolOutput> {
    let mut bundle = output.evidence()?.context("media evidence is missing")?;
    for item in &mut bundle.items {
        item.context.snapshot = Some(snapshot.clone());
    }
    bundle.validate()?;
    let data = output.data.as_mut().context("media data is missing")?;
    data["evidence"] = serde_json::to_value(bundle)?;
    Ok(output)
}

/// Retain the exact fitted representation; never reread or silently retain the original source.
fn retain(ws: &Workspace, mut output: ToolOutput) -> Result<ToolOutput> {
    let Some(store) = ws.artifacts() else {
        return Ok(output);
    };
    let bundle = output.evidence()?.context("media evidence is missing")?;
    let encoded = output
        .data
        .as_ref()
        .and_then(|data| data.pointer("/artifact/data"))
        .and_then(Value::as_str)
        .context("media PNG is missing")?;
    let png = STANDARD.decode(encoded)?;
    let citations = bundle
        .items
        .iter()
        .map(|item| store.retain_evidence(item, &png))
        .collect::<Result<Vec<_>>>()?;
    output.text.push_str(&format!(
        "\nEvidence citation: {} (retention-limited; not verification).",
        citations.join(", ")
    ));
    output.data.as_mut().unwrap()["evidence_references"] = json!(citations);
    Ok(output)
}

/// Splits `file.ext:selector` at the first colon after the last dot, so clock selectors such as
/// `clip.mp4:01:23` keep their own colons.
fn split(path: &str) -> (&str, &str) {
    let Some(dot) = path.rfind('.') else {
        return (path, "");
    };
    match path[dot..].find(':') {
        Some(colon) => (&path[..dot + colon], &path[dot + colon + 1..]),
        None => (path, ""),
    }
}

/// Returns the page number for a `page=N:img` selector.
fn pdf_page(selector: &str) -> Option<u32> {
    selector
        .strip_prefix("page=")?
        .strip_suffix(":img")?
        .parse::<u32>()
        .ok()
        .filter(|page| (1..=10_000).contains(page))
}

fn resolve(ws: &Workspace, file: &str) -> Result<PathBuf> {
    let path = ws.resolve(file).map_err(anyhow::Error::msg)?;
    ensure!(path.is_file(), "{file} is not a regular file");
    Ok(path)
}

fn read_source(path: &Path) -> Result<Vec<u8>> {
    let meta = fs::metadata(path).context("reading media")?;
    ensure!(
        meta.is_file() && meta.len() <= images::MAX_IMAGE_SOURCE_BYTES,
        "media must be a regular file no larger than 64 MiB"
    );
    fs::read(path).context("reading media")
}

fn tool(name: &str, hint: &str) -> Result<PathBuf> {
    which(Path::new(name)).with_context(|| format!("{name} is not installed; {hint}"))
}

fn bounded_output(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "converter output must be a regular file"
    );
    ensure!(
        metadata.len() <= images::MAX_IMAGE_SOURCE_BYTES,
        "converter output exceeds 64 MiB"
    );
    let mut bytes = Vec::new();
    file.take(images::MAX_IMAGE_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= images::MAX_IMAGE_SOURCE_BYTES,
        "converter output exceeds 64 MiB"
    );
    Ok(bytes)
}

struct Origin<'a> {
    source: &'a str,
    location: Location,
    method: Method,
    producer: &'static str,
    limitations: &'static [&'static str],
}

/// Keep provenance beside the existing artifact, hashing the fitted PNG actually returned.
fn derived(mut text: String, png: &[u8], origin: Origin<'_>) -> Result<ToolOutput> {
    ensure!(
        png.starts_with(b"\x89PNG\r\n\x1a\n"),
        "media converter did not return a PNG"
    );
    images::dimensions(png).context("invalid or oversized derived PNG header")?;
    let png = images::fit_png(png)?;
    let mut limitations = vec![
        "Returned PNG digest identifies the derived representation; source snapshot, when present, identifies captured input bytes.".into(),
        "Extraction is not visual interpretation or verification of the depicted behavior.".into(),
    ];
    limitations.extend(origin.limitations.iter().map(|value| (*value).to_owned()));
    text.push_str("\n[Evidence limitations: ");
    text.push_str(&limitations.join(" "));
    text.push(']');
    let evidence = EvidenceBundle::new(vec![Evidence {
        source: origin.source.into(),
        location: origin.location.clone(),
        method: origin.method,
        producer: origin.producer.into(),
        kind: EvidenceKind::Observation,
        summary: text.clone(),
        representation: Representation::of_bytes("image/png", &png),
        limitations,
        context: EvidenceContext {
            coverage: Some(Coverage {
                extent: CoverageExtent::Partial,
                inspected: if origin.method == Method::Sample && origin.location == Location::Whole
                {
                    vec![]
                } else {
                    vec![origin.location]
                },
                omitted: vec![],
                truncated: false,
                failures: vec![],
            }),
            ..Default::default()
        },
    }])?;
    let mut output = ToolOutput::ok(text);
    output.data = Some(json!({
        "artifact": {
            "encoding": "base64",
            "media_type": "image/png",
            "data": STANDARD.encode(&png),
        }
    }));
    output.with_evidence(evidence)
}

fn svg(ws: &Workspace, file: &str) -> Result<ToolOutput> {
    let path = resolve(ws, file)?;
    let png = rasterize_svg(&read_source(&path)?)?;
    derived(
        format!(
            "Image: {file} rasterized from SVG; attached to the next model request when vision is available"
        ),
        &png,
        Origin {
            source: file,
            location: Location::Whole,
            method: Method::Render,
            producer: "resvg",
            limitations: &[],
        },
    )
}

fn rasterize_svg(bytes: &[u8]) -> Result<Vec<u8>> {
    let tree = usvg::Tree::from_data(bytes, &usvg::Options::default())
        .context("SVG is not valid or cannot be parsed")?;
    let size = tree.size();
    let longest = size.width().max(size.height());
    ensure!(
        longest.is_finite() && longest > 0.0,
        "SVG has no drawable size"
    );
    let edge = policy::image_edge() as f32;
    let target = longest.max(MIN_SVG_EDGE).min(edge);
    let scale = target / longest;
    let width = (size.width() * scale).round().max(1.0) as u32;
    let height = (size.height() * scale).round().max(1.0) as u32;
    let mut pixmap =
        tiny_skia::Pixmap::new(width, height).context("SVG raster size is unsupported")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
        .encode_png()
        .map_err(|error| anyhow!("encoding SVG raster: {error}"))
}

fn heic(path: &Path, file: &str, cancel: &AtomicBool) -> Result<ToolOutput> {
    let sips = tool(
        "sips",
        "HEIC reads use macOS sips; convert the file to PNG or JPEG first",
    )?;
    let dir = tempfile::tempdir().context("creating conversion directory")?;
    let out = dir.path().join("converted.png");
    let spec = CommandSpec::new(sips)
        .arg("-s")
        .arg("format")
        .arg("png")
        .arg("-Z")
        .arg("2048")
        .arg(path.as_os_str())
        .arg("--out")
        .arg(out.as_os_str());
    run(&spec, None, cancel, COMMAND_TIMEOUT).context("converting HEIC image")?;
    let png = bounded_output(&out).context("HEIC conversion produced no bounded image")?;
    derived(
        format!(
            "Image: {file} converted from HEIC; attached to the next model request when vision is available"
        ),
        &png,
        Origin {
            source: file,
            location: Location::Whole,
            method: Method::Render,
            producer: "sips",
            limitations: &[],
        },
    )
}

fn render_pdf_page(
    ws: &Workspace,
    file: &str,
    page: u32,
    cancel: &AtomicBool,
) -> Result<ToolOutput> {
    let bytes = source::read(ws, file)?;
    let (png, snapshot) = super::visual::render_pdf_page(file, &bytes, page, cancel)?;
    let output = derived(
        format!(
            "Image: {file} page {page} rendered from PDF; attached to the next model request when vision is available"
        ),
        &png,
        Origin {
            source: file,
            location: Location::Page { number: page },
            method: Method::Render,
            producer: "pdftoppm",
            limitations: &[
                "Only the selected page was rendered; no text or layout interpretation was performed.",
            ],
        },
    )?;
    with_snapshot(output, snapshot)
}

struct Probe {
    duration: Option<f64>,
    fps: Option<f64>,
    width: u64,
    height: u64,
    video_codec: String,
    audio_codec: Option<String>,
}

fn video(path: &Path, file: &str, selector: &str, cancel: &AtomicBool) -> Result<ToolOutput> {
    let probe = probe(path, cancel)?;
    if selector.is_empty() {
        let text = describe_video(file, &probe);
        let grid = preview_grid(path, &probe, cancel)?;
        return derived(
            text,
            &grid,
            Origin {
                source: file,
                location: Location::Whole,
                method: Method::Sample,
                producer: "ffmpeg",
                limitations: &[
                    "Preview grid is sparse sampling, not continuous video coverage. Cell timestamps are not established. Audio was not interpreted.",
                ],
            },
        );
    }
    let at = locate(selector, &probe)?;
    ensure!(
        at * 1000.0 < u64::MAX as f64,
        "video position exceeds evidence timestamp range"
    );
    let requested_ms = (at * 1000.0).round() as u64;
    let png = frame(path, at, cancel)?;
    derived(
        format!(
            "Frame requested at {at:.3} s of {file} ({}x{}); attached to the next model request when vision is available",
            probe.width, probe.height
        ),
        &png,
        Origin {
            source: file,
            location: Location::Time {
                start_ms: requested_ms,
                end_ms: requested_ms,
            },
            method: Method::Sample,
            producer: "ffmpeg",
            limitations: &[
                "Timestamp is the requested seek position, not a measured presentation timestamp. A single frame cannot establish a sequence of actions. Audio was not interpreted.",
            ],
        },
    )
}

fn probe(path: &Path, cancel: &AtomicBool) -> Result<Probe> {
    let ffprobe = tool("ffprobe", FFMPEG_HINT)?;
    let spec = CommandSpec::new(ffprobe)
        .arg("-v")
        .arg("error")
        .arg("-print_format")
        .arg("json")
        .arg("-show_format")
        .arg("-show_streams")
        .arg(path.as_os_str());
    let out = run(&spec, None, cancel, COMMAND_TIMEOUT).context("reading video metadata")?;
    let value: Value =
        serde_json::from_slice(&out.stdout).context("ffprobe returned unreadable metadata")?;
    let empty = Vec::new();
    let streams = value["streams"].as_array().unwrap_or(&empty);
    let video = streams
        .iter()
        .find(|stream| stream["codec_type"] == "video")
        .context("file has no video stream")?;
    let audio_codec = streams
        .iter()
        .find(|stream| stream["codec_type"] == "audio")
        .and_then(|stream| stream["codec_name"].as_str())
        .map(str::to_string);
    let duration = value["format"]["duration"]
        .as_str()
        .and_then(|text| text.parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0);
    let fps = video["avg_frame_rate"]
        .as_str()
        .and_then(parse_rate)
        .or_else(|| video["r_frame_rate"].as_str().and_then(parse_rate));
    Ok(Probe {
        duration,
        fps,
        width: video["width"].as_u64().unwrap_or(0),
        height: video["height"].as_u64().unwrap_or(0),
        video_codec: video["codec_name"]
            .as_str()
            .unwrap_or("unknown")
            .to_string(),
        audio_codec,
    })
}

/// Parses an ffprobe rational such as `30000/1001`.
fn parse_rate(text: &str) -> Option<f64> {
    let (numerator, denominator) = text.split_once('/')?;
    let numerator: f64 = numerator.parse().ok()?;
    let denominator: f64 = denominator.parse().ok()?;
    (numerator > 0.0 && denominator > 0.0).then(|| numerator / denominator)
}

fn describe_video(file: &str, probe: &Probe) -> String {
    let duration = probe
        .duration
        .map_or_else(|| "unknown duration".to_string(), |d| format!("{d:.2} s"));
    let fps = probe
        .fps
        .map_or_else(String::new, |fps| format!(", {fps:.2} fps"));
    let audio = probe
        .audio_codec
        .as_deref()
        .map_or_else(String::new, |codec| format!(", {codec} audio"));
    format!(
        "Video: {file} ({}x{} {}{audio}, {duration}{fps}). Preview grid attached; read `{file}:<time>` (e.g. 90s, 01:23, 1h5m42s) or `{file}:<frame>` for one frame.",
        probe.width, probe.height, probe.video_codec
    )
}

fn preview_grid(path: &Path, probe: &Probe, cancel: &AtomicBool) -> Result<Vec<u8>> {
    let ffmpeg = tool("ffmpeg", FFMPEG_HINT)?;
    let Some(duration) = probe.duration else {
        bail!("video has no duration; a preview grid cannot be built");
    };
    // Sampling across the full duration yields at least PREVIEW_FRAMES frames for the 3x3 tile.
    let rate = PREVIEW_FRAMES as f64 / duration;
    let spec = CommandSpec::new(ffmpeg)
        .arg("-nostdin")
        .arg("-v")
        .arg("error")
        .arg("-i")
        .arg(path.as_os_str())
        .arg("-vf")
        .arg(format!(
            "fps={rate:.9},scale=320:320:force_original_aspect_ratio=decrease,tile=3x3"
        ))
        .arg("-frames:v")
        .arg("1")
        .arg("-f")
        .arg("image2pipe")
        .arg("-vcodec")
        .arg("png")
        .arg("-");
    let out = run(&spec, None, cancel, COMMAND_TIMEOUT).context("building video preview grid")?;
    ensure!(!out.stdout.is_empty(), "ffmpeg produced no preview grid");
    Ok(out.stdout)
}

fn frame(path: &Path, at: f64, cancel: &AtomicBool) -> Result<Vec<u8>> {
    let ffmpeg = tool("ffmpeg", FFMPEG_HINT)?;
    let spec = CommandSpec::new(ffmpeg)
        .arg("-nostdin")
        .arg("-v")
        .arg("error")
        .arg("-ss")
        .arg(format!("{at:.3}"))
        .arg("-i")
        .arg(path.as_os_str())
        .arg("-vf")
        .arg("scale=2048:2048:force_original_aspect_ratio=decrease")
        .arg("-frames:v")
        .arg("1")
        .arg("-f")
        .arg("image2pipe")
        .arg("-vcodec")
        .arg("png")
        .arg("-");
    let out = run(&spec, None, cancel, COMMAND_TIMEOUT).context("extracting video frame")?;
    ensure!(
        !out.stdout.is_empty(),
        "ffmpeg produced no frame at {at:.3} s"
    );
    Ok(out.stdout)
}

/// Resolves a video selector to seconds: a frame number (0-based), a clock (`01:23`,
/// `1:02:03`), or a unit duration (`90s`, `1h5m42s`, `12.5s`).
fn locate(selector: &str, probe: &Probe) -> Result<f64> {
    let seconds = if selector.bytes().all(|b| b.is_ascii_digit()) {
        let frame: u64 = selector
            .parse()
            .context("video frame number is out of range")?;
        let fps = probe
            .fps
            .context("this video reports no frame rate; use a time selector such as 90s")?;
        frame as f64 / fps
    } else if selector.contains(':') {
        parse_clock(selector)
            .with_context(|| format!("unrecognized clock selector `{selector}`"))?
    } else {
        parse_units(selector).with_context(|| {
            format!(
                "unrecognized video selector `{selector}`; use a frame number, 01:23, or 1h5m42s"
            )
        })?
    };
    ensure!(
        seconds.is_finite() && seconds >= 0.0,
        "video position must be a non-negative time"
    );
    if let Some(duration) = probe.duration {
        ensure!(
            seconds < duration,
            "position {selector} is beyond the video duration of {duration:.3} s"
        );
    }
    Ok(seconds)
}

fn parse_clock(text: &str) -> Option<f64> {
    let parts: Vec<f64> = text
        .split(':')
        .map(|part| part.parse::<f64>().ok())
        .collect::<Option<_>>()?;
    (parts.len() == 2 || parts.len() == 3).then(|| parts.iter().fold(0.0, |acc, p| acc * 60.0 + p))
}

fn parse_units(text: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut number = String::new();
    let mut units = 0;
    for ch in text.chars() {
        match ch {
            '0'..='9' | '.' => number.push(ch),
            'h' | 'm' | 's' => {
                let value: f64 = number.parse().ok()?;
                number.clear();
                total += value
                    * match ch {
                        'h' => 3600.0,
                        'm' => 60.0,
                        _ => 1.0,
                    };
                units += 1;
            }
            _ => return None,
        }
    }
    (number.is_empty() && units > 0).then_some(total)
}

#[cfg(test)]
mod tests;
