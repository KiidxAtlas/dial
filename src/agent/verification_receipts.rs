//! Execution-status receipts for focused verification checks.
//!
//! A receipt binds the observed status of named check commands to a digest of the changed source
//! files. It is execution evidence only: it does not prove the change is correct or complete. It never
//! stores source text, command output, or an unredacted command.

use super::verification::CheckStatus;
use crate::storage::{redact::redact_text, session::Session};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Component, Path},
};

pub(super) const MAX_RECEIPTS: usize = 20;
const MAX_IDENTITY_FILES: usize = 256;
const MAX_HASHED_BYTES: u64 = 64 * 1024 * 1024;
const CLAIM: &str = "Execution status only: records the observed status of named check commands for one source snapshot. Not proof of correctness, coverage, or absence of defects.";

/// One check's observed result. `ran` is false when interruption skipped the check.
pub(super) struct Outcome {
    pub name: String,
    pub command: String,
    pub status: CheckStatus,
    pub ran: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SourceIdentity {
    pub digest: String,
    pub files: usize,
    /// False when any file was oversized, unreadable, non-regular, outside the workspace, or beyond the file cap.
    pub complete: bool,
}

/// Digest of sorted relative paths and their current content digests. No source text is retained.
pub(super) fn source_identity(root: &Path, paths: &[String]) -> SourceIdentity {
    let mut sorted: Vec<&str> = paths.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut complete = sorted.len() <= MAX_IDENTITY_FILES;
    let mut digest = Sha256::new();
    digest.update(b"dial-verification-source-v1\n");
    for path in sorted.iter().take(MAX_IDENTITY_FILES) {
        let (state, file_complete) = file_state(root, path);
        complete &= file_complete;
        // Length-prefixing keeps paths containing separators unambiguous.
        digest.update(format!("{}:{}|{}\n", path.len(), path, state).as_bytes());
    }
    digest.update(format!("files:{}\n", sorted.len()).as_bytes());
    SourceIdentity {
        digest: format!("{:x}", digest.finalize()),
        files: sorted.len(),
        complete,
    }
}

fn file_state(root: &Path, relative: &str) -> (String, bool) {
    let rel = Path::new(relative);
    let components: Vec<_> = rel.components().collect();
    if components.is_empty()
        || components
            .iter()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return ("outside-workspace".into(), false);
    }
    let mut path = root.to_path_buf();
    for (index, part) in components.iter().enumerate() {
        path.push(part.as_os_str());
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return ("absent".into(), true);
            }
            Err(_) => return ("unreadable".into(), false),
        };
        if meta.file_type().is_symlink() {
            return ("symlink".into(), false);
        }
        if index + 1 < components.len() {
            if !meta.is_dir() {
                return ("non-directory-parent".into(), false);
            }
        } else if !meta.file_type().is_file() {
            return ("non-regular".into(), false);
        }
    }
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(_) => return ("unreadable".into(), false),
    };
    if meta.len() > MAX_HASHED_BYTES {
        return (format!("oversized:{}", meta.len()), false);
    }
    let Ok(file) = fs::File::open(&path) else {
        return ("unreadable".into(), false);
    };
    // Read one byte past the cap so a file that grows during hashing is detected, not truncated silently.
    let mut reader = file.take(MAX_HASHED_BYTES + 1);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    let mut total = 0u64;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                total += read as u64;
                hasher.update(&buffer[..read]);
            }
            Err(_) => return ("unreadable".into(), false),
        }
    }
    if total > MAX_HASHED_BYTES {
        return (format!("oversized:{total}"), false);
    }
    (format!("sha256:{:x}", hasher.finalize()), true)
}

fn overall(outcomes: &[Outcome], stable: bool) -> &'static str {
    if outcomes.is_empty() {
        "no_checks"
    } else if outcomes.iter().any(|o| o.status == CheckStatus::Cancelled) {
        "cancelled"
    } else if !stable {
        "source_changed"
    } else if outcomes.iter().all(|o| o.status == CheckStatus::Passed) {
        "passed"
    } else {
        "failed"
    }
}

/// Build one receipt. Only names, redacted commands, statuses and digests are included.
pub(super) fn receipt(
    before: &SourceIdentity,
    after: &SourceIdentity,
    outcomes: &[Outcome],
) -> Value {
    let stable = before.digest == after.digest;
    let checks: Vec<Value> = outcomes
        .iter()
        .map(|outcome| {
            let command = redact_text(&outcome.command).into_owned();
            json!({
                "name": outcome.name,
                "command": command,
                "command_redacted": command != outcome.command,
                "status": outcome.status,
                "ran": outcome.ran,
            })
        })
        .collect();
    json!({
        "recorded_at": Session::exploration_timestamp(),
        "evidence": "execution_status_only",
        "claim": CLAIM,
        "source": {
            "algorithm": "sha256",
            "digest": before.digest,
            "digest_after": after.digest,
            "files": before.files,
            "identity_complete": before.complete && after.complete,
            "stable": stable,
        },
        "checks": checks,
        "overall": overall(outcomes, stable),
    })
}

/// Append a receipt, keeping only the newest `MAX_RECEIPTS`.
pub(super) fn append(reliability: &mut Value, receipt: Value) {
    let mut history = reliability["verification_receipts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    history.push(receipt);
    let excess = history.len().saturating_sub(MAX_RECEIPTS);
    history.drain(..excess);
    reliability["verification_receipts"] = Value::Array(history);
}

#[cfg(test)]
mod tests;
