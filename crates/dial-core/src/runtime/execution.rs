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
    /// Process-local controller identity, independent of provider IDs (which may be empty).
    /// Recovery uses durable runtime nodes, never these ephemeral settlement receipts.
    #[serde(skip)]
    pub invocation: Option<Invocation>,
    /// Internal accounting receipt. JSON payloads and restored sessions cannot manufacture it.
    #[serde(skip)]
    pub mutation_settled: bool,
    /// A successful native eval reset repairs only the named namespace's dependencies.
    #[serde(skip)]
    pub namespace_reset: Option<EvalNamespace>,
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
    pub shell_launch: Option<ShellLaunch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<crate::runtime::integration::IntegrationExecution>,
    /// Native capture receipts, never populated from tool payload or printed claims.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captures: Vec<NativeCapture>,
    /// Native source-witness prerequisites for retry, never inferred from diagnostic text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_edit_retry: Option<NativeEditRetry>,
}

/// One admitted native invocation and its synchronous parent. IDs are unique in this process;
/// they are not permissions, idempotency keys, or authority to replay an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub id: u64,
    pub parent: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvalNamespace {
    JavaScript,
    Python,
}

impl Invocation {
    pub fn fresh() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT
            .try_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |id| id.checked_add(1),
            )
            .expect("native invocation identity space exhausted");
        let parent = CHANGES.with(|stack| {
            stack
                .borrow()
                .last()
                .and_then(|facts| facts.invocation.map(|invocation| invocation.id))
        });
        Self { id, parent }
    }
}

/// A native edit was refused for missing source witnesses. Every requirement must be satisfied
/// by the current native witness store before the controller releases its retry barrier.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeEditRetry {
    pub requirements: Vec<NativeWitnessRequirement>,
}

/// Exact editable-source identity and inclusive, 1-based rows required by the native validator.
/// Notebook raw revisions additionally bind metadata that is absent from the editable projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeWitnessRequirement {
    pub path: PathBuf,
    pub source_sha256: [u8; 32],
    pub raw_revision: Option<[u8; 32]>,
    pub ranges: Vec<(usize, usize)>,
}

/// How the native execution owner attached the initial shell, not its descendants.
/// A launch receipt is not process termination or evidence that the requested work succeeded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellLaunch {
    pub stdin: ShellStdin,
    pub terminal_allocated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
}

/// The initial shell's controller-provided input attachment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellStdin {
    Eof,
    Pipe,
    Terminal,
}

/// Identity of actual native-captured image bytes. The agent binds these to its effect revision
/// and only credits images delivered to the model. Pixels are observations, not correctness proof.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeCapture {
    pub sha256: String,
    pub media_type: String,
    /// Exact locator: `url:<canonical URL>`, `window:<native ID>`, or `display:<native ID>`.
    /// Unknown/multi-display identity stays absent rather than inventing a target.
    pub target: Option<String>,
}

impl NativeCapture {
    pub fn new(bytes: &[u8], media_type: &str, target: Option<String>) -> Self {
        use sha2::{Digest, Sha256};
        Self {
            sha256: format!("{:x}", Sha256::digest(bytes)),
            media_type: media_type.to_owned(),
            target,
        }
    }

    pub fn matches(&self, bytes: &[u8], media_type: &str) -> bool {
        self.media_type == media_type && self.sha256 == Self::new(bytes, media_type, None).sha256
    }
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

/// What the controller permits after a failed or uncertain operation. Derived from execution
/// facts only; a diagnostic, payload or printed claim never selects or changes these values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retry {
    /// Correct the named cause (arguments, prerequisite or source) before a new attempt.
    AfterCorrection,
    /// Inspect the recorded state or process before any new attempt.
    AfterInspection,
    /// Do not replay automatically; resume only on the user's explicit direction.
    OnlyByDirection,
    /// A permission or policy boundary withholds the operation; no retry is implied.
    Withheld,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextAction {
    Correct,
    InspectProcess,
    InspectState,
    FollowDirection,
    RespectBoundary,
    ResolvePrerequisite,
}

/// Typed recovery state. Uncertain effects always stop automatic continuation, whatever status
/// was printed or recorded; a known commit is preserved rather than rolled back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recovery {
    pub status: ToolStatus,
    pub effects: Effects,
    pub committed_paths: usize,
    pub retry: Retry,
    pub next: NextAction,
}

impl Recovery {
    /// `None` when nothing needs recovery: a successful status with no uncertain effects.
    pub fn derive(status: ToolStatus, effects: Effects, committed_paths: usize) -> Option<Self> {
        if status == ToolStatus::Ok && effects != Effects::Unknown {
            return None;
        }
        let (retry, next) = if effects == Effects::Unknown {
            (Retry::AfterInspection, NextAction::InspectState)
        } else {
            match status {
                ToolStatus::Blocked => (Retry::Withheld, NextAction::RespectBoundary),
                ToolStatus::Aborted => (Retry::OnlyByDirection, NextAction::FollowDirection),
                ToolStatus::Timeout => (Retry::AfterInspection, NextAction::InspectProcess),
                ToolStatus::Skipped => (Retry::AfterCorrection, NextAction::ResolvePrerequisite),
                ToolStatus::Ok | ToolStatus::Error => (Retry::AfterCorrection, NextAction::Correct),
            }
        };
        Some(Self {
            status,
            effects,
            committed_paths,
            retry,
            next,
        })
    }
}

impl Execution {
    /// Recovery for an operation whose status the caller resolved (`ToolOutput` holds `is_error`).
    /// Distinct committed paths are counted, so repeated observations of one file do not inflate it.
    pub fn recovery(&self, status: ToolStatus) -> Option<Recovery> {
        let committed = self
            .changed_paths
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        Recovery::derive(status, self.effects, committed)
    }
}

thread_local! { static CHANGES: std::cell::RefCell<Vec<Execution>> = const { std::cell::RefCell::new(Vec::new()) }; }
/// The outer native invocation on this execution thread. A controller may carry this correlation
/// across a callback channel; it grants no authority and does not carry mutation state.
pub fn root_invocation() -> Option<u64> {
    CHANGES.with(|stack| {
        stack
            .borrow()
            .first()
            .and_then(|facts| facts.invocation.map(|invocation| invocation.id))
    })
}
/// Called by the transaction owner after a real commit, never from rendered output.
pub fn committed(paths: impl IntoIterator<Item = PathBuf>) {
    CHANGES.with(|stack| {
        let mut stack = stack.borrow_mut();
        if let Some(changes) = stack.last_mut() {
            changes.changed_paths.extend(paths);
        }
    });
}
/// Called only by native source-witness validators while the tool execution capture is active.
pub fn native_edit_retry(requirement: NativeWitnessRequirement) {
    CHANGES.with(|stack| {
        if let Some(facts) = stack.borrow_mut().last_mut() {
            let retry = facts.native_edit_retry.get_or_insert_with(Default::default);
            if !retry.requirements.contains(&requirement) {
                retry.requirements.push(requirement);
            }
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
        let invocation = Invocation::fresh();
        CHANGES.with(|stack| {
            stack.borrow_mut().push(Execution {
                invocation: Some(invocation),
                ..Default::default()
            })
        });
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
                facts.native_edit_retry = None;
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
                if let Some(retry) = changes.native_edit_retry {
                    parent
                        .native_edit_retry
                        .get_or_insert_with(Default::default)
                        .requirements
                        .extend(retry.requirements);
                }
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
