//! Lossless, bounded data access through the ordinary read/write admission paths.
use crate::tools::{ToolOutput, Workspace, integrations::ext::ToolContext};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_BYTES: usize = 1024 * 1024;
/// Lines one ranged text read returns at most (also bounded by [`MAX_BYTES`]).
const MAX_LINES: u64 = 100_000;
/// Maximum consumed input per ranged page, including skipped lines. The read-data path has
/// no cancellation/deadline context, so refuse expensive offsets rather than scan indefinitely.
/// The buffered reader may read ahead by one fixed-size buffer beyond this ceiling.
const MAX_SCAN_BYTES: usize = 64 * MAX_BYTES;

/// Structured data is returned to code, while the model-facing text stays a compact receipt.
pub(in crate::tools) fn read(ws: &Workspace, args: &Value) -> Result<ToolOutput, String> {
    if args.get("paths").is_some() {
        return Err("data reads require one literal local path without selectors".into());
    }
    let input = crate::tools::req_str(args, "path")?;
    let format = crate::tools::req_str(args, "format")?;
    if !matches!(format, "text" | "base64") {
        return Err("read format must be text or base64".into());
    }
    if args.get("offset").is_some() || args.get("limit").is_some() {
        if format != "text" || args["retain"] == true || input.starts_with("artifact://") {
            return Err(
                "offset/limit page a local file's lines with format=text, without retain".into(),
            );
        }
        return read_lines(ws, input, args);
    }
    let bytes = if input.starts_with("artifact://") {
        if args["retain"] == true {
            return Err("retained artifacts need no second retention".into());
        }
        ws.artifacts()
            .ok_or("no artifact store")?
            .exact_bytes(input, MAX_BYTES)
            .map_err(|error| format!("reading artifact bytes: {error:#}"))?
    } else {
        crate::tools::knowledge::evidence::source::read_bounded(ws, input, MAX_BYTES as u64)
            .map_err(|error| {
                format!(
                    "reading data: {error:#}; for a larger text file page its lines with offset/limit"
                )
            })?
    };
    if bytes.len() > MAX_BYTES {
        return Err(
            "data reads are limited to 1 MiB; page a larger text file with offset/limit".into(),
        );
    }
    let content = if format == "base64" {
        STANDARD.encode(&bytes)
    } else {
        String::from_utf8(bytes.clone()).map_err(|_| "data is not UTF-8; use format=base64")?
    };
    let artifact = if args["retain"] == true {
        Some(
            ws.artifacts()
                .ok_or("retention requires an artifact store")?
                .retain_exact_bytes(&bytes)
                .map_err(|error| error.to_string())?,
        )
    } else {
        None
    };
    let mut data = json!({"path":input,"encoding":format,"bytes":bytes.len(),
        "sha256":format!("{:x}", Sha256::digest(&bytes)),"artifact":artifact});
    let text = data.to_string();
    data["content"] = json!(content);
    Ok(ToolOutput {
        text,
        data: Some(data),
        is_error: false,
        execution: Default::default(),
    })
}

/// Lines `offset..offset+limit` (1-based), preserving line endings up to [`MAX_BYTES`].
/// Each page scans at most [`MAX_SCAN_BYTES`] input bytes, including skipped lines;
/// offsets beyond that work budget are refused. `next_offset` is null at EOF.
fn read_lines(ws: &Workspace, input: &str, args: &Value) -> Result<ToolOutput, String> {
    use std::io::BufRead;
    let offset = crate::tools::opt_u64(args, "offset")?.unwrap_or(1);
    if offset == 0 {
        return Err("offset is 1-based".into());
    }
    let limit = crate::tools::opt_u64(args, "limit")?
        .unwrap_or(MAX_LINES)
        .clamp(1, MAX_LINES);
    let file = crate::tools::knowledge::evidence::source::open_admitted(ws, input)
        .map_err(|error| format!("reading data: {error:#}"))?;
    let mut reader = std::io::BufReader::new(file);
    let (mut line_no, mut taken) = (0u64, 0u64);
    let mut content = Vec::new();
    let mut scan_left = MAX_SCAN_BYTES;
    let mut next = None;
    loop {
        if reader
            .fill_buf()
            .map_err(|error| format!("reading data: {error}"))?
            .is_empty()
        {
            break;
        }
        line_no += 1;
        if line_no < offset {
            scan_line(&mut reader, None, 0, &mut scan_left)
                .map_err(|error| format!("reading data: {error}"))?;
            continue;
        }
        if taken == limit {
            next = Some(line_no);
            break;
        }
        let before = content.len();
        if !scan_line(&mut reader, Some(&mut content), MAX_BYTES, &mut scan_left)
            .map_err(|error| format!("reading data: {error}"))?
        {
            content.truncate(before);
            if taken > 0 {
                next = Some(line_no);
                break;
            }
            return Err(format!(
                "line {line_no} alone exceeds 1 MiB; read it with format=base64 or a shell tool"
            ));
        }
        taken += 1;
    }
    let content = String::from_utf8(content)
        .map_err(|_| "these lines are not UTF-8; use format=base64 on a smaller file")?;
    let mut data = json!({"path":input,"encoding":"text","offset":offset,"lines":taken,
        "bytes":content.len(),"sha256":format!("{:x}", Sha256::digest(content.as_bytes())),
        "next_offset":next});
    let text = data.to_string();
    data["content"] = json!(content);
    Ok(ToolOutput {
        text,
        data: Some(data),
        is_error: false,
        execution: Default::default(),
    })
}

