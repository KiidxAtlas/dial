//! Shared typed execution facts for tools, journals, RPC and presentation.
// Execution statuses never depend on arbitrary output prose.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Knowledge about effects is independent of why execution stopped. A timeout can leave a
/// completed write, a partial write, or no write; never infer rollback from the status alone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effects {
    None,
    #[default]
    Known,
    Unknown,
}

/// Controller-owned execution facts. Tool payload and display text never supply these facts.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Execution {
    pub status: Option<ToolStatus>,
    pub effects: Effects,
    #[serde(default)]
    pub changed_paths: Vec<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<Observation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputIntegrity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<crate::runtime::integration::IntegrationExecution>,
}

/// A successful wait/read is not evidence that the observed child succeeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    Snapshot,
    Wait,
    Readiness,
    Input,
    Termination,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub kind: ObservationKind,
    pub satisfied: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    Complete,
    Partial,
    #[default]
    Unknown,
}

/// Independent byte-stream facts. Counts exclude generated labels and truncation markers.
/// `original_bytes` counts observed bytes, not an estimate of bytes never read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputIntegrity {
    pub capture: Completeness,
    pub retention: Completeness,
    pub presentation: Completeness,
    pub original_bytes: Option<u64>,
    pub retained_bytes: Option<u64>,
    pub omitted_bytes: Option<u64>,
    pub presented_bytes: Option<u64>,
    pub artifact: Option<String>,
}

impl OutputIntegrity {
    /// A downstream projection cannot keep a producer's full-presentation claim after clipping.
    pub fn mark_presentation_partial(&mut self) {
        self.presentation = Completeness::Partial;
        self.presented_bytes = None;
    }
}

/// Observed subprocess termination, independent of printed shell output or model claims.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessOutcome {
    pub started: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub output_complete: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    #[default]
    Ok,
    Error,
    Blocked,
    Skipped,
    Timeout,
    Aborted,
}
impl ToolStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Blocked => "blocked",
            Self::Skipped => "skipped",
            Self::Timeout => "timeout",
            Self::Aborted => "aborted",
        }
    }
    pub fn failed(self) -> bool {
        matches!(self, Self::Error | Self::Timeout | Self::Aborted)
    }
}

thread_local! { static CHANGES: std::cell::RefCell<Vec<Execution>> = const { std::cell::RefCell::new(Vec::new()) }; }
/// Called by the transaction owner after a real commit, never from rendered output.
pub fn committed(paths: impl IntoIterator<Item = PathBuf>) {
    CHANGES.with(|stack| {
        let mut stack = stack.borrow_mut();
        if let Some(changes) = stack.last_mut() {
            changes.changed_paths.extend(paths);
        }
    });
}

/// The mutation owner could not restore a definite state. This survives callers catching errors.
pub fn unsettled() {
    CHANGES.with(|stack| {
        if let Some(facts) = stack.borrow_mut().last_mut() {
            facts.effects = Effects::Unknown;
        }
    });
}

/// A caller may catch the subprocess error, but cannot turn uncertain interrupted effects into
/// an ordinary success or erase the physical termination category.
pub(crate) fn process_status(status: ToolStatus) {
    CHANGES.with(|stack| {
        if let Some(facts) = stack.borrow_mut().last_mut() {
            facts.status = merge_status(facts.status, status);
        }
    });
}

pub(crate) fn process_failure(status: ToolStatus) {
    CHANGES.with(|stack| {
        if let Some(facts) = stack.borrow_mut().last_mut() {
            facts.effects = Effects::Unknown;
            facts.status = merge_status(facts.status, status);
        }
    });
}

fn merge_status(previous: Option<ToolStatus>, incoming: ToolStatus) -> Option<ToolStatus> {
    let rank = |status| match status {
        ToolStatus::Aborted => 3,
        ToolStatus::Timeout => 2,
        ToolStatus::Error => 1,
        _ => 0,
    };
    if previous.is_none_or(|previous| rank(incoming) >= rank(previous)) {
        Some(incoming)
    } else {
        previous
    }
}

pub struct Capture;
impl Capture {
    pub fn begin() -> Self {
        CHANGES.with(|stack| stack.borrow_mut().push(Default::default()));
        Self
    }
    pub fn finish(self, recovered: bool) -> Execution {
        let changes = CHANGES.with(|stack| {
            let mut stack = stack.borrow_mut();
            if recovered
                && let Some(facts) = stack.last_mut()
                && facts.effects != Effects::Unknown
            {
                facts.status = None;
            }
            stack.last().cloned().unwrap_or_default()
        });
        drop(self);
        changes
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        CHANGES.with(|stack| {
            let mut stack = stack.borrow_mut();
            if let Some(changes) = stack.pop()
                && let Some(parent) = stack.last_mut()
            {
                parent.changed_paths.extend(changes.changed_paths);
                if changes.effects == Effects::Unknown {
                    parent.effects = Effects::Unknown;
                }
                if let Some(status) = changes.status {
                    parent.status = merge_status(parent.status, status);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests;
