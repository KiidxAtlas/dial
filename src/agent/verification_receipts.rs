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
mod tests {
    use super::*;

    fn outcome(status: CheckStatus, ran: bool) -> Outcome {
        Outcome {
            name: "syntax".into(),
            command: "python -m py_compile a.py".into(),
            status,
            ran,
        }
    }

    #[test]
    fn identity_binds_content_and_absence_and_ignores_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        let forward = source_identity(dir.path(), &["a.py".into(), "b.py".into()]);
        let reversed = source_identity(dir.path(), &["b.py".into(), "a.py".into()]);
        assert_eq!(forward, reversed);
        assert!(forward.complete);
        assert!(!forward.digest.contains("x = 1"));

        std::fs::write(dir.path().join("b.py"), "y = 2\n").unwrap();
        let with_b = source_identity(dir.path(), &["a.py".into(), "b.py".into()]);
        assert_ne!(
            forward.digest, with_b.digest,
            "created file must change identity"
        );

        std::fs::write(dir.path().join("a.py"), "x = 3\n").unwrap();
        let edited = source_identity(dir.path(), &["a.py".into(), "b.py".into()]);
        assert_ne!(
            with_b.digest, edited.digest,
            "content edit must change identity"
        );
    }

    #[test]
    fn escaping_paths_mark_identity_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let identity = source_identity(dir.path(), &["../outside.py".into()]);
        assert!(!identity.complete);
    }
    #[cfg(unix)]
    #[test]
    fn symlinked_parent_paths_are_never_hashed_as_workspace_files() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "private contents").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).unwrap();
        let identity = source_identity(dir.path(), &["linked/secret.txt".into()]);
        assert!(!identity.complete);
        assert!(!identity.digest.contains("private contents"));
    }

    #[test]
    fn receipt_labels_status_and_never_carries_output() {
        let identity = source_identity(tempfile::tempdir().unwrap().path(), &[]);
        let passed = receipt(&identity, &identity, &[outcome(CheckStatus::Passed, true)]);
        assert_eq!(passed["overall"], "passed");
        assert_eq!(passed["evidence"], "execution_status_only");
        assert!(passed.to_string().contains("Not proof of correctness"));
        assert!(passed.get("output").is_none() && passed["checks"][0].get("output").is_none());

        let cancelled = receipt(
            &identity,
            &identity,
            &[
                outcome(CheckStatus::Passed, true),
                outcome(CheckStatus::Cancelled, false),
            ],
        );
        assert_eq!(cancelled["overall"], "cancelled");
        assert_eq!(cancelled["checks"][1]["ran"], false);

        let changed = source_identity(tempfile::tempdir().unwrap().path(), &["gone.py".into()]);
        let drifted = receipt(&identity, &changed, &[outcome(CheckStatus::Passed, true)]);
        assert_eq!(drifted["overall"], "source_changed");
        assert_eq!(drifted["source"]["stable"], false);
    }

    #[test]
    fn history_is_bounded_to_newest_receipts() {
        let mut state = json!({"verification":"kept"});
        for seq in 0..(MAX_RECEIPTS as u64 + 5) {
            append(&mut state, json!({"seq": seq}));
        }
        let history = state["verification_receipts"].as_array().unwrap();
        assert_eq!(history.len(), MAX_RECEIPTS);
        assert_eq!(history[0]["seq"], 5);
        assert_eq!(state["verification"], "kept");
    }
}
