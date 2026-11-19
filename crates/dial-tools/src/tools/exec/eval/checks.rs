//! File identities for explicitly source-bound Eval checks. The predicate itself is cell-supplied.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

use crate::security::contracts::sha256_hex;
use crate::tools::Workspace;
use crate::tools::exec::preludes::PreludeCall;
use crate::tools::fs::data_io::MAX_BYTES;
use std::borrow::Cow;

/// Automated file observations never inherit an attached directory or a private-storage grant.
fn source_workspace(ws: &Workspace) -> Cow<'_, Workspace> {
    if ws.additional_roots.is_empty()
        && ws.read_only_roots.is_empty()
        && ws.sensitive_roots.is_empty()
    {
        return Cow::Borrowed(ws);
    }
    let mut scoped = ws.clone();
    scoped.additional_roots.clear();
    scoped.read_only_roots.clear();
    scoped.sensitive_roots.clear();
    Cow::Owned(scoped)
}

pub fn check_source_digest(ws: &Workspace, path: &str) -> Option<String> {
    if !is_check_source_path(path) {
        return None;
    }
    let bytes = crate::tools::knowledge::evidence::source::read_bounded(
        &source_workspace(ws),
        path,
        MAX_BYTES as u64,
    )
    .ok()?;
    Some(sha256_hex(&bytes))
}

/// A stable, portable literal under the primary workspace, not a selector, handle or URI.
/// This is a lexical filter, not read authorization.
pub fn is_check_source_path(path: &str) -> bool {
    path.len() <= 4096
        && !path.contains([':', '\\'])
        && !path.chars().any(char::is_control)
        && !matches!(path.split('/').next(), Some("~" | "$TMPDIR" | "${TMPDIR}"))
        && !path
            .strip_prefix("@f")
            .is_some_and(|s| s.starts_with(|c: char| c.is_ascii_digit()))
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

pub(in crate::tools::exec) fn confirm_source(
    source: &Value,
    workspace: Option<&Workspace>,
    call: &mut PreludeCall<'_>,
) -> Result<Value, String> {
    let invalid = "check source needs a whole-file dial.read receipt for a workspace-relative path";
    let path = source["path"]
        .as_str()
        .filter(|p| is_check_source_path(p))
        .ok_or(invalid)?;
    let sha = source["sha256"]
        .as_str()
        .filter(|s| crate::storage::image_refs::is_sha256_hex(s))
        .ok_or(invalid)?;
    if source.as_object().is_none_or(|fields| fields.len() != 2) {
        return Err(invalid.into());
    }
    let workspace = workspace.ok_or("source confirmation requires a foreground workspace")?;
    let resolved = source_workspace(workspace).resolve_read(path)?;
    let canonical = resolved
        .strip_prefix(workspace.root())
        .ok()
        .and_then(|path| path.to_str())
        .ok_or(invalid)?;
    #[cfg(windows)]
    let normalized = canonical.replace('\\', "/");
    #[cfg(windows)]
    let canonical = normalized.as_str();
    if !is_check_source_path(canonical) {
        return Err(invalid.into());
    }
    // Do not return file contents to the cell, or trust its digest, path or backend as evidence.
    let data = call.tool("read", json!({"path":canonical, "format":"base64"}))?;
    let malformed = "confirming read did not return a matching whole-file receipt";
    if data["path"] != canonical
        || data["sha256"] != sha
        || data["encoding"] != "base64"
        || ["offset", "lines", "next_offset"]
            .iter()
            .any(|k| data.get(*k).is_some())
    {
        return Err(malformed.into());
    }
    let size = data["bytes"]
        .as_u64()
        .filter(|n| *n <= MAX_BYTES as u64)
        .ok_or(malformed)? as usize;
    let encoded = data["content"]
        .as_str()
        .filter(|s| s.len() == size.div_ceil(3) * 4)
        .ok_or(malformed)?;
    let bytes = STANDARD.decode(encoded).map_err(|_| malformed)?;
    if bytes.len() != size || sha256_hex(&bytes) != sha {
        return Err(malformed.into());
    }
    Ok(json!({"path":path,"sha256":sha}))
}
