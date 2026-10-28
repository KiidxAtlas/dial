//! Visual media reads that yield a model-visible PNG: SVG rasters, HEIC conversions, PDF page
//! renders and video metadata, preview grids and frames. Pixels travel on the structured
//! `data.artifact` channel (captured like screenshots); the text stays a short description.
//! External tools are detected at runtime and missing ones fail with an actionable error.
use crate::llm::{images, policy};
use crate::runtime::tool_output::ToolOutput;
use crate::tools::Workspace;
use crate::voice::process::{CommandSpec, run, which};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use resvg::{tiny_skia, usvg};
use serde_json::{Value, json};
use std::fs;
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

/// Intercepts media targets before the generic file reader. `None` means the target is not a
/// media request and ordinary text or image handling applies.
pub(crate) fn read(
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
) -> Option<Result<ToolOutput, String>> {
    let path_text = args.get("path")?.as_str()?;
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
    let result = match ext.as_str() {
        "svg" if selector == "img" => svg(ws, file),
        "pdf" => render_pdf_page(ws, file, pdf_page(selector)?, cancel),
        "heic" | "heif" if selector.is_empty() => heic(ws, file, cancel),
        "heic" | "heif" => Err(anyhow!("HEIC images take no selector")),
        ext if VIDEO_EXTENSIONS.contains(&ext) => video(ws, file, selector, cancel),
        _ => return None,
    };
    Some(result.map_err(|error| format!("{error:#}")))
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
        .filter(|page| *page >= 1)
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

/// Wraps a derived PNG as the structured artifact the agent captures and attaches.
fn derived(text: String, png: &[u8]) -> Result<ToolOutput> {
    let png = images::fit_png(png)?;
    let mut output = ToolOutput::ok(text);
    output.data = Some(json!({
        "artifact": {
            "encoding": "base64",
            "media_type": "image/png",
            "data": STANDARD.encode(&png),
        }
    }));
    Ok(output)
}

fn svg(ws: &Workspace, file: &str) -> Result<ToolOutput> {
    let path = resolve(ws, file)?;
    let png = rasterize_svg(&read_source(&path)?)?;
    derived(
        format!(
            "Image: {file} rasterized from SVG; attached to the next model request when vision is available"
        ),
        &png,
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

fn heic(ws: &Workspace, file: &str, cancel: &AtomicBool) -> Result<ToolOutput> {
    let path = resolve(ws, file)?;
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
        .arg(path.as_os_str())
        .arg("--out")
        .arg(out.as_os_str());
    run(&spec, None, cancel, COMMAND_TIMEOUT).context("converting HEIC image")?;
    let png = fs::read(&out).context("HEIC conversion produced no image")?;
    derived(
        format!(
            "Image: {file} converted from HEIC; attached to the next model request when vision is available"
        ),
        &png,
    )
}

fn render_pdf_page(
    ws: &Workspace,
    file: &str,
    page: u32,
    cancel: &AtomicBool,
) -> Result<ToolOutput> {
    let path = resolve(ws, file)?;
    let pdftoppm = tool(
        "pdftoppm",
        "PDF page images use poppler's pdftoppm (install with `brew install poppler`)",
    )?;
    let dir = tempfile::tempdir().context("creating render directory")?;
    let prefix = dir.path().join("page");
    let spec = CommandSpec::new(pdftoppm)
        .arg("-png")
        .arg("-r")
        .arg("150")
        .arg("-f")
        .arg(page.to_string())
        .arg("-l")
        .arg(page.to_string())
        .arg("-singlefile")
        .arg(path.as_os_str())
        .arg(prefix.as_os_str());
    run(&spec, None, cancel, COMMAND_TIMEOUT).context("rendering PDF page")?;
    let png = fs::read(dir.path().join("page.png"))
        .context("pdftoppm produced no page image; the page may be out of range")?;
    derived(
        format!(
            "Image: {file} page {page} rendered from PDF; attached to the next model request when vision is available"
        ),
        &png,
    )
}

struct Probe {
    duration: Option<f64>,
    fps: Option<f64>,
    width: u64,
    height: u64,
    video_codec: String,
    audio_codec: Option<String>,
}

fn video(ws: &Workspace, file: &str, selector: &str, cancel: &AtomicBool) -> Result<ToolOutput> {
    let path = resolve(ws, file)?;
    let probe = probe(&path, cancel)?;
    if selector.is_empty() {
        let text = describe_video(file, &probe);
        let grid = preview_grid(&path, &probe, cancel)?;
        return derived(text, &grid);
    }
    let at = locate(selector, &probe)?;
    let png = frame(&path, at, cancel)?;
    derived(
        format!(
            "Frame at {at:.3} s of {file} ({}x{}); attached to the next model request when vision is available",
            probe.width, probe.height
        ),
        &png,
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
        .arg(format!("fps={rate:.9},scale=320:-2,tile=3x3"))
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
