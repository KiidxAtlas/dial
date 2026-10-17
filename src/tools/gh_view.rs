//! `repo_view` and `file_read` ops of the `github` tool.

use std::fmt::Write as _;
use std::io::Cursor;
use std::sync::atomic::AtomicBool;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;

use super::gh_common::*;
use super::{ToolOutput, Workspace, opt_str};

/// Fields requested from `gh repo view --json`.
const REPO_FIELDS: &[&str] = &[
    "nameWithOwner",
    "description",
    "url",
    "defaultBranchRef",
    "homepageUrl",
    "forkCount",
    "isArchived",
    "isFork",
    "primaryLanguage",
    "repositoryTopics",
    "stargazerCount",
    "updatedAt",
    "viewerPermission",
    "visibility",
];

/// Bytes sniffed for NUL / UTF-8 validity before reading a file as text.
const BINARY_SNIFF_BYTES: usize = 8192;

pub(super) fn run(
    op: &str,
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
) -> Result<ToolOutput, String> {
    match op {
        "repo_view" => repo_view(ws, args, cancel),
        "file_read" => file_read(ws, args, cancel),
        other => Err(format!("gh_view does not handle op {other:?}")),
    }
}

fn repo_view(ws: &Workspace, args: &Value, cancel: &AtomicBool) -> Result<ToolOutput, String> {
    let repo = normalize_optional(opt_str(args, "repo")?);
    let branch = normalize_optional(opt_str(args, "branch")?);
    let mut argv = strings(&["repo", "view"]);
    if let Some(repo) = &repo {
        let parsed = parse_repo_ref(repo)?;
        argv.push(parsed.qualified());
    }
    if let Some(branch) = &branch {
        argv.push("--branch".to_owned());
        argv.push(branch.clone());
    }
    argv.push("--json".to_owned());
    argv.push(REPO_FIELDS.join(","));

    let raw = gh(ws, &argv, GH_TIMEOUT, cancel)?;
    let data: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("GitHub CLI returned invalid JSON for gh repo view: {e}"))?;
    let text = format_repo_view(&data, repo.as_deref(), branch.as_deref());
    Ok(ToolOutput::ok(clip(ws, text)))
}

