//! Lossless, bounded data access through the ordinary read/write admission paths.
use crate::tools::{ToolOutput, Workspace, integrations::ext::ToolContext};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_BYTES: usize = 1024 * 1024;

/// Structured data is returned to code, while the model-facing text stays a compact receipt.
pub(in crate::tools) fn read(ws: &Workspace, args: &Value) -> Result<ToolOutput, String> {
    if ["paths", "offset", "limit"]
        .iter()
        .any(|key| args.get(key).is_some())
    {
        return Err(
            "data reads require one literal local path without selectors or line ranges".into(),
        );
    }
    let input = crate::tools::req_str(args, "path")?;
    let format = crate::tools::req_str(args, "format")?;
    if !matches!(format, "text" | "base64") {
        return Err("read format must be text or base64".into());
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
            .map_err(|error| format!("reading data: {error:#}"))?
    };
    if bytes.len() > MAX_BYTES {
        return Err(
            "data reads are limited to 1 MiB; use ordinary ranged reads for larger files".into(),
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
    if args.get("verify").is_some() || args.get("allow_syntax_errors").is_some() {
        return Err("binary writes do not accept text-edit verification or syntax options".into());
    }
    let encoded = crate::tools::req_str(args, "content")?;
    if encoded.len() > MAX_BYTES.div_ceil(3) * 4 {
        return Err("binary write exceeds 1 MiB".into());
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|error| format!("invalid base64: {error}"))?;
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
    Ok(
        ToolOutput::structured(json!({"path":input,"bytes":bytes.len(),
        "sha256":format!("{:x}",Sha256::digest(&bytes)),"created":true}))
        .with_changes([path]),
    )
}

#[cfg(test)]
mod tests;
