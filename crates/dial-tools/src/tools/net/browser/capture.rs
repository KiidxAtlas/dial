//! Host-side support for the capture actions: argument validation (executed in `capture.cjs`
//! inside the driver) and ffmpeg encoding of driver-spooled screencast frames.

use serde_json::Value;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const FFMPEG_TIMEOUT: Duration = Duration::from_secs(300);
const FFMPEG_STDOUT_CAP: usize = 64 * 1024;
const FFMPEG_STDERR_CAP: usize = 16 * 1024;

/// Actions run by `capture.cjs`. `screenshot` is among them too, but it already has a base entry in
/// the browser action list, so it is not repeated here.
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
