//! Bounded whole-file write requests and machine-readable persistence receipts.
use crate::tools::{
    ToolOutput, Workspace, edit::edit_transaction::Change, opt_bool, present, req_str,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
pub(super) const MAX_FILES: usize = 64;
pub(super) const MAX_TOTAL: usize = 8 * 1024 * 1024;

/// One validated destination's opt-in semantics, kept next to its transaction snapshot.
pub(crate) struct Policy {
    pub path: std::path::PathBuf,
    pub idempotent: bool,
    pub exact_sha256: Option<String>,
}

pub(in crate::tools) fn entries(args: &Value) -> Result<Vec<&Value>, String> {
    let object = args
        .as_object()
        .ok_or("write arguments must be an object")?;
    let batch = present(args, "files");
    let allowed: &[&str] = if batch {
        &["files", "verify", "allow_syntax_errors", "receipt"]
    } else {
        &[
            "path",
            "content",
            "source",
            "encoding",
            "if_absent",
            "expected_sha256",
            "idempotent",
            "receipt",
            "verify",
            "allow_syntax_errors",
        ]
    };
    if object
        .iter()
        .any(|(key, value)| !value.is_null() && !allowed.contains(&key.as_str()))
    {
        return Err(
            "write contains unknown fields or mixes single-file and batch arguments".into(),
        );
    }
    opt_bool(args, "receipt")?;
    let entries = if batch {
        let files = args["files"].as_array().ok_or("files must be an array")?;
        if files.is_empty() || files.len() > MAX_FILES {
            return Err(format!("files requires 1..{MAX_FILES} entries"));
        }
        files.iter().collect()
    } else {
        vec![args]
    };
    for entry in &entries {
        let object = entry
            .as_object()
            .ok_or("each write entry must be an object")?;
        if batch
            && object.iter().any(|(key, value)| {
                !value.is_null()
                    && !matches!(
                        key.as_str(),
                        "path"
                            | "content"
                            | "source"
                            | "if_absent"
                            | "expected_sha256"
                            | "idempotent"
                    )
            })
        {
            return Err("batch entries accept path, content/source, if_absent, expected_sha256 and idempotent only; binary and remote batches are unsupported".into());
        }
        let path = req_str(entry, "path")?;
        if batch && path.starts_with("ssh://") {
            return Err("remote writes cannot be batched".into());
        }
        if present(entry, "content") == present(entry, "source") {
            return Err("supply exactly one of content or source for each write".into());
        }
        if present(entry, "content") {
            req_str(entry, "content")?;
        }
        if present(entry, "source") {
            let source = entry["source"]
                .as_object()
                .ok_or("source must be an artifact object")?;
            if source.iter().any(|(key, value)| {
                !value.is_null() && !matches!(key.as_str(), "artifact" | "sha256")
            }) {
                return Err("source accepts only artifact and sha256".into());
            }
            let uri = req_str(&entry["source"], "artifact")?;
            if !uri.starts_with("artifact://") {
                return Err("source.artifact must be an immutable artifact:// reference".into());
            }
            validate_hash(req_str(&entry["source"], "sha256")?)?;
        }
        opt_bool(entry, "if_absent")?;
        opt_bool(entry, "idempotent")?;
        if present(entry, "expected_sha256") {
            validate_hash(req_str(entry, "expected_sha256")?)?;
            if entry["if_absent"] == true {
                return Err("if_absent and expected_sha256 are mutually exclusive".into());
            }
        }
    }
    Ok(entries)
}

pub(in crate::tools) fn targets(args: &Value) -> Result<Vec<String>, String> {
    if present(args, "files") {
        if present(args, "path") {
            return Err("write cannot mix path and files".into());
        }
        let files = args["files"].as_array().ok_or("files must be an array")?;
        if files.is_empty() || files.len() > MAX_FILES {
            return Err(format!("files requires 1..{MAX_FILES} entries"));
        }
        files
            .iter()
            .map(|entry| req_str(entry, "path").map(str::to_owned))
            .collect()
    } else {
        Ok(vec![req_str(args, "path")?.to_owned()])
    }
}

fn validate_hash(hash: &str) -> Result<(), String> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("sha256 must contain exactly 64 hexadecimal digits".into());
    }
    Ok(())
}

pub(in crate::tools) fn artifact_bytes(
    ws: &Workspace,
    entry: &Value,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let source = &entry["source"];
    let uri = req_str(source, "artifact")?;
    let expected = req_str(source, "sha256")?;
    validate_hash(expected)?;
    let bytes = ws
        .artifacts()
        .ok_or("source requires an artifact store")?
        .exact_bytes(uri, limit)
        .map_err(|error| format!("reading write source: {error:#}"))?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(expected)
        || !uri
            .strip_prefix("artifact://")
            .is_some_and(|id| id.eq_ignore_ascii_case(&actual))
    {
        return Err("artifact source SHA-256 mismatch; nothing was written".into());
    }
    Ok(bytes)
}