/// Formats the repo view.
fn format_repo_view(data: &Value, repo: Option<&str>, branch: Option<&str>) -> String {
    let mut lines: Vec<String> = Vec::new();
    let name = data["nameWithOwner"]
        .as_str()
        .or(repo)
        .unwrap_or("GitHub Repository");
    lines.push(format!("# {name}"));
    lines.push(String::new());
    let description = normalize_text(data["description"].as_str().unwrap_or(""));
    lines.push(if description.is_empty() {
        "No description provided.".to_owned()
    } else {
        description
    });
    lines.push(String::new());

    push_line(&mut lines, "URL", scalar(&data["url"]));
    push_line(
        &mut lines,
        "Default branch",
        scalar(&data["defaultBranchRef"]["name"]),
    );
    push_line(&mut lines, "Branch", branch.map(str::to_owned));
    push_line(&mut lines, "Visibility", scalar(&data["visibility"]));
    push_line(
        &mut lines,
        "Viewer permission",
        scalar(&data["viewerPermission"]),
    );
    push_line(
        &mut lines,
        "Primary language",
        scalar(&data["primaryLanguage"]["name"]),
    );
    push_line(&mut lines, "Stars", scalar(&data["stargazerCount"]));
    push_line(&mut lines, "Forks", scalar(&data["forkCount"]));
    push_line(&mut lines, "Archived", scalar(&data["isArchived"]));
    push_line(&mut lines, "Fork", scalar(&data["isFork"]));
    push_line(&mut lines, "Updated", scalar(&data["updatedAt"]));
    push_line(&mut lines, "Homepage", scalar(&data["homepageUrl"]));
    let topics = data["repositoryTopics"]
        .as_array()
        .map(|topics| {
            topics
                .iter()
                .filter_map(|topic| {
                    topic["name"]
                        .as_str()
                        .or_else(|| topic["topic"]["name"].as_str())
                })
                .filter(|name| !name.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|joined| !joined.is_empty());
    push_line(&mut lines, "Topics", topics);

    lines.join("\n").trim().to_owned()
}

/// Skips absent and empty values; booleans and numbers are rendered.
fn push_line(lines: &mut Vec<String>, label: &str, value: Option<String>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        lines.push(format!("{label}: {value}"));
    }
}

/// Scalar JSON value rendered as text; `null`, arrays, and objects are absent.
fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn file_read(ws: &Workspace, args: &Value, cancel: &AtomicBool) -> Result<ToolOutput, String> {
    let repo_arg = opt_str(args, "repo")?
        .map(str::trim)
        .filter(|repo| !repo.is_empty());
    let repo = resolve_repo(ws, repo_arg, cancel)?;
    let repo_name = repo.qualified();
    let path = normalize_optional(opt_str(args, "path")?).ok_or("path is required")?;
    if path.starts_with('/') {
        return Err("path must be repository-relative".to_owned());
    }
    let branch = normalize_optional(opt_str(args, "branch")?);
    let endpoint_path = path
        .split('/')
        .map(encode_uri_component)
        .collect::<Vec<_>>()
        .join("/");
    let revision = branch.clone().unwrap_or_else(|| "HEAD".to_owned());

    let mut argv = strings(&[
        "api",
        &format!("/repos/{}/contents/{endpoint_path}", repo.slug),
        "--method",
        "GET",
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "Accept-Encoding: identity",
    ]);
    argv.extend(repo.hostname_args());
    if let Some(branch) = &branch {
        argv.push("-f".to_owned());
        argv.push(format!("ref={branch}"));
    }
    // The Contents API 404s conflate repository, revision, path, and access failures; name the request.
    let raw = gh(ws, &argv, GH_TIMEOUT, cancel).map_err(|error| {
        format!("GitHub file read failed for '{repo_name}@{revision}:{path}': {error}")
    })?;
    let response: Value = serde_json::from_str(&raw).map_err(|error| {
        format!(
            "GitHub file read failed for '{repo_name}@{revision}:{path}': invalid JSON: {error}"
        )
    })?;
    if response.is_array() || response["type"] != "file" {
        return Err(format!("GitHub path '{path}' is not a file."));
    }

    let host = repo.host.clone().unwrap_or_else(default_gh_host);
    let fallback_url = format!(
        "https://{host}/{}/blob/{}/{endpoint_path}",
        repo.slug,
        encode_uri_component(&revision)
    );
    let source_url = response["html_url"]
        .as_str()
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
        .unwrap_or(fallback_url);

    let content = match response["content"].as_str() {
        Some(content) if response["encoding"] == "base64" => content,
        _ => {
            let size = response["size"]
                .as_u64()
                .map_or_else(|| "unknown size".to_owned(), format_bytes);
            return Ok(ToolOutput::ok(format!(
                "[GitHub did not return file bytes for '{path}' ({size}). Open {source_url} to view it.]"
            )));
        }
    };

    let compact: String = content.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = STANDARD
        .decode(&compact)
        .map_err(|error| format!("GitHub returned invalid base64 for '{path}': {error}"))?;

    if let Some((mime, dimensions)) = image_info(&bytes) {
        let dimensions = dimensions
            .map(|(w, h)| format!("\nDimensions: {w}x{h}"))
            .unwrap_or_default();
        // PNG bytes travel as `data.artifact` so `artifact::capture_image` attaches them; other formats stay text-only.
        let tail = if mime == "image/png" {
            "Image attached to this result.".to_owned()
        } else {
            format!("Image contents are not included in this result; open {source_url} to view it.")
        };
        let text = clip(
            ws,
            format!(
                "Image file: {path}\nMIME: {mime}\nSize: {}{dimensions}\n{tail}",
                format_bytes(bytes.len() as u64)
            ),
        );
        let data = (mime == "image/png").then(|| {
            serde_json::json!({ "artifact": { "encoding": "base64", "media_type": mime, "data": compact } })
        });
        return Ok(ToolOutput {
            execution: Default::default(),
            text,
            is_error: false,
            data,
        });
    }

    let header = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
    // A sniff window can end inside a multi-byte character; only a genuinely invalid byte is binary evidence.
    let invalid_utf8 = std::str::from_utf8(header).is_err_and(|error| error.error_len().is_some());
    let probably_binary = header.contains(&0) || invalid_utf8;
    if !probably_binary && let Ok(text) = std::str::from_utf8(&bytes) {
        return Ok(ToolOutput::ok(clip(ws, text.to_owned())));
    }
    Ok(ToolOutput::ok(format!(
        "[Cannot read binary file '{path}' ({}); not valid UTF-8 text. Open {source_url} to view it.]",
        format_bytes(bytes.len() as u64)
    )))
}

/// Recognizes the image formats this tool can attach (PNG, JPEG, GIF, WebP) and reads their dimensions
/// from the header without decoding pixels.
fn image_info(bytes: &[u8]) -> Option<(&'static str, Option<(u32, u32)>)> {
    let mime = match image::guess_format(bytes).ok()? {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        _ => return None,
    };
    let dimensions = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_dimensions().ok());
    Some((mime, dimensions))
}

/// Formats a byte count with human-readable units.
fn format_bytes(size: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let size_f = size as f64;
    if size < 1024 {
        format!("{size}B")
    } else if size_f < MB {
        format!("{:.1}KB", size_f / KB)
    } else {
        format!("{:.1}MB", size_f / MB)
    }
}

/// JavaScript `encodeURIComponent`: unreserved characters pass through, everything else is
/// percent-encoded as UTF-8 bytes.
fn encode_uri_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Trims; empty becomes absent.
fn normalize_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// Normalizes text: unify line endings, expand tabs, trim.
fn normalize_text(value: &str) -> String {
    value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests;
