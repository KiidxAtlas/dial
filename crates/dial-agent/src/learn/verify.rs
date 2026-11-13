//! Deterministic, declarative task verification. A `Check` looks at the final workspace and the
//! agent's final answer and returns pass/fail: no model is involved, which is what makes outcomes
//! produced with it externally grounded (for gate cases, self-play and training data alike).
use crate::runtime::{execution::ProcessOutcome, process};
use crate::tools::Workspace;
#[cfg(test)]
use crate::tools::{self, exec::sandbox::Policy};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Duration,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Check {
    /// Operator-owned checks restored after execution, never taken from solver edits.
    Repository {
        files: std::collections::BTreeMap<String, String>,
        command: String,
    },
    /// Final answer equals `expected` after trimming whitespace.
    AnswerExact {
        expected: String,
    },
    AnswerContains {
        needle: String,
    },
    /// Final answer parses as JSON equal to `expected`.
    AnswerJson {
        expected: Value,
    },
    /// File content equals `content` ignoring trailing whitespace.
    FileEquals {
        path: String,
        content: String,
    },
    FileContains {
        path: String,
        needle: String,
    },
    FileAbsent {
        path: String,
    },
    /// JSON file whose value at JSON-pointer `pointer` equals `expected`.
    JsonFile {
        path: String,
        pointer: String,
        expected: Value,
    },
    /// Run `command` with bash in the workspace under the strict sandbox; stdout (trimmed) must equal
    /// `expect_stdout` when given and the exit status must be `expect_exit`.
    Command {
        command: String,
        #[serde(default)]
        expect_stdout: Option<String>,
        #[serde(default)]
        expect_exit: i32,
    },
    /// Every nested check must pass.
    All {
        checks: Vec<Check>,
    },
}

impl Check {
    /// Verdict for callers that cannot act on a launch failure: a command that could not run counts
    /// as failed. Callers that must tell the two apart use [`Check::try_verify`].
    pub fn verify(&self, workspace: &Path, answer: &str) -> bool {
        self.try_verify(workspace, answer).unwrap_or(false)
    }

    /// `Ok(verdict)` once the checks have run. `Err` means a command check could not be launched, so
    /// there is no verdict; a command that runs and fails is `Ok(false)`.
    pub fn try_verify(&self, workspace: &Path, answer: &str) -> Result<bool> {
        Ok(match self {
            Check::Repository { files, command } => {
                super::repository::verify(workspace, files, command)?
            }
            Check::AnswerExact { expected } => answer.trim() == expected.trim(),
            Check::AnswerContains { needle } => answer.contains(needle.as_str()),
            Check::AnswerJson { expected } => {
                serde_json::from_str::<Value>(answer.trim()).is_ok_and(|v| &v == expected)
            }
            Check::FileEquals { path, content } => {
                read(workspace, path).is_some_and(|c| c.trim_end() == content.trim_end())
            }
            Check::FileContains { path, needle } => {
                read(workspace, path).is_some_and(|c| c.contains(needle.as_str()))
            }
            // Only a definite NotFound proves absence; any other metadata error is unavailable.
            Check::FileAbsent { path } => match confine(workspace, path) {
                Some(p) => matches!(
                    fs::symlink_metadata(p),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound
                ),
                None => false,
            },
            Check::JsonFile {
                path,
                pointer,
                expected,
            } => read(workspace, path)
                .and_then(|c| serde_json::from_str::<Value>(&c).ok())
                .is_some_and(|v| v.pointer(pointer) == Some(expected)),
            Check::Command {
                command,
                expect_stdout,
                expect_exit,
            } => run_command(workspace, command, expect_stdout.as_deref(), *expect_exit)?,
            Check::All { checks } => {
                if checks.is_empty() {
                    return Ok(false);
                }
                for check in checks {
                    if !check.try_verify(workspace, answer)? {
                        return Ok(false);
                    }
                }
                true
            }
        })
    }
}

/// A path under `root` with no `..`, no absolute prefix and no symlinked final component.
fn confine(root: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    if rel.is_empty()
        || rel.contains('\0')
        || !p.components().all(|c| matches!(c, Component::Normal(_)))
    {
        return None;
    }
    let full = root.join(p);
    // A symlink anywhere along the way could point outside the workspace.
    let mut probe = root.to_path_buf();
    for part in p.components() {
        probe.push(part);
        match fs::symlink_metadata(&probe) {
            Ok(meta) if meta.file_type().is_symlink() => return None,
            Ok(_) => {}
            // A missing ancestor means the target is absent, not that the walk failed.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            // Unreadable or otherwise unknown components cannot be confined.
            Err(_) => return None,
        }
    }
    Some(full)
}

fn read(root: &Path, rel: &str) -> Option<String> {
    let path = confine(root, rel)?;
    if fs::metadata(&path).ok()?.len() > 4 * 1024 * 1024 {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn run_command(
    workspace: &Path,
    command: &str,
    expect_stdout: Option<&str>,
    expect_exit: i32,
) -> Result<bool> {
    // Workspace::new applies the tool-call admission boundary: a canonical directory, not the
    // filesystem root. Its error means verification never ran.
    let ws = Workspace::new(workspace).context("verification workspace is unavailable")?;
    // The shell owner chooses the configured backend (embedded by default) and the strict sandbox,
    // and owns the child environment and scratch directory. Raw streams are read here, not the
    // tool's merged text with status and artifact diagnostics.
    let child = crate::tools::exec::shell::verifier_command(&ws, command)
        .map_err(anyhow::Error::msg)
        .context("verification command could not be built")?;
    let mut policy = process::Policy::bounded(Duration::from_secs(30), 4 * 1024 * 1024);
    policy.fail_on_output_limit = true;
    // A spawn failure is an Err from the process runner; a command that ran and failed is a verdict.
    let out =
        process::run_with_policy(child, None, policy, &AtomicBool::new(false), &mut |_, _| {})
            .map_err(anyhow::Error::msg)
            .context("verification command could not be launched")?;
    Ok(command_matches(
        &out.outcome(),
        &out.stdout,
        expect_stdout,
        expect_exit,
    ))
}

fn command_matches(
    outcome: &ProcessOutcome,
    stdout: &[u8],
    expect_stdout: Option<&str>,
    expect_exit: i32,
) -> bool {
    outcome.started
        && outcome.output_complete
        && !outcome.timed_out
        && !outcome.cancelled
        && outcome.signal.is_none()
        && outcome.exit_code == Some(expect_exit)
        && expect_stdout.is_none_or(|expected| {
            std::str::from_utf8(stdout).is_ok_and(|actual| actual.trim() == expected.trim())
        })
}

#[cfg(test)]
mod tests;