pub(in crate::tools) fn plan(
    ws: &Workspace,
    args: &Value,
) -> Result<(Vec<Change>, Vec<Policy>), String> {
    let mut paths = BTreeSet::new();
    let mut total = 0usize;
    let mut changes = Vec::new();
    let mut policies = Vec::new();
    for entry in entries(args)? {
        let path = ws.resolve(req_str(entry, "path")?)?;
        if !paths.insert(path.clone())
            || paths.iter().any(|other| {
                other != &path && (path.starts_with(other) || other.starts_with(&path))
            })
        {
            return Err("write destinations duplicate or overlap; nothing was written".into());
        }
        let mut resolved = entry.clone();
        let exact_sha256 = if present(entry, "source") {
            let bytes = artifact_bytes(ws, entry, 1024 * 1024)?;
            let text = String::from_utf8(bytes).map_err(|_| "text write source is not UTF-8; use encoding=base64 for a single new binary file")?;
            resolved["content"] = json!(text);
            Some(req_str(&entry["source"], "sha256")?.to_ascii_lowercase())
        } else {
            None
        };
        total = total
            .checked_add(req_str(&resolved, "content")?.len())
            .ok_or("write size overflow")?;
        if total > MAX_TOTAL {
            return Err("write batch exceeds the 8 MiB total limit".into());
        }
        // Preserve source so write_plan knows not to strip display-like bytes from artifacts.
        let mut planned = super::files::write_plan(ws, &resolved)?;
        let change = planned
            .pop()
            .ok_or("write planner produced no destination")?;
        policies.push(Policy {
            path,
            idempotent: entry["idempotent"] == true,
            exact_sha256,
        });
        changes.push(change);
    }
    Ok((changes, policies))
}

pub(super) fn preconditions(entry: &Value, change: &Change) -> Result<(), String> {
    if entry["if_absent"] == true && change.before.is_some() {
        return Err(
            "if_absent precondition failed: destination exists; nothing was written".into(),
        );
    }
    if present(entry, "expected_sha256") {
        let expected = req_str(entry, "expected_sha256")?;
        let matches = change.before.as_ref().is_some_and(|before| {
            crate::tools::edit::edit_source::revision_hex(&before.text)
                .eq_ignore_ascii_case(expected)
        });
        if !matches {
            return Err("expected_sha256 precondition failed; nothing was written".into());
        }
    }
    Ok(())
}

pub(crate) fn receipt(ws: &Workspace, change: &Change, outcome: &str) -> Value {
    let after = change.after.as_deref().unwrap_or_default();
    json!({"path":ws.rel(&change.path),"outcome":outcome,"changed":outcome != "unchanged",
        "bytes":after.len(),"lines":crate::tools::edit::hashline::split_lines(&crate::tools::edit::hashline::normalize(after)).len(),
        "sha256":crate::tools::edit::edit_source::revision_hex(after),
        "tag":crate::tools::edit::hashline::tag_of(&crate::tools::edit::hashline::normalize(after))})
}

pub(in crate::tools) fn output(
    ws: &Workspace,
    args: &Value,
    execution: &crate::tools::edit::editing::Execution<'_>,
) -> Result<ToolOutput, String> {
    crate::tools::edit::editing::verify_targets(args)?;
    let execution = crate::tools::edit::editing::Execution {
        refuse_syntax_errors: crate::tools::edit::editing::syntax_gate(ws, args)?,
        ..*execution
    };
    let (changes, policies) = plan(ws, args)?;
    crate::tools::edit::editing::finish_write_with(ws, changes, &execution, &policies, args)
}

pub(in crate::tools) fn schema(description: &str) -> Value {
    let mut properties = json!({
        "path":{"type":"string"},
        "content":{"type":"string"},
        "source":{"type":"object","description":"Exact bytes, not a read projection; replaces content.","properties":{
            "artifact":{"type":"string"},
            "sha256":{"type":"string","minLength":64,"maxLength":64}},"required":["artifact","sha256"],"additionalProperties":false},
        "if_absent":{"type":"boolean","description":"Require missing destination, even on retries."},
        "expected_sha256":{"type":"string","minLength":64,"maxLength":64,"description":"Require this existing revision; still read first."},
        "idempotent":{"type":"boolean","description":"Witnessed equal bytes succeed unchanged; no progress credit."}
    });
    let mut entry_properties = properties.clone();
    for property in entry_properties.as_object_mut().unwrap().values_mut() {
        property.as_object_mut().unwrap().remove("description");
    }
    let entry = json!({"type":"object","properties":entry_properties,"required":["path"],"additionalProperties":false});
    properties["files"] = json!({"type":"array","minItems":1,"maxItems":MAX_FILES,"items":entry,
        "description":"Local UTF-8 batch instead of path/content/source. Rollback transaction, not crash-atomic; no binary/SSH batches."});
    properties["encoding"] = json!({"type":"string","enum":["base64"],"description":"Single NEW binary file: base64 content or raw artifact source; parent must exist."});
    properties["receipt"] = json!({"type":"boolean","description":"Compact text receipt; structured data always returned."});
    properties["allow_syntax_errors"] =
        json!({"type":"boolean","description":"Only for a parser dialect gap in valid code."});
    properties["verify"] = crate::tools::edit::editing::verify_property();
    crate::tools::tool_schema("write", description, properties, &[])
}

#[cfg(test)]
mod tests;
