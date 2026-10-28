//! Host-side support for the capture actions: argument validation (executed in `capture.cjs`
//! inside the driver) and ffmpeg encoding of driver-spooled screencast frames.

use serde_json::Value;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const FFMPEG_TIMEOUT: Duration = Duration::from_secs(300);
const FFMPEG_STDOUT_CAP: usize = 64 * 1024;
const FFMPEG_STDERR_CAP: usize = 16 * 1024;

/// Actions run by `capture.cjs`. `screenshot` and `locate` are among them too, but they already
/// have base entries in the browser action list, so they are not repeated here.
pub const ACTIONS: &[&str] = &[
    "pdf",
    "record_start",
    "record_stop",
    "profile_start",
    "profile_stop",
    "metrics",
    "vitals",
];

/// Whether `action` is one of the capture actions listed above.
pub fn is_action(action: &str) -> bool {
    ACTIONS.contains(&action)
}

/// Validate capture arguments before the driver sees them.
pub fn validate(action: &str, args: &Value) -> Result<(), String> {
    match action {
        "screenshot" => validate_screenshot(args),
        "locate" => validate_locate(args),
        "record_start" => flag(args, "record_restart"),
        _ => Ok(()),
    }
}

fn flag(args: &Value, key: &str) -> Result<(), String> {
    match args.get(key) {
        None => Ok(()),
        Some(value) if value.is_boolean() => Ok(()),
        Some(_) => Err(format!("{key} must be a boolean")),
    }
}

fn validate_screenshot(args: &Value) -> Result<(), String> {
    let format = match args.get("format") {
        None => "png",
        Some(value) => value.as_str().ok_or("format must be a string")?,
    };
    if !matches!(format, "png" | "jpeg") {
        return Err("format must be png or jpeg".into());
    }
    if let Some(quality) = args.get("quality") {
        if format != "jpeg" {
            return Err("quality requires format jpeg".into());
        }
        quality
            .as_u64()
            .filter(|quality| (1..=100).contains(quality))
            .ok_or("quality must be an integer from 1 to 100")?;
    }
    flag(args, "annotate")?;
    flag(args, "if_changed")
}

fn validate_locate(args: &Value) -> Result<(), String> {
    let description = args
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or("locate requires description, e.g. \"the blue Sign in button\"")?;
    if description.chars().count() > 500 {
        return Err("locate description is limited to 500 characters".into());
    }
    if args.get("full_page").and_then(Value::as_bool) == Some(true) {
        return Err(
            "locate works on the viewport; scroll the element into view instead of full_page"
                .into(),
        );
    }
    Ok(())
}

/// Ground the driver's `locate` capture with the configured grounding model and map the device-pixel
/// point back to viewport CSS pixels for `click_at`.
pub(super) fn finish_locate(
    result: &mut Value,
    config: &crate::config::types::Config,
    description: &str,
    cancel: std::sync::Arc<AtomicBool>,
) -> Result<(), String> {
    use base64::Engine as _;
    let image = result
        .as_object_mut()
        .and_then(|object| object.remove("image"))
        .ok_or("browser locate returned no screenshot")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image.as_str().ok_or("invalid browser locate screenshot")?)
        .map_err(|error| format!("invalid browser locate screenshot: {error}"))?;
    let css_width = result["viewport"]["width"].as_f64().unwrap_or(0.0);
    let css_height = result["viewport"]["height"].as_f64().unwrap_or(0.0);
    if css_width <= 0.0 || css_height <= 0.0 {
        return Err("browser locate returned no viewport size".into());
    }
    let located = crate::tools::vision::grounding::locate(config, &bytes, description, cancel)
        .map_err(|error| format!("{error:#}"))?;
    let (x, y) = css_point(&located, css_width, css_height);
    result["x"] = serde_json::json!(x);
    result["y"] = serde_json::json!(y);
    result["model"] = serde_json::json!(located.model);
    result["hint"] = serde_json::json!(
        "x/y are viewport CSS pixels from a vision model; pass them to click_at and verify with a screenshot"
    );
    Ok(())
}

/// Device-pixel point → viewport CSS pixels, rounded to 0.1 px.
fn css_point(
    located: &crate::tools::vision::grounding::Located,
    css_width: f64,
    css_height: f64,
) -> (f64, f64) {
    let scale_x = css_width / f64::from(located.width.max(1));
    let scale_y = css_height / f64::from(located.height.max(1));
    let round = |value: f64| (value * 10.0).round() / 10.0;
    (
        round(f64::from(located.x) * scale_x),
        round(f64::from(located.y) * scale_y),
    )
}

/// Encode the driver's concat list into an H.264 MP4 at 30 fps and remove the spooled frames.
/// A missing ffmpeg is an error naming the requirement, never a silent skip.
pub(super) fn finish_recording(result: &mut Value, cancel: &AtomicBool) -> Result<(), String> {
    let concat = result["concat"]
        .as_str()
        .ok_or("browser recording returned no frame list")?
        .to_owned();
    let output = result["recording"]
        .as_str()
        .ok_or("browser recording returned no output path")?
        .to_owned();
    let mut command = Command::new("ffmpeg");
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
        concat.as_str(),
        "-vf",
        "fps=30,scale=trunc(iw/2)*2:trunc(ih/2)*2",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "23",
        "-pix_fmt",
        "yuv420p",
        "-movflags",
        "+faststart",
        output.as_str(),
    ]);
    let mut policy = crate::runtime::process::Policy::bounded(FFMPEG_TIMEOUT, FFMPEG_STDOUT_CAP);
    policy.stderr_cap = FFMPEG_STDERR_CAP;
    let finished = crate::runtime::process::run_with_policy(
        command,
        None,
        policy,
        cancel,
        &mut |_, _| {},
    )
    .map_err(|error| {
        format!(
            "browser recording requires ffmpeg on PATH (install it, for example `brew install ffmpeg`): {error}"
        )
    })?;
    if !finished.success() {
        return Err(finished.failure("ffmpeg", FFMPEG_TIMEOUT));
    }
    if let Some(spool) = std::path::Path::new(&concat).parent() {
        std::fs::remove_dir_all(spool)
            .map_err(|error| format!("removing browser recording frames: {error}"))?;
    }
    if let Some(object) = result.as_object_mut() {
        object.remove("concat");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