/// Consume one line without allocating for skipped bytes. A selected line stops at the
/// output budget rather than materializing an arbitrarily large line before checking it.
/// `false` means the line did not fit; the caller must discard its partial contribution.
fn scan_line(
    reader: &mut impl std::io::BufRead,
    mut output: Option<&mut Vec<u8>>,
    budget: usize,
    scan_left: &mut usize,
) -> std::io::Result<bool> {
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Ok(true);
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(chunk.len(), |index| index + 1);
        if let Some(output) = output.as_mut() {
            if count > budget.saturating_sub(output.len()) {
                return Ok(false);
            }
            output.extend_from_slice(&chunk[..count]);
        }
        if count > *scan_left {
            return Err(std::io::Error::other(
                "ranged read exceeds the 64 MiB scan budget, including skipped lines; use a shell tool to select a smaller source",
            ));
        }
        *scan_left -= count;
        reader.consume(count);
        if newline.is_some() {
            return Ok(true);
        }
    }
}

/// Best-effort initial task preservation. Only direct regular root files, with explicit file,
/// byte and elapsed limits; no links, hidden files or credential-like names are captured.
pub(in crate::tools) fn preserve_inputs(
    ws: &Workspace,
    checkpoints: &crate::storage::checkpoint::Checkpoints,
    turn: u64,
) -> Value {
    preserve_inputs_with_budget(ws, checkpoints, turn, std::time::Duration::from_millis(100))
}

fn preserve_inputs_with_budget(
    ws: &Workspace,
    checkpoints: &crate::storage::checkpoint::Checkpoints,
    turn: u64,
    budget: std::time::Duration,
) -> Value {
    let started = std::time::Instant::now();
    let Some(artifacts) = ws.artifacts() else {
        return json!({"files":[],"coverage":"unavailable: no artifact store"});
    };
    let Ok(entries) = std::fs::read_dir(ws.root()) else {
        return json!({"files":[],"coverage":"unavailable: root unreadable"});
    };
    let mut entries: Vec<_> = entries.take(257).filter_map(Result::ok).collect();
    if entries.len() > 256 {
        return json!({"files":[],"coverage":"skipped: root directory exceeds 256 entries"});
    }
    entries.sort_by_key(|entry| entry.file_name());
    let mut files = Vec::new();
    let mut total = 0;
    let mut omitted = 0;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name.starts_with('.')
            || ["secret", "credential", "token", "password"]
                .iter()
                .any(|part| name.contains(part))
            || [".pem", ".key", ".p12", ".pfx"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
            || !entry.file_type().is_ok_and(|kind| kind.is_file())
            || ws.is_protected(&entry.path())
        {
            omitted += 1;
            continue;
        }
        if files.len() >= 64 || started.elapsed() > budget {
            omitted += 1;
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            omitted += 1;
            continue;
        };
        if meta.len() > MAX_BYTES as u64 || total + meta.len() as usize > 4 * MAX_BYTES {
            omitted += 1;
            continue;
        }
        let path = entry.path();
        let Ok(bytes) = crate::tools::knowledge::evidence::source::read_bounded(
            ws,
            &ws.rel(&path),
            MAX_BYTES as u64,
        ) else {
            omitted += 1;
            continue;
        };
        match checkpoints.preserve_input(turn, &path, &bytes, artifacts) {
            Ok(receipt) => {
                total += bytes.len();
                files.push(receipt);
            }
            Err(_) => {
                omitted += 1;
            }
        }
    }
    json!({"files":files,"bytes":total,"omitted":omitted,"coverage":"bounded root-level regular inputs; not an atomic database snapshot"})
}

/// Binary writes deliberately create a separate new file. The native binary transaction owns
/// no-follow staging, checkpoint observation, cancellation and exclusive publication.
pub(in crate::tools) fn write(
    ctx: &ToolContext<'_>,
    args: &Value,
    cancel: &AtomicBool,
) -> Result<ToolOutput, String> {
    if crate::tools::present(args, "verify") || crate::tools::present(args, "allow_syntax_errors") {
        return Err("binary writes do not accept text-edit verification or syntax options".into());
    }
    super::write::entries(args)?;
    if crate::tools::present(args, "expected_sha256") || args["idempotent"] == true {
        return Err(
            "binary writes are create-only and do not accept expected_sha256 or idempotent".into(),
        );
    }
    let bytes = if crate::tools::present(args, "source") {
        super::write::artifact_bytes(ctx.workspace, args, MAX_BYTES)?
    } else {
        let encoded = crate::tools::req_str(args, "content")?;
        if encoded.len() > MAX_BYTES.div_ceil(3) * 4 {
            return Err("binary write exceeds 1 MiB".into());
        }
        STANDARD
            .decode(encoded)
            .map_err(|error| format!("invalid base64: {error}"))?
    };
    if bytes.len() > MAX_BYTES {
        return Err("binary write exceeds 1 MiB".into());
    }
    let input = crate::tools::req_str(args, "path")?;
    let path = ctx.workspace.resolve(input)?;
    ctx.authorize_mutation("write", std::slice::from_ref(&path), false)?;
    if cancel.load(Ordering::SeqCst) {
        return Err("binary write interrupted".into());
    }
    let mut staged = crate::tools::edit::edit_transaction::NewBinary::create(ctx.workspace, &path)?;
    staged
        .file_mut()
        .write_all(&bytes)
        .map_err(|error| error.to_string())?;
    staged.publish(ctx.workspace, cancel)?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    Ok(ToolOutput::structured(json!({"path":input,"bytes":bytes.len(),"sha256":hash,"created":true,
        "files":[{"path":input,"bytes":bytes.len(),"sha256":hash,"outcome":"created","changed":true}],
        "changed":true,"verification":{"status":"not_requested"}})).with_changes([path]))
}

#[cfg(test)]
mod tests;
