//! Built-in, model-facing tools: read, write, edit, glob, grep and bash.
//!
//! Every file path a model supplies is resolved against the workspace root:
//! `..` escapes, absolute paths outside the root and symlinks that lead out of
//! it are rejected, and directory walks (glob, grep) never follow symlinks.
//! `bash` runs inside the platform sandbox (macOS sandbox-exec or Linux/WSL2 bubblewrap;
//! see [`sandbox`]) and refuses to run at all when no sandbox is available.
//!
//! Residual limits, stated plainly:
//! * Path checks and later file operations are not atomic. Tools run one at a
//!   time, but a process the model leaves running between calls could swap a
//!   symlink in the window; bash is killed at the end of every call, so only a
//!   process that escapes its process group (e.g. via `setsid`) could do so.
//!   The sandbox still bounds what such a process can reach.
//! * Hard links to files outside the workspace cannot be detected by path.
//!   The sandbox forbids creating them from inside bash, but a pre-existing
//!   hard link in the workspace would be readable.
//!   Likewise, pre-existing hard links to protected Dial files are not identified by path checks.
//! * Native protected-path checks do not confine external language servers, plugins, remote SSH
//!   tools, or unsandboxed shell commands; those need their own execution/approval boundary.
//! * Timeouts kill the bash process group. A command that calls `setsid` can
//!   outlive the call (still sandboxed, but orphaned).

macro_rules! notebook_edit_description {($description:literal)=>{concat!($description,"\nNotebooks: ordinary .ipynb reads expose cell source as # %% [code|markdown|raw] cell:N (zero-based). Keep indexed markers to retain authentic cells; unindexed # %% markers create cells. Preserve source-literal marker lines with one extra %, e.g. # %%%. All edit modes operate on this projection and retain untouched raw JSON metadata, number lexemes and outputs; changed code cells clear stale outputs/execution counts. Metadata-only changes invalidate witnesses. Read :raw for literal JSON; whole-file write requires actual JSON, not the source projection. Notebook moves/deletes need complete relevant source witnesses.")};}

pub mod archive;
pub mod artifact;
pub mod ast;
pub mod autoresearch;
pub mod background;
pub mod bash_interceptor;
pub mod browser;
pub mod catalog;
pub mod contracts;
pub mod dap;
pub mod diagnostics;
mod diff;
mod document_transaction;
pub mod documents;
mod edit_native;
pub(crate) mod edit_source;
mod edit_transaction;
pub mod editing;
pub mod eval;
pub(crate) mod execution;
pub mod ext;
mod files;
pub mod gh;
mod gh_cache;
mod gh_common;
mod gh_pr;
mod gh_pr_diff;
mod gh_run_watch;
mod gh_search;
mod gh_view;
pub mod git;
pub(crate) mod git_patch;
pub mod hashline;
pub mod kernel;
pub mod lsp;
pub mod mcp;
pub mod navigation;
pub mod notebook;
pub mod outcome;
pub mod plugin;
pub mod preludes;
pub mod presentation;
pub mod preview;
pub mod resources;
pub mod sandbox;
mod search;
mod select;
pub mod shell;
pub mod signals;
pub mod speculation;
pub mod ssh;
pub mod syntax;
#[cfg(test)]
mod testkit;
pub(crate) mod walk;
pub mod web;

use crate::config::types::SandboxConfig;
use anyhow::{Context, Result, bail, ensure};
use sandbox::Sandbox;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

pub const BUILTIN: &[&str] = &["read", "write", "edit", "glob", "grep", "bash"];

/// dial's own tools beyond the file/shell built-ins.
pub const EXTRA_TOOLS: &[&str] = &[
    "todo",
    "finish",
    "task",
    "solve",
    "escalate",
    "skill",
    "skill_manage",
    "session_search",
    "tool_search",
    "tool_load",
    "artifact_read",
    "working_state",
    "message",
    "recall",
    "retain",
    "memory_update",
    "forget",
    "memory_history",
    "memory_import",
    "context_notes",
    "new_context",
    "checkpoint",
    "rewind",
    "ask",
    "tts",
];

/// Integration tool names accepted in the config's `integrations` list (see [`ext`]). Most are
/// offered only when listed; `find`, `browser`, `kernel`, `eval`, `autoresearch` and `archive` are
/// always offered, so older saved lists keep them.
pub const INTEGRATION_TOOLS: &[&str] = &[
    "bg",
    "web_fetch",
    "web_search",
    "lsp",
    "ast_search",
    "ast_edit",
    "dap",
    "git",
    "find",
    "browser",
    "kernel",
    "eval",
    "github",
    "notebook",
    "ssh",
    "security_scan",
    "autoresearch",
    "archive",
    "computer",
];

/// Tools dial enables by default: the built-ins plus the core dial tools below. The rest of
/// [`EXTRA_TOOLS`] stays opt-in through the harness or tool flags.
pub fn default_tools() -> Vec<String> {
    BUILTIN
        .iter()
        .chain(
            [
                "todo",
                "task",
                "skill",
                "tool_search",
                "tool_load",
                "artifact_read",
                "working_state",
                "recall",
                "retain",
                "memory_update",
            ]
            .iter(),
        )
        .map(|s| s.to_string())
        .collect()
}

/// Selected tools withheld from the default per-request offer. They stay in the catalog, so the
/// system prompt lists them under "Loadable tools" and `tool_load` (or `tool_search`) enables them
/// for the next round; schemas and historical contracts are unchanged. Each overlaps a tool that
/// stays offered: `memory_update` corrects a fact by the id and revision only `recall` supplies
/// (`retain` records new facts), and `ast_search` is the structural variant of `grep`/`find`,
/// joining `ast_edit`, which was already loadable. An operator-pinned `[harness] tools` list, a
/// hermetic run and the selection lent to subagents keep every selected tool offered.
pub const DEFERRED_BY_DEFAULT: &[&str] = &["memory_update", "ast_search"];

const MAX_LIST: usize = 500;

/// Most bytes of one tool result the agent keeps in the conversation. The built-in tools cap their own
/// output near this and say how to fetch the rest; the agent applies [`model_output_limit`] on top.
pub const MODEL_OUTPUT_LIMIT: usize = 48 * 1024;
/// Floor for [`model_output_limit`]: below this a compiler error with its location no longer fits.
const MIN_MODEL_OUTPUT: usize = 4 * 1024;

/// The per-result byte limit for a model with `context_window` tokens: about an eighth of the window
/// (at ~4 bytes per token) between 4 KiB and [`MODEL_OUTPUT_LIMIT`]. A fixed 48 KiB result is about
/// 12k tokens, more than an 8k local model's context; full text stays retrievable from artifacts.
pub fn model_output_limit(context_window: u32) -> usize {
    (context_window as usize / 2).clamp(MIN_MODEL_OUTPUT, MODEL_OUTPUT_LIMIT)
}

/// Bytes of one tool result the model sees inline. Successful inspections used to be cut to 8 KiB when
/// an artifact held the rest; that made the model fetch most grep, test and build output in a second
/// round, so every result now gets the full per-model limit. `efficiency.tool_preview_bytes` can
/// still reduce retained successful inspections for small-window models.
pub fn preview_limit(window: u32, tool: &str, failed: bool, retained: bool) -> usize {
    let _ = (tool, failed, retained);
    model_output_limit(window)
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    #[test]
    fn compact_previews_require_retrievable_successful_observations() {
        assert_eq!(
            preview_limit(128000, "read", false, true),
            MODEL_OUTPUT_LIMIT
        );
        assert_eq!(
            preview_limit(128000, "read", false, false),
            MODEL_OUTPUT_LIMIT
        );
        assert_eq!(
            preview_limit(128000, "bash", true, true),
            MODEL_OUTPUT_LIMIT
        );
        assert_eq!(
            preview_limit(128000, "edit", false, true),
            MODEL_OUTPUT_LIMIT
        );
        assert_eq!(preview_limit(8000, "read", false, true), 4096);
        assert_eq!(
            preview_limit(128000, "grep", false, true),
            MODEL_OUTPUT_LIMIT
        );
    }
}

/// Per-workspace scratch directory exported as TMPDIR for bash; walks skip it.
pub const SCRATCH_DIR: &str = ".tmp";

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
    /// Original structured result for clients; never recover richer objects by parsing `text`.
    pub data: Option<Value>,
    /// Execution metadata is independent of tool payload, hooks and presentation.
    pub execution: outcome::Execution,
}

pub use outcome::ToolStatus;

impl ToolOutput {
    pub fn with_effects(mut self, effects: outcome::Effects) -> Self {
        self.execution.effects = effects;
        if effects == outcome::Effects::Unknown {
            self.is_error = true;
            const NOTE: &str = "\n[Effects unknown: work may have partially completed. Inspect the affected state before retrying; interruption does not imply rollback.]";
            if !self.text.ends_with(NOTE) {
                self.text.push_str(NOTE);
            }
        }
        self
    }
    /// Annotate a controller-created outcome without interpreting arbitrary prose.
    pub fn process_outcome(&self) -> Option<crate::runtime::execution::ProcessOutcome> {
        self.data
            .as_ref()?
            .get("process")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    }
    pub fn with_process(mut self, process: crate::runtime::execution::ProcessOutcome) -> Self {
        let data = self.data.get_or_insert_with(|| serde_json::json!({}));
        if let Some(data) = data.as_object_mut() {
            data.insert("process".into(), serde_json::json!(process));
        }
        self
    }
    pub fn with_status(mut self, status: ToolStatus) -> Self {
        self.execution.status = Some(status);
        self.is_error |= status != ToolStatus::Ok;
        self
    }

    pub fn with_changes(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.execution.changed_paths = paths.into_iter().collect();
        self
    }

    pub fn structured(data: Value) -> Self {
        Self {
            text: data.to_string(),
            is_error: false,
            data: Some(data),
            execution: Default::default(),
        }
    }
    pub fn ok(text: String) -> Self {
        Self {
            data: None,
            text,
            is_error: false,
            execution: Default::default(),
        }
    }

    pub fn err(text: impl Into<String>) -> Self {
        Self {
            data: None,
            text: text.into(),
            is_error: true,
            execution: Default::default(),
        }
    }
}

use crate::config::types::EditMode;
pub use editing::{EditRepair, EditRepairRequest, EditSettings, EditToggle};
pub use editing::{declared_targets as declared_edit_targets, targets as editing_targets};
pub use editing::{
    inline_payload as inline_edit_payload, instructions as editing_instructions,
    schema as editing_schema,
};

/// Called with the canonical absolute path BEFORE any file is created,
/// overwritten, moved or deleted by the file tools. An `Err` aborts the
/// mutation (and, for a multi-file patch, everything not yet written).
pub trait WriteObserver: Send + Sync {
    fn before_write(&self, abs: &Path) -> Result<()>;
    fn before_delete(&self, abs: &Path) -> Result<()> {
        self.before_write(abs)
    }
}

#[derive(Clone, Default)]
struct ObserverSlot(Option<Arc<dyn WriteObserver>>);

impl std::fmt::Debug for ObserverSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "Some(WriteObserver)"
        } else {
            "None"
        })
    }
}

#[derive(Debug, Clone)]
pub struct Workspace {
    syntax_guard: bool,
    root: PathBuf,
    additional_roots: Vec<PathBuf>,
    /// Private application trees excluded independently of shell sandbox mode.
    protected_paths: Vec<PathBuf>,
    sandbox: Option<Sandbox>,
    allow_network: bool,
    policy: Option<sandbox::Policy>,
    unsandboxed: bool,
    /// Shell that runs `bash` commands (`shell.backend`).
    shell_backend: crate::config::types::ShellBackend,
    edit_mode: EditMode,
    edit_settings: EditSettings,
    /// Bash intent interceptor and the tool names offered this session (a redirect applies only
    /// when its target is offered). `None` leaves bash commands unchecked.
    bash_interceptor: Option<(bash_interceptor::BashInterceptorConfig, Vec<String>)>,
    /// User SSH settings (`Config.ssh`) applied to `ssh://` reads and writes this session.
    ssh: ssh::SshConfig,
    /// Connection targets for `ssh://` hosts, keyed by name (see [`Workspace::with_ssh_hosts`]).
    ssh_hosts: Arc<BTreeMap<String, ssh::Target>>,
    edit_repair: editing::RepairSlot,
    edit_blackbox_dir: Option<PathBuf>,
    native_edits: Arc<edit_native::store::EditStore>,
    observer: ObserverSlot,
    /// Session snapshots / seen lines / registers for hashline; shared by clones.
    edits: Arc<Mutex<hashline::EditStore>>,
    artifacts: Option<artifact::ArtifactStore>,
    operations: Arc<Mutex<()>>,
    shell_sessions: Arc<Mutex<BTreeMap<String, crate::embedded_shell::state::State>>>,
    pub(crate) speculation: Arc<speculation::Pool>,
}

impl Workspace {
    pub fn new(root: &Path) -> Result<Workspace> {
        let root = root
            .canonicalize()
            .with_context(|| format!("workspace {} does not exist", root.display()))?;
        ensure!(
            root.is_dir(),
            "workspace {} is not a directory",
            root.display()
        );
        ensure!(
            root.parent().is_some(),
            "the filesystem root cannot be a workspace"
        );
        Ok(Workspace {
            syntax_guard: false,
            root,
            additional_roots: Vec::new(),
            protected_paths: Vec::new(),
            sandbox: None,
            allow_network: false,
            policy: None,
            unsandboxed: false,
            shell_backend: crate::config::types::ShellBackend::Auto,
            edit_mode: EditMode::default(),
            edit_settings: EditSettings::default(),
            bash_interceptor: None,
            ssh: ssh::SshConfig::default(),
            ssh_hosts: Arc::default(),
            edit_repair: editing::RepairSlot::default(),
            edit_blackbox_dir: None,
            native_edits: Arc::default(),
            observer: ObserverSlot::default(),
            edits: Arc::default(),
            artifacts: None,
            operations: Arc::default(),
            shell_sessions: Arc::default(),
            speculation: Arc::default(),
        })
    }

    pub(crate) fn speculative_workspace(&self) -> Self {
        let mut isolated = self.clone();
        isolated.speculation = Arc::default();
        isolated.native_edits = Arc::default();
        isolated.edits = Arc::default();
        isolated.observer = ObserverSlot::default();
        isolated.artifacts = None;
        isolated
    }
    pub fn with_artifacts(mut self, store: artifact::ArtifactStore) -> Self {
        self.artifacts = Some(store);
        self
    }
    pub fn artifacts(&self) -> Option<&artifact::ArtifactStore> {
        self.artifacts.as_ref()
    }
    pub(crate) fn serial_operation(&self) -> MutexGuard<'_, ()> {
        self.operations.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(crate) fn try_serial_operation(&self) -> Option<MutexGuard<'_, ()>> {
        match self.operations.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => Some(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    /// Override sandbox detection (tests, or an operator who pre-probed).
    pub fn with_sandbox(mut self, sandbox: Sandbox) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    /// Permit network access from bash. Off by default.
    pub fn with_network(mut self, allow: bool) -> Self {
        self.allow_network = allow;
        self
    }

    /// Run bash under an explicit sandbox policy (strict or developer).
    pub fn with_policy(mut self, policy: sandbox::Policy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// The sandbox policy in force at exec time: the configured policy, with every attached root
    /// granted the same write access as the primary root. Attached roots are folded in here, not
    /// stored in the policy, so `add_root` takes effect on the next command.
    pub fn sandbox_policy(&self) -> sandbox::Policy {
        let roots = self.additional_roots.iter().cloned();
        match &self.policy {
            Some(sandbox::Policy::Dev {
                deny_read,
                extra_write,
                allow_network,
            }) => sandbox::Policy::Dev {
                deny_read: deny_read
                    .iter()
                    .chain(&self.protected_paths)
                    .cloned()
                    .collect(),
                extra_write: extra_write.iter().cloned().chain(roots).collect(),
                allow_network: *allow_network,
            },
            Some(sandbox::Policy::Strict {
                allow_network,
                deny_read,
                extra_write,
            }) => sandbox::Policy::Strict {
                allow_network: *allow_network,
                deny_read: deny_read
                    .iter()
                    .chain(&self.protected_paths)
                    .cloned()
                    .collect(),
                extra_write: extra_write.iter().cloned().chain(roots).collect(),
            },
            None => sandbox::Policy::Strict {
                allow_network: self.allow_network,
                deny_read: self.protected_paths.clone(),
                extra_write: roots.collect(),
            },
        }
    }

    /// Run bash with no sandbox, only a scrubbed environment. An explicit operator opt-out.
    pub fn without_sandbox(mut self) -> Self {
        self.unsandboxed = true;
        self
    }

    /// Select the shell that runs `bash` commands (`shell.backend`).
    pub fn with_shell_backend(mut self, backend: crate::config::types::ShellBackend) -> Self {
        self.shell_backend = backend;
        self
    }

    /// Choose the `edit`/`read` contract (default: hashline).
    pub fn with_edit_mode(mut self, mode: EditMode) -> Self {
        self.edit_mode = mode;
        self
    }

    pub fn with_edit_settings(mut self, settings: EditSettings) -> Self {
        self.edit_mode = settings.mode;
        self.edit_settings = settings;
        self
    }
    pub fn edit_settings(&self) -> &EditSettings {
        &self.edit_settings
    }
    /// Enables the bash intent interceptor for this session. `available_tools` are the tool names
    /// offered to the model; a redirect is applied only when its target is among them.
    pub fn with_bash_interceptor(
        mut self,
        config: bash_interceptor::BashInterceptorConfig,
        available_tools: Vec<String>,
    ) -> Self {
        self.bash_interceptor = Some((config, available_tools));
        self
    }
    pub fn bash_interceptor(
        &self,
    ) -> Option<&(bash_interceptor::BashInterceptorConfig, Vec<String>)> {
        self.bash_interceptor.as_ref()
    }
    /// Applies the user's SSH settings (`Config.ssh`) to `ssh://` reads and writes.
    pub fn with_ssh_config(mut self, ssh: ssh::SshConfig) -> Self {
        self.ssh = ssh;
        self
    }
    pub fn ssh_config(&self) -> &ssh::SshConfig {
        &self.ssh
    }
    /// Applies the `[ssh_hosts]` table, merged with `~/.ssh/config` aliases, to `ssh://` targets.
    pub fn with_ssh_hosts(
        mut self,
        configured: &BTreeMap<String, crate::config::types::SshHostConfig>,
    ) -> Self {
        self.ssh_hosts = Arc::new(match std::env::var_os("HOME").map(PathBuf::from) {
            Some(home) => ssh::host_targets(&home, configured),
            None => BTreeMap::new(),
        });
        self
    }
    pub fn ssh_hosts(&self) -> &BTreeMap<String, ssh::Target> {
        &self.ssh_hosts
    }
    pub fn with_edit_repair(mut self, repair: Arc<dyn EditRepair>) -> Self {
        self.edit_repair = editing::RepairSlot(Some(repair));
        self
    }
    pub fn with_edit_blackbox_dir(mut self, path: PathBuf) -> Self {
        self.edit_blackbox_dir = Some(path);
        self
    }

    /// Install the observer that sees every file mutation before it happens.
    pub fn with_observer(mut self, observer: Arc<dyn WriteObserver>) -> Self {
        self.observer = ObserverSlot(Some(observer));
        self
    }

    /// The installed write observer, shared with a child that edits the same project directly.
    pub fn observer(&self) -> Option<Arc<dyn WriteObserver>> {
        self.observer.0.clone()
    }

    pub fn edit_mode(&self) -> EditMode {
        self.edit_mode
    }

    pub fn with_syntax_guard(mut self, enabled: bool) -> Self {
        self.syntax_guard = enabled;
        self
    }
    pub(crate) fn syntax_guard_enabled(&self) -> bool {
        self.syntax_guard
    }
    pub(crate) fn syntax_advisory(
        &self,
        path: &Path,
        before: &str,
        proposed: &str,
    ) -> Option<String> {
        if self.syntax_guard {
            syntax::advisory(path, before, proposed)
        } else {
            None
        }
    }

    /// The same sandbox, edit policy, and SSH settings over another directory, such as an isolated
    /// project copy. Attached roots, edit history, and the write observer are not shared: the copy
    /// reaches the project only through an explicit merge.
    pub fn rebased(&self, root: &Path) -> Result<Workspace> {
        let mut copy = Workspace::new(root)?;
        copy.sandbox = self.sandbox.clone();
        copy.allow_network = self.allow_network;
        copy.policy = self.policy.clone();
        copy.unsandboxed = self.unsandboxed;
        copy.bash_interceptor = self.bash_interceptor.clone();
        copy.edit_mode = self.edit_mode;
        copy.edit_settings = self.edit_settings.clone();
        copy.edit_repair = self.edit_repair.clone();
        copy.edit_blackbox_dir = self.edit_blackbox_dir.clone();
        copy.syntax_guard = self.syntax_guard;
        copy.ssh = self.ssh.clone();
        copy.ssh_hosts = Arc::clone(&self.ssh_hosts);
        copy.artifacts = self.artifacts.clone();
        Ok(copy)
    }

    /// The same session configuration (sandbox, edit mode, SSH, artifacts) over another primary
    /// root, for `/move` in a live session. Attached roots belonged to the old project and are dropped.
    pub fn reroot(&self, root: &Path) -> Result<Workspace> {
        let fresh = Workspace::new(root)?;
        let mut next = self.clone();
        next.root = fresh.root;
        next.additional_roots.clear();
        Ok(next)
    }

    pub(crate) fn store(&self) -> MutexGuard<'_, hashline::EditStore> {
        self.edits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    /// Record immutable search/read evidence; callers pass only complete verbatim source rows.
    pub fn record_edit_witnesses(
        &self,
        path: &Path,
        source: &str,
        complete_ranges: &[(usize, usize)],
    ) -> Result<(), String> {
        if complete_ranges.is_empty() {
            return Ok(());
        }
        if source.len() > hashline::MAX_FILE as usize {
            return Err("edit witness source exceeds 4 MiB".into());
        }
        let resolved = self.resolve(&self.rel(path))?;
        if resolved != path {
            return Err("edit witness target is not a canonical workspace path".into());
        }
        let lf = hashline::normalize(source);
        let count = hashline::split_lines(&lf).len();
        if complete_ranges
            .iter()
            .any(|&(a, b)| a == 0 || b < a || b > count)
        {
            return Err("edit witness ranges are out of bounds".into());
        }
        if lf.len() > hashline::MAX_FILE as usize {
            return Err("edit witness source exceeds 4 MiB".into());
        }
        let current =
            edit_transaction::read(self, path)?.ok_or("edit witness target no longer exists")?;
        if current.text != source {
            return Err("edit witness source changed before publication; not recorded".into());
        }
        self.store().record(path, &lf, complete_ranges);
        Ok(())
    }
    /// Complete source rows returned by the owner-local notebook cell reader.
    pub fn record_notebook_cell_witnesses(
        &self,
        path: &Path,
        raw: &str,
        index: usize,
        coverage: &[(usize, usize)],
    ) -> Result<(), String> {
        edit_source::record_cell(self, path, raw, index, coverage)
    }

    pub(crate) fn observe_write(&self, abs: &Path) -> Result<(), String> {
        self.check_protected_mutation(abs)?;
        navigation::invalidate(abs);
        match &self.observer.0 {
            Some(observer) => observer
                .before_write(abs)
                .map_err(|e| format!("write to {:?} blocked by the harness: {e:#}", self.rel(abs))),
            None => Ok(()),
        }
    }

    pub(crate) fn observe_delete(&self, abs: &Path) -> Result<(), String> {
        self.check_protected_mutation(abs)?;
        navigation::invalidate(abs);
        match &self.observer.0 {
            Some(observer) => observer.before_delete(abs).map_err(|e| {
                format!(
                    "delete of {:?} blocked by the harness: {e:#}",
                    self.rel(abs)
                )
            }),
            None => Ok(()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every root this workspace may touch: the primary root first, then the
    /// additional roots attached with `with_additional_roots`.
    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.root.as_path())
            .chain(self.additional_roots.iter().map(PathBuf::as_path))
    }

    /// Exclude private trees from native tools, including when the shell sandbox is off.
    /// Missing tails are retained so protection also covers state created later.
    pub fn with_protected_paths(mut self, paths: &[PathBuf]) -> Self {
        self.protected_paths = paths
            .iter()
            .map(|path| {
                let absolute = if path.is_absolute() {
                    path.clone()
                } else {
                    self.root.join(path)
                };
                Self::canonical_prefix(&absolute)
            })
            .collect();
        self
    }

    pub(crate) fn is_protected(&self, path: &Path) -> bool {
        if self.protected_paths.is_empty() {
            return false;
        }
        let canonical = Self::canonical_prefix(path);
        self.protected_paths.iter().any(|protected| {
            path.starts_with(protected)
                || canonical.starts_with(protected)
                || canonical.starts_with(Self::canonical_prefix(protected))
        })
    }

    fn check_protected_mutation(&self, path: &Path) -> Result<(), String> {
        if self.protected_paths.is_empty() {
            return Ok(());
        }
        let canonical = Self::canonical_prefix(path);
        if self.is_protected(path)
            || self.protected_paths.iter().any(|protected| {
                protected.starts_with(path)
                    || Self::canonical_prefix(protected).starts_with(&canonical)
            })
        {
            return Err("mutation would affect protected Dial storage".into());
        }
        Ok(())
    }

    /// Resolve a model-supplied path to a canonical-prefixed absolute path that
    /// lies inside one of the workspace roots. Relative paths resolve against the
    /// primary root; absolute paths must fall under some root. The target need not
    /// exist; existing components are resolved through symlinks and re-checked.
    pub fn resolve(&self, input: &str) -> Result<PathBuf, String> {
        if input.contains('\0') {
            return Err("path contains a NUL byte".to_string());
        }
        let path = Path::new(input);
        let (root, relative): (&Path, PathBuf) = if path.is_absolute() {
            let within = |candidate: &Path| {
                self.roots().find_map(|root| {
                    candidate
                        .strip_prefix(root)
                        .ok()
                        .map(|rest| (root, rest.to_path_buf()))
                })
            };
            // Roots are canonical (`/private/var` on macOS); input may name the same place through a symlink.
            within(path)
                .or_else(|| within(&Self::canonical_prefix(path)))
                .ok_or_else(|| {
                    // Name the directory to add, so the model can ask for it instead of giving up.
                    let project = path
                        .ancestors()
                        .skip(1)
                        .find(|dir| dir.join(".git").exists())
                        .or_else(|| path.parent())
                        .unwrap_or(path);
                    format!(
                        "path {input:?} is outside the workspace; use a path relative to the workspace root or an added directory. To work there, ask the user to run `/add-dir {}`",
                        project.display()
                    )
                })?
        } else {
            (self.root.as_path(), path.to_path_buf())
        };
        let mut current = root.to_path_buf();
        if self.is_protected(&current) {
            return Err(format!("path {input:?} enters protected Dial storage"));
        }
        let mut missing = false;
        for component in relative.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if missing || current.as_path() == root {
                        return Err(format!("path {input:?} escapes the workspace"));
                    }
                    current.pop();
                }
                Component::Normal(name) => {
                    current.push(name);
                    if self.is_protected(&current) {
                        return Err(format!("path {input:?} enters protected Dial storage"));
                    }
                    if missing {
                        continue;
                    }
                    match fs::symlink_metadata(&current) {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            current = fs::canonicalize(&current).map_err(|_| {
                                format!("path {input:?} goes through a dangling symlink")
                            })?;
                            if !current.starts_with(root) {
                                return Err(format!(
                                    "path {input:?} follows a symlink out of the workspace"
                                ));
                            }
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == io::ErrorKind::NotFound => missing = true,
                        Err(e) => return Err(format!("cannot access {input:?}: {e}")),
                    }
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(format!("path {input:?} is not a valid workspace path"));
                }
            }
            if self.is_protected(&current) {
                return Err(format!("path {input:?} enters protected Dial storage"));
            }
        }
        Ok(current)
    }

    /// Attach extra directories the session may read and write alongside the primary
    /// root. Each must be an existing directory; duplicates and the primary root are skipped.
    pub fn with_additional_roots(mut self, dirs: &[PathBuf]) -> Result<Self> {
        for dir in dirs {
            let canonical = Self::canonical_dir(dir)?;
            if canonical != self.root {
                self.add_root(&canonical)?;
            }
        }
        Ok(self)
    }

    /// Extra roots attached to this workspace, in the order they were added.
    pub fn additional_roots(&self) -> &[PathBuf] {
        &self.additional_roots
    }

    /// Attach one directory at run time. Returns `false` when it is already a root and
    /// refuses the primary root, which is always present.
    pub fn add_root(&mut self, dir: &Path) -> Result<bool> {
        let canonical = Self::canonical_dir(dir)?;
        ensure!(
            canonical != self.root,
            "{} is already the primary workspace root",
            canonical.display()
        );
        if self.additional_roots.contains(&canonical) {
            return Ok(false);
        }
        self.additional_roots.push(canonical);
        Ok(true)
    }

    /// Detach an extra root. Returns `false` when it is not attached. The primary root
    /// cannot be removed. A directory that no longer exists can still be detached by path.
    pub fn remove_root(&mut self, dir: &Path) -> Result<bool> {
        let target = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        ensure!(
            target != self.root,
            "{} is the primary workspace root and cannot be removed",
            target.display()
        );
        let before = self.additional_roots.len();
        self.additional_roots.retain(|root| *root != target);
        Ok(self.additional_roots.len() != before)
    }

    fn canonical_dir(dir: &Path) -> Result<PathBuf> {
        let canonical = dir
            .canonicalize()
            .with_context(|| format!("workspace directory {} does not exist", dir.display()))?;
        ensure!(
            canonical.is_dir(),
            "workspace directory {} is not a directory",
            canonical.display()
        );
        Ok(canonical)
    }

    /// `path` with its deepest existing ancestor canonicalized and the missing tail re-appended, so an
    /// absolute path named through a symlink (`/var` → `/private/var`) compares against canonical roots.
    fn canonical_prefix(path: &Path) -> PathBuf {
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let mut head = path.to_path_buf();
        loop {
            if let Ok(canonical) = head.canonicalize() {
                return tail
                    .iter()
                    .rev()
                    .fold(canonical, |acc, part| acc.join(part));
            }
            match (
                head.file_name().map(ToOwned::to_owned),
                head.parent().map(Path::to_path_buf),
            ) {
                (Some(name), Some(parent)) => {
                    tail.push(name);
                    head = parent;
                }
                _ => return path.to_path_buf(),
            }
        }
    }

    /// Bounded, workspace-confined identity for a read observation, including selectors.
    /// Used only to expire recovery feedback; this is not verification evidence.
    pub(crate) fn read_identity(&self, input: &str) -> Option<[u8; 32]> {
        let (path, _) = select::split_target(self, input).ok()?;
        self.file_identity(Path::new(path))
            .map(|(_, digest)| digest)
    }

    /// Canonical workspace path and SHA-256 of an exact file path (never a read selector, since
    /// file names may contain `:`). `None` for paths outside every root, protected storage,
    /// non-UTF-8 names, missing or unreadable targets, directories and files over 16 MiB.
    /// Recovery bookkeeping only; this is not verification evidence.
    pub(crate) fn file_identity(&self, input: &Path) -> Option<(PathBuf, [u8; 32])> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        const LIMIT: u64 = 16 * 1024 * 1024;
        let path = self.resolve(input.to_str()?).ok()?;
        let metadata = fs::metadata(&path).ok()?;
        if !metadata.is_file() || metadata.len() > LIMIT {
            return None;
        }
        let file = fs::File::open(&path).ok()?;
        let mut data = Vec::new();
        file.take(LIMIT + 1).read_to_end(&mut data).ok()?;
        (data.len() as u64 <= LIMIT).then(|| (path, Sha256::digest(&data).into()))
    }

    pub(crate) fn rel(&self, path: &Path) -> String {
        match path.strip_prefix(&self.root) {
            Ok(rest) if rest.as_os_str().is_empty() => ".".to_string(),
            Ok(rest) => rest.to_string_lossy().into_owned(),
            Err(_) => path.to_string_lossy().into_owned(),
        }
    }

    /// The workspace root that contains `path` (already resolved by [`Self::resolve`]), preferring
    /// the deepest one when an added root sits inside another. `None` when no root contains it.
    pub(crate) fn root_containing(&self, path: &Path) -> Option<&Path> {
        self.roots()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
    }
}

pub(crate) fn tool_schema(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            },
        },
    })
}

const HASHLINE_READ_DESC: &str = "Read a text file (or list a directory). Output starts with a header `[PATH#TAG]` \
followed by `LINE:TEXT` rows; copy that header's TAG into `edit` sections. Select lines by appending to the path \
`:50-100`, `:50+20` (20 lines from 50), `:50-` (from 50), `:-30` (last 30 lines) or comma-joined ranges such as \
`:5-16,960-973`; or use offset/limit (not together with a selector). A footer states the lines returned and the \
file's total line count. \
`:raw` (e.g. `src/a.rs:raw:1-40`) returns verbatim text without anchors; raw output cannot be edited. \
Without a range, output is capped at 48 KiB and the footer names the range to read next, so read large files in \
ranges. `edit` may only touch lines you have been shown.";

const HASHLINE_WRITE_DESC: &str = "Create a new file or replace a whole file (max 1 MiB). BEFORE replacing an existing file, read its complete current contents; prefer edit for changes. Missing parent directories are created. Notebook content is literal JSON, not the editable # %% source projection. In hashline mode, complete copied read headers/numbered prefixes are stripped; incomplete/truncated projections reject. Existing files require complete source witnesses unless the operator disables the seen-line guard. Returns the fresh `[PATH#TAG]` header.";

const HASHLINE_EDIT_DESC: &str = notebook_edit_description!(
    r#"Edit existing files with a line-anchored patch (use `write` to create or overwrite whole files).
`input` holds one or more sections, each a header `[PATH#TAG]` followed by operations. TAG is the 4-hex tag from the latest `read`, `edit` or `write` result for that file. Line numbers always refer to THAT snapshot, never to earlier operations in the same call, and you may only touch lines a read has shown you.
Operations (N and M are line numbers):
PUT N.=M:   replace lines N..M with the `+` rows that follow (N.=N for one line)
PUT <N:     insert the `+` rows before line N   (PUT <1: = top of file)
PUT >N:     insert after line N                  (PUT >$: = end of file)
CUT N.=M    delete lines N..M
PUT N*:     replace the whole block that starts at line N (function, class, { } or [ ] body, indented body, markdown section)
PUT >N*:    insert after that block;   CUT N* deletes it
REM         delete the file;   MV new/path   move/rename it (after any edits)
Body rows: every row starts with `+` followed by the exact text including indentation. A lone `+` is a blank line. Text that itself starts with + or - is written `++x` or `+-x`. Rows are the final text, not a diff; never write `-` rows.
Moving code: `CUT 3.=5 @tmp` then `PUT >9 @tmp` (works across files in one call; `PUT N.=M @tmp` replaces with it; without @name one anonymous register is used).
Example:
[src/main.rs#3FA9]
PUT 4.=4:
+    let total = a + b;
CUT 10.=12
PUT >$:
+// end
Put all changes to one file in one section. On success you get the fresh [PATH#TAG] and the changed lines; use that tag next time. If the file changed since your read, the edit is re-anchored only when the lines it touches are unchanged; otherwise you receive the current lines so you can retry."#
);

const REPLACE_READ_DESC: &str = "Read a text file in the workspace with line numbers (or list a directory). \
Select lines by appending to the path `:50-100`, `:50+150` (150 lines from 50), `:50-` (from 50), `:-60` (last 60 lines) \
or comma-joined ranges such as `:5-16,960-973`; or use offset/limit (not together with a selector). A footer states \
the lines returned and the file's total line count. Without a range, output is capped at 48 KiB and the footer names \
the range to read next, so read large files in ranges.";

fn schema(name: &str, mode: EditMode) -> Option<Value> {
    let hashline = mode == EditMode::Hashline;
    Some(match name {
        "read" => {
            let mut schema = tool_schema(
                "read",
                if hashline {
                    HASHLINE_READ_DESC
                } else {
                    REPLACE_READ_DESC
                },
                json!({
                    "path": {"type": "string", "description": "Workspace file/directory or `ssh://host/path` remote file. SSH URL reads require explicit approval; absolute and `~/` remote paths supported. Use paths instead to read several targets in one call."},
                    "offset": {"type": "integer", "minimum": 1, "description": "1-based line to start at (default 1); not together with a path selector."},
                    "limit": {"type": "integer", "minimum": 1, "description": "Maximum number of lines (default 2000)."},
                    "paths": {"type": "array", "items": {"type": "string"}, "maxItems": 16, "description": "Several targets read in one call, each a path with its own optional selector (e.g. `src/a.rs:1-80`); results come back in order. Use instead of path."},
                }),
                &[],
            );
            schema["function"]["parameters"]["minProperties"] = json!(1);
            schema["function"]["parameters"]["properties"]["paths"]["minItems"] = json!(1);
            schema
        }
        "write" => tool_schema(
            "write",
            if hashline {
                HASHLINE_WRITE_DESC
            } else {
                "Create or overwrite a file in the workspace with the given content (max 1 MiB). \
                 Missing parent directories are created."
            },
            json!({
                "path": {"type": "string", "description": "Workspace-relative file path or an `ssh://host/path` remote file (requires approval)."},
                "content": {"type": "string", "description": "Full file content."},
            }),
            &["path", "content"],
        ),
        "edit" => editing::schema(mode),
        "glob" => tool_schema(
            "glob",
            "List pageable workspace files in sorted component/depth-first order, max 500 per page. Supports \
             `*`, `?`, `[a-z]`, `{a,b}` and `**`; bare patterns match names at any depth. \
             Coverage and JSON next_cursor/remaining_skip are always reported. Copy them to cursor/skip \
             with the same query and unchanged workspace. Directories over 50,000 immediate entries are \
             explicitly omitted before sorting; narrow the scope to their children. Directory enumeration \
             is bounded to 2,050,000 entries/call. Symlinks, .git and scratch are never searched.",
            json!({
                "pattern": {"type": "string", "description": "Glob such as `src/**/*.rs` or `*.py`, relative to the workspace root."},
                "path": {"type": "string", "description": "File/directory scope, or semicolon-separated scopes; an existing literal filename wins. Default workspace root."},
                "hidden": {"type": "boolean", "description": "Include hidden entries (default true)."},
                "gitignore": {"type": "boolean", "description": "Honor local ignore rules and skip dependency/build directories (default true); false includes them, but never .git or scratch."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 500, "description": "Maximum paths per page (default 500)."},
                "skip": {"type": "integer", "minimum": 0, "description": "Skip this many matching paths in deterministic file order (default 0); when resuming use remaining_skip from the footer."},
                "cursor": {"type": "object", "properties": {"path": {"type": "string"}, "match": {"type": "integer", "minimum": 0, "maximum": 0}}, "required": ["path", "match"], "additionalProperties": false, "description": "Inclusive workspace-relative continuation position copied from next_cursor, not a pattern. Null next_cursor means traversal completed within the reported coverage."},
            }),
            &["pattern"],
        ),
        "grep" => tool_schema(
            "grep",
            "Search with Rust regex (no lookaround/backreferences) or fixed_string. Default line matching \
             returns path:line:text; context uses path-line-text and -- groups. Newline patterns or multiline \
             match whole files and return path:start:end:text with inclusive source lines. Pages are ordered \
             by sorted component/depth-first files then matching lines/non-overlapping matches. Always inspect \
             coverage and JSON next_cursor/remaining_skip; copy to cursor/skip with the same query and unchanged \
             workspace. Limits: 50,000 considered walk entries, 2,050,000 directory entries enumerated, \
             64 MiB scanned, 2 MiB/file and 48 KiB result rows. Directories over 50,000 immediate entries \
             are explicitly omitted before sorting; narrow to children. Skipped/unreadable/ignored/depth-capped \
             coverage is explicit, never exhaustive empty success.",
            json!({
                "pattern": {"type": "string", "description": "Regular expression (Rust `regex` syntax, e.g. `fn \\w+\\(` or `TODO|FIXME`; non-empty). Set fixed_string to search literal text instead."},
                "fixed_string": {"type": "boolean", "description": "Treat pattern as literal text, not a regex (default false)."},
                "path": {"type": "string", "description": "Workspace file/directory or one `ssh://host/path` remote file (requires approval); semicolon-separated local scopes are supported."},
                "glob": {"type": "string", "description": "Only search files whose workspace-relative path matches this glob."},
                "ignore_case": {"type": "boolean", "description": "Case-insensitive match (default false)."},
                "context": {"type": "integer", "minimum": 0, "description": "Lines of context to show before and after each match (default 0; values above 10 are capped at 10)."},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Maximum number of matching lines (default 400)."},
                "multiline": {"type": "boolean", "description": "Match whole-file bytes (default false, automatic for actual newlines or regex \\n). Anchors match each line; use (?s) to make dot match newline."},
                "hidden": {"type": "boolean", "description": "Include hidden entries (default true)."},
                "gitignore": {"type": "boolean", "description": "Honor local ignore rules and skip dependency/build directories (default true); false includes them, never .git/scratch/symlinks."},
                "skip": {"type": "integer", "minimum": 0, "description": "Skip matching lines, or non-overlapping multiline matches, in deterministic order. Default 0; use remaining_skip when resuming a capped scan."},
                "cursor": {"type": "object", "properties": {"path": {"type": "string"}, "match": {"type": "integer", "minimum": 0}}, "required": ["path", "match"], "additionalProperties": false, "description": "Inclusive file position and already consumed result count from next_cursor. Copy unchanged; null next_cursor means traversal completed within reported coverage."},
            }),
            &["pattern"],
        ),
        "bash" => tool_schema(
            "bash",
            if cfg!(windows) {
                "Run a bash command in the workspace root, or in `cwd` when supplied. It runs in Git for Windows bash when installed, otherwise in Dial's built-in bash-compatible shell; only when `shell.backend = \"powershell\"` is it Windows PowerShell source. Windows filesystem/network isolation is unavailable; execution requires `sandbox.mode = \"off\"` and runs with the current user's privileges. stdout and stderr are merged; large output keeps its beginning and end. Commands run non-interactively without a controlling terminal. Use `background: true` for long-running work; check or stop it with the bg tool."
            } else {
                "Run a shell command with `/bin/bash -c` in the workspace root, or in `cwd` when supplied. It may run inside a sandbox: writes are confined to the project and added directories, some paths outside them can be unreadable, and the network may be blocked. Temporary files belong in `$TMPDIR`. stdout and stderr are merged; large output keeps its beginning and end. Commands run non-interactively without a controlling terminal. Use `background: true` for long-running work; check or stop it with the bg tool."
            },
            json!({
                "command": {"type": "string", "description": "Shell command to run."},
                "cwd": {"type": "string", "description": "Working directory: relative to the primary workspace root, or absolute inside the primary root or an added workspace root. Default is the project root: do not prefix commands with `cd <root> &&`."},
                "session": {"type":"string", "description":"With the embedded backend, retain cwd and exported string variables between completed foreground commands in this named workspace session. Does not retain functions, unexported variables or background jobs. Cancelled/timed-out exchanges invalidate the state."},
                "timeout": {"type": "integer", "minimum": 1, "description": "Timeout in seconds, capped by the configured bash timeout."},
                "background": {"type": "boolean", "description": "Start this command as a managed background job now (servers, watchers, long builds). Commands still running after ~15s move to background automatically. Check or stop jobs with the bg tool."},
                "async": {"type": "boolean", "description": "Alias of background."},
            }),
            &["command"],
        ),
        _ => return None,
    })
}

pub fn schemas(names: &[&str], mode: EditMode) -> Result<Vec<Value>> {
    names
        .iter()
        .map(|name| match schema(name, mode) {
            Some(value) => Ok(value),
            None => bail!("unknown tool {name:?}; built-in tools are {BUILTIN:?}"),
        })
        .collect()
}

/// Run one tool call. Never panics: failures become `is_error` output.
pub fn execute(ws: &Workspace, name: &str, args: &Value, timeout_secs: u64) -> ToolOutput {
    execution::run(&std::sync::atomic::AtomicBool::new(false), || {
        dispatch(ws, name, args, timeout_secs).unwrap_or_else(ToolOutput::err)
    })
}

/// Read through the same confined parser pipeline while allowing managed
/// converter/cache misses to observe the execution owner's cancellation.
pub fn read_tool_streaming(
    ws: &Workspace,
    args: &Value,
    cancel: &std::sync::atomic::AtomicBool,
) -> ToolOutput {
    if !args.is_object() {
        return ToolOutput::err("tool arguments must be a JSON object");
    }
    execution::run(cancel, || {
        match files::read_tool_streaming(ws, args, cancel) {
            Ok(text) => ToolOutput::ok(text),
            Err(error) => ToolOutput::err(error),
        }
    })
}

/// Preview uses the same complete payload preparation but never repairs or writes.
pub fn preview_edit(ws: &Workspace, args: &Value) -> ToolOutput {
    if let Err(error) = editing::validate_shape(ws.edit_mode, args) {
        return ToolOutput::err(error);
    }
    let mut preview = ws.clone();
    preview.native_edits = Arc::new(edit_native::store::EditStore::default());
    preview.edits = Arc::new(Mutex::new(ws.store().clone()));
    let no_change = std::cell::Cell::new(false);
    let execution = editing::Execution {
        preview_no_change: Some(&no_change),
        ..Default::default()
    };
    let result = if ws.edit_mode == EditMode::Hashline {
        hashline::input_arg(args)
            .and_then(|input| hashline::edit_with(&preview, input, false, &execution))
    } else {
        editing::prepare_with_preview(&preview, args, Some(&no_change))
            .and_then(|changes| editing::finish_with(&preview, changes, false, &execution))
    };
    let mut output = match result {
        Ok(text) => ToolOutput::ok(text),
        Err(error) => ToolOutput::err(error),
    };
    output.data = Some(json!({"edit_preview":{"no_change":no_change.get()}}));
    output
}
/// Preview of a partially streamed edit: an incomplete apply_patch envelope is not an error.
pub fn preview_edit_streaming(ws: &Workspace, args: &Value) -> ToolOutput {
    if let Err(error) = editing::validate_shape(ws.edit_mode, args) {
        return ToolOutput::err(error);
    }
    let mut preview = ws.clone();
    preview.native_edits = Arc::new(edit_native::store::EditStore::default());
    preview.edits = Arc::new(Mutex::new(ws.store().clone()));
    let no_change = std::cell::Cell::new(false);
    let execution = editing::Execution {
        preview_no_change: Some(&no_change),
        ..Default::default()
    };
    let result = if ws.edit_mode == EditMode::Hashline {
        hashline::input_arg(args)
            .and_then(|input| hashline::edit_with(&preview, input, false, &execution))
    } else {
        editing::prepare_streaming_preview(&preview, args, Some(&no_change))
            .and_then(|changes| editing::finish_with(&preview, changes, false, &execution))
    };
    let mut output = match result {
        Ok(text) => ToolOutput::ok(text),
        Err(error) => ToolOutput::err(error),
    };
    output.data = Some(json!({"edit_preview":{"no_change":no_change.get()}}));
    output
}
/// Only complete native/planner failures abort generation; already-satisfied edits do not.
pub fn preview_edit_should_abort(output: &ToolOutput) -> bool {
    output.is_error
        && !output
            .data
            .as_ref()
            .and_then(|data| data["edit_preview"]["no_change"].as_bool())
            .unwrap_or(false)
}
fn bind_edit_targets(
    ws: &Workspace,
    paths: Vec<String>,
) -> Result<Vec<(String, PathBuf, PathBuf)>, String> {
    paths
        .into_iter()
        .map(|literal| {
            let canonical = ws.resolve(&literal)?;
            let path = Path::new(&literal);
            let lexical = if path.is_absolute() {
                path.to_path_buf()
            } else {
                ws.root().join(path)
            };
            Ok((literal, canonical, lexical))
        })
        .collect()
}
fn authorize_edit_targets(
    ctx: &ext::ToolContext<'_>,
    name: &str,
    bound: &[(String, PathBuf, PathBuf)],
    paths: &[PathBuf],
) -> Result<(), String> {
    let mut all = paths.to_vec();
    for (literal, expected, lexical) in bound {
        if ctx.workspace.resolve(literal)? != *expected || !paths.contains(expected) {
            return Err(format!(
                "mutation target {literal:?} changed resolution while preparing the edit; read and retry"
            ));
        }
        all.push(lexical.clone());
        all.push(expected.clone());
    }
    all.sort();
    all.dedup();
    ctx.authorize_mutation(name, &all, false)
}
/// Controller entry point: real owner permission/cancellation is checked again after model repair.
/// The owner retains the workspace serial barrier; this function does not acquire it recursively.
pub fn edit_with_context(
    ctx: &ext::ToolContext<'_>,
    args: &Value,
    repair: Option<&dyn Fn(&EditRepairRequest) -> Result<String, String>>,
    cancel: &std::sync::atomic::AtomicBool,
) -> ToolOutput {
    edit_for_workspace(ctx, ctx.workspace, args, repair, cancel)
}
/// Only the trusted terminal-recovery branch calls this; ordinary schemas stay configured.
pub fn edit_recovered_with_context(
    ctx: &ext::ToolContext<'_>,
    args: &Value,
    repair: Option<&dyn Fn(&EditRepairRequest) -> Result<String, String>>,
    cancel: &std::sync::atomic::AtomicBool,
) -> ToolOutput {
    let workspace = ctx.workspace.clone().with_edit_mode(EditMode::Sloppy);
    edit_for_workspace(ctx, &workspace, args, repair, cancel)
}
fn repair_source_authority(
    ctx: &ext::ToolContext<'_>,
    bound: &[(String, PathBuf, PathBuf)],
    request: &EditRepairRequest,
) -> Result<(), String> {
    if request.before.is_empty() {
        return Ok(());
    }
    authorize_source_targets(ctx, bound)
}
fn authorize_source_targets(
    ctx: &ext::ToolContext<'_>,
    bound: &[(String, PathBuf, PathBuf)],
) -> Result<(), String> {
    let mut paths = Vec::new();
    for (literal, expected, lexical) in bound {
        if ctx.workspace.resolve(literal)? != *expected {
            return Err("source target changed while preparing edit".into());
        }
        if expected.is_file() {
            paths.push(expected.clone());
            paths.push(lexical.clone());
        }
    }
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        Ok(())
    } else {
        ctx.authorize_mutation("read", &paths, false)
    }
}
fn edit_for_workspace(
    ctx: &ext::ToolContext<'_>,
    workspace: &Workspace,
    args: &Value,
    repair: Option<&dyn Fn(&EditRepairRequest) -> Result<String, String>>,
    cancel: &std::sync::atomic::AtomicBool,
) -> ToolOutput {
    let bound = match editing::targets(workspace.edit_mode, args)
        .and_then(|paths| bind_edit_targets(workspace, paths))
    {
        Ok(bound) => bound,
        Err(error) => return ToolOutput::err(error),
    };
    if let Err(error) = authorize_source_targets(ctx, &bound) {
        return ToolOutput::err(error);
    }
    let authorize = |paths: &[PathBuf]| authorize_edit_targets(ctx, "edit", &bound, paths);
    let guarded_repair = |request: &EditRepairRequest| {
        repair_source_authority(ctx, &bound, request)?;
        if let Some(callback) = repair {
            callback(request)
        } else {
            workspace
                .edit_repair
                .0
                .as_ref()
                .ok_or_else(|| "no repair callback".to_owned())?
                .repair(request)
        }
    };
    let effective_repair = (repair.is_some() || workspace.edit_repair.0.is_some())
        .then_some(&guarded_repair as &dyn Fn(&EditRepairRequest) -> Result<String, String>);
    let execution = editing::Execution {
        repair: effective_repair,
        authorize: Some(&authorize),
        cancel: Some(cancel),
        preview_no_change: None,
    };
    let result = if workspace.edit_mode == EditMode::Hashline {
        hashline::input_arg(args)
            .and_then(|input| hashline::edit_with(workspace, input, true, &execution))
    } else {
        editing::prepare(workspace, args)
            .and_then(|changes| editing::finish_with(workspace, changes, true, &execution))
    };
    match result {
        Ok(text) => ToolOutput::ok(text),
        Err(error) => ToolOutput::err(error),
    }
}
pub fn write_with_context(
    ctx: &ext::ToolContext<'_>,
    args: &Value,
    repair: Option<&dyn Fn(&EditRepairRequest) -> Result<String, String>>,
    cancel: &std::sync::atomic::AtomicBool,
) -> ToolOutput {
    if let Some(path) = args
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| ssh::url::is_ssh_url(path))
    {
        let content = match args.get("content").and_then(Value::as_str) {
            Some(content) => content,
            None => return ToolOutput::err("missing string argument \"content\""),
        };
        if let Err(error) = ctx.authorize_mutation("write", &[], true) {
            return ToolOutput::err(error);
        }
        let user_dir = match ssh::user_dir() {
            Ok(path) => path,
            Err(error) => return ToolOutput::err(error),
        };
        return match ssh::write_url(
            path,
            &user_dir,
            ctx.workspace.ssh_config(),
            ctx.workspace.ssh_hosts(),
            content.as_bytes(),
            cancel,
        ) {
            Ok(()) => ToolOutput::ok(format!("wrote {} bytes to {path}", content.len())),
            Err(error) => ToolOutput::err(error),
        };
    }
    let bound = match req_str(args, "path")
        .and_then(|path| bind_edit_targets(ctx.workspace, vec![path.to_owned()]))
    {
        Ok(bound) => bound,
        Err(error) => return ToolOutput::err(error),
    };
    let authorize = |paths: &[PathBuf]| authorize_edit_targets(ctx, "write", &bound, paths);
    let guarded_repair = |request: &EditRepairRequest| {
        repair_source_authority(ctx, &bound, request)?;
        if let Some(callback) = repair {
            callback(request)
        } else {
            ctx.workspace
                .edit_repair
                .0
                .as_ref()
                .ok_or_else(|| "no repair callback".to_owned())?
                .repair(request)
        }
    };
    let effective_repair = (repair.is_some() || ctx.workspace.edit_repair.0.is_some())
        .then_some(&guarded_repair as &dyn Fn(&EditRepairRequest) -> Result<String, String>);
    let execution = editing::Execution {
        repair: effective_repair,
        authorize: Some(&authorize),
        cancel: Some(cancel),
        preview_no_change: None,
    };
    match files::write_with(ctx.workspace, args, &execution) {
        Ok(text) => ToolOutput::ok(text),
        Err(error) => ToolOutput::err(error),
    }
}

fn dispatch(
    ws: &Workspace,
    name: &str,
    args: &Value,
    timeout_secs: u64,
) -> Result<ToolOutput, String> {
    if !args.is_object() {
        return Err("tool arguments must be a JSON object".to_string());
    }
    match name {
        "read" => files::read_tool(ws, args).map(ToolOutput::ok),
        "write" => files::write_tool(ws, args).map(ToolOutput::ok),
        "edit" => match ws.edit_mode {
            EditMode::Hashline => editing::validate_shape(ws.edit_mode, args)
                .and_then(|_| hashline::input_arg(args))
                .and_then(|input| hashline::edit(ws, input))
                .map(ToolOutput::ok),
            _ => editing::edit(ws, args).map(ToolOutput::ok),
        },
        "glob" => search::glob_tool(ws, args).map(ToolOutput::ok),
        "grep" => search::grep_tool(ws, args).map(ToolOutput::ok),
        "bash" => shell::bash_tool(ws, args, timeout_secs),
        _ => Err(format!(
            "unknown tool {name:?}; available tools: {}",
            BUILTIN.join(", ")
        )),
    }
}

/// A model-callable argument is present only when set to a non-null value. Strict-tools schemas make
/// every optional property nullable, so `null` means omitted, never a value.
pub(crate) fn present(args: &Value, key: &str) -> bool {
    args.get(key).is_some_and(|value| !value.is_null())
}

pub(crate) fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    match args.get(key) {
        None | Some(Value::Null) => Err(format!("missing required argument {key:?}")),
        Some(Value::String(text)) => Ok(text),
        Some(_) => Err(format!("argument {key:?} must be a string")),
    }
}

pub(crate) fn opt_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(format!("argument {key:?} must be a string")),
    }
}

pub(crate) fn opt_u64(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("argument {key:?} must be a non-negative integer")),
    }
}

pub(crate) fn opt_bool(args: &Value, key: &str) -> Result<bool, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(format!("argument {key:?} must be a boolean")),
    }
}

fn io_message(action: &str, path: &str, error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::NotFound => format!("{action} {path:?}: no such file or directory"),
        io::ErrorKind::PermissionDenied => format!("{action} {path:?}: permission denied"),
        _ => format!("{action} {path:?}: {error}"),
    }
}

/// Apply the configured shell sandbox. `protected` is (dial state dir, dial user dir): in developer
/// mode bash may read most of the machine but never those or common credential directories.
pub fn apply_sandbox_config(
    ws: Workspace,
    config: &SandboxConfig,
    protected: Option<(&Path, &Path)>,
) -> Result<Workspace> {
    use crate::config::types::SandboxMode;
    use sandbox::Policy;
    let ws = match protected {
        Some((state, user)) => ws.with_protected_paths(&[state.to_owned(), user.to_owned()]),
        None => ws,
    };
    Ok(match config.mode {
        SandboxMode::Off => ws.without_sandbox(),
        SandboxMode::Strict => ws.with_policy(Policy::Strict {
            allow_network: false,
            deny_read: Vec::new(),
            extra_write: Vec::new(),
        }),
        SandboxMode::Dev => {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let mut deny_read: Vec<PathBuf> = Vec::new();
            if let Some((state, user)) = protected {
                deny_read.push(state.to_owned());
                deny_read.push(user.to_owned());
            }
            if let Some(home) = &home {
                for rel in [
                    ".ssh",
                    ".aws",
                    ".gnupg",
                    ".config/gcloud",
                    ".azure",
                    ".kube",
                    ".docker",
                    "Library/Keychains",
                ] {
                    deny_read.push(home.join(rel));
                }
            }
            let extra_write = config
                .extra_write
                .iter()
                .map(|p| match (p.strip_prefix("~"), &home) {
                    (Ok(rest), Some(home)) => home.join(rest),
                    _ => p.clone(),
                })
                .collect();
            ws.with_policy(Policy::Dev {
                deny_read,
                extra_write,
                allow_network: config.allow_network,
            })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[test]
    fn unknown_tool_and_bad_args_are_errors_not_panics() {
        let (_dir, ws) = workspace();
        assert!(run(&ws, "nope", json!({})).is_error);
        assert!(run(&ws, "read", json!("text")).is_error);
        let out = run(&ws, "read", json!({"path": 5}));
        assert!(out.is_error && out.text.contains("must be a string"));
        assert!(run(&ws, "read", json!({})).text.contains("missing"));
    }

    #[cfg(unix)]
    #[test]
    fn paths_cannot_escape_workspace() {
        let (dir, ws) = workspace();
        let ws = ws.with_edit_mode(EditMode::Replace);
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("canary"), "secret").unwrap();
        fs::write(dir.path().join("inside.txt"), "hello\n").unwrap();
        symlink(outside.path().join("canary"), dir.path().join("file_link")).unwrap();
        symlink(outside.path(), dir.path().join("dir_link")).unwrap();
        symlink(dir.path().join("nowhere"), dir.path().join("dangling")).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        symlink(
            dir.path().join("inside.txt"),
            dir.path().join("sub/inner_link"),
        )
        .unwrap();

        let canary = outside.path().join("canary");
        let escapes = [
            "../x".to_string(),
            "sub/../../x".to_string(),
            canary.to_string_lossy().into_owned(),
            "file_link".to_string(),
            "dir_link/canary".to_string(),
            "dir_link/new.txt".to_string(),
            "dangling".to_string(),
        ];
        for path in &escapes {
            let read = run(&ws, "read", json!({"path": path}));
            assert!(
                read.is_error && !read.text.contains("secret"),
                "read {path}: {}",
                read.text
            );
            let write = run(&ws, "write", json!({"path": path, "content": "pwned"}));
            assert!(write.is_error, "write {path}");
            let edit = run(
                &ws,
                "edit",
                json!({"path": path, "old_string": "secret", "new_string": "x"}),
            );
            assert!(edit.is_error, "edit {path}");
        }
        assert_eq!(fs::read_to_string(&canary).unwrap(), "secret");
        assert!(!outside.path().join("new.txt").exists());
        assert!(!dir.path().join("nowhere").exists());

        // Symlinks that stay inside the workspace are fine.
        let ok = run(&ws, "read", json!({"path": "sub/inner_link"}));
        assert!(!ok.is_error && ok.text.contains("hello"), "{}", ok.text);
        // Absolute paths inside the workspace are fine too.
        let abs = dir.path().canonicalize().unwrap().join("inside.txt");
        assert!(!run(&ws, "read", json!({"path": abs})).is_error);
    }

    #[test]
    fn home_workspace_sandbox_policies_keep_private_storage_excluded() {
        use crate::config::types::SandboxMode;
        let (dir, ws) = workspace();
        let user = dir.path().canonicalize().unwrap().join(".dial");
        let state = user.join("projects/home");
        fs::create_dir_all(&state).unwrap();
        for mode in [SandboxMode::Dev, SandboxMode::Strict] {
            let ws = apply_sandbox_config(
                ws.clone(),
                &SandboxConfig {
                    mode,
                    ..Default::default()
                },
                Some((&state, &user)),
            )
            .unwrap();
            assert!(ws.resolve(".dial/config.toml").is_err());
            assert!(ws.resolve("public.txt").is_ok());
            let denied = match ws.sandbox_policy() {
                sandbox::Policy::Dev { deny_read, .. }
                | sandbox::Policy::Strict { deny_read, .. } => deny_read,
            };
            assert!(denied.contains(&user));
            assert!(denied.contains(&state));
        }
    }

    #[test]
    fn protected_storage_is_excluded_even_without_sandbox_or_gitignore() {
        let (dir, ws) = workspace();
        let private = dir.path().join("storage/private");
        fs::create_dir_all(&private).unwrap();
        fs::write(private.join("secret.txt"), "private-canary").unwrap();
        fs::write(dir.path().join("public.txt"), "public-canary").unwrap();
        let ws = apply_sandbox_config(
            ws,
            &SandboxConfig {
                mode: crate::config::types::SandboxMode::Off,
                ..Default::default()
            },
            Some((&private, &private)),
        )
        .unwrap();
        for path in [
            "storage/private",
            "storage/private/secret.txt",
            "storage/private/new.txt",
            "storage/private/../escape.txt",
        ] {
            assert!(ws.resolve(path).is_err(), "{path}");
            assert!(run(&ws, "read", json!({"path":path})).is_error);
            assert!(run(&ws, "write", json!({"path":path,"content":"changed"})).is_error);
        }
        assert!(ws.observe_delete(&ws.root().join("storage")).is_err());
        assert!(ws.observe_write(&ws.root().join("storage")).is_err());
        for (tool, args) in [
            ("read", json!({"path":"."})),
            ("glob", json!({"pattern":"**/*","gitignore":false})),
            ("grep", json!({"pattern":"canary","gitignore":false})),
        ] {
            let out = run(&ws, tool, args);
            assert!(!out.is_error, "{tool}: {}", out.text);
            assert!(
                !out.text.contains("secret.txt") && !out.text.contains("private-canary"),
                "{tool}: {}",
                out.text
            );
            assert!(out.text.contains("public.txt"), "{tool}: {}", out.text);
        }
        assert_eq!(
            fs::read_to_string(private.join("secret.txt")).unwrap(),
            "private-canary"
        );
    }

    #[cfg(unix)]
    #[test]
    fn protected_storage_rejects_symlink_aliases_and_missing_tails() {
        let (dir, ws) = workspace();
        let private = dir.path().join("private");
        fs::create_dir(&private).unwrap();
        fs::write(private.join("secret.txt"), "private-canary").unwrap();
        symlink(&private, dir.path().join("alias")).unwrap();
        symlink(private.join("secret.txt"), dir.path().join("file-alias")).unwrap();
        let ws =
            ws.with_protected_paths(&[dir.path().join("alias"), dir.path().join("future/state")]);
        for path in [
            "private/secret.txt",
            "alias/secret.txt",
            "alias/new.txt",
            "file-alias",
            "future/state/new.txt",
        ] {
            assert!(run(&ws, "read", json!({"path":path})).is_error, "{path}");
            assert!(
                run(&ws, "write", json!({"path":path,"content":"changed"})).is_error,
                "{path}"
            );
        }
        assert!(ws.resolve("future/public.txt").is_ok());
        assert_eq!(
            fs::read_to_string(private.join("secret.txt")).unwrap(),
            "private-canary"
        );
    }
}

#[cfg(test)]
mod multi_root_tests {
    use super::testkit::*;
    use serde_json::json;

    #[test]
    fn absolute_paths_resolve_under_added_roots_and_relative_under_primary() {
        let (_dir, mut ws) = workspace();
        let extra = tempfile::tempdir().unwrap();
        std::fs::write(extra.path().join("e.txt"), "e").unwrap();
        assert!(ws.add_root(extra.path()).unwrap());

        let inside = extra.path().join("e.txt");
        assert_eq!(
            ws.resolve(inside.to_str().unwrap()).unwrap(),
            inside.canonicalize().unwrap()
        );
        assert_eq!(ws.resolve("p.txt").unwrap(), ws.root().join("p.txt"));
        assert_eq!(ws.roots().count(), 2);
        assert_eq!(
            ws.additional_roots(),
            [extra.path().canonicalize().unwrap()]
        );
    }

    #[test]
    fn paths_outside_every_root_are_refused() {
        let (_dir, mut ws) = workspace();
        let extra = tempfile::tempdir().unwrap();
        ws.add_root(extra.path()).unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("o.txt"), "o").unwrap();
        let err = ws
            .resolve(other.path().join("o.txt").to_str().unwrap())
            .unwrap_err();
        assert!(err.contains("outside the workspace"), "{err}");
        let escape = format!("{}/../{}", extra.path().display(), "o.txt");
        assert!(ws.resolve(&escape).is_err());
    }

    #[test]
    fn removed_root_stops_resolving_and_primary_cannot_be_added_or_removed() {
        let (_dir, mut ws) = workspace();
        let extra = tempfile::tempdir().unwrap();
        std::fs::write(extra.path().join("e.txt"), "e").unwrap();
        let file = extra.path().join("e.txt");
        assert!(ws.add_root(extra.path()).unwrap());
        assert!(!ws.add_root(extra.path()).unwrap(), "second add is a no-op");
        assert!(ws.remove_root(extra.path()).unwrap());
        assert!(!ws.remove_root(extra.path()).unwrap());
        assert!(ws.resolve(file.to_str().unwrap()).is_err());

        let root = ws.root().to_path_buf();
        assert!(ws.add_root(&root).is_err());
        assert!(ws.remove_root(&root).is_err());
    }

    #[test]
    fn with_additional_roots_rejects_missing_directories() {
        let (_dir, ws) = workspace();
        let missing = ws.root().join("does-not-exist");
        assert!(ws.with_additional_roots(&[missing]).is_err());
    }

    /// `/add-dir` must make file tools work under the added directory, not just path resolution:
    /// the confined edit transaction used to walk only from the primary root.
    #[test]
    fn file_tools_read_and_write_under_an_added_root() {
        let (_dir, mut ws) = workspace();
        let extra = tempfile::tempdir().unwrap();
        std::fs::write(extra.path().join("e.txt"), "existing\n").unwrap();
        ws.add_root(extra.path()).unwrap();
        let root = extra.path().canonicalize().unwrap();

        let read = run(&ws, "read", json!({"path": root.join("e.txt")}));
        assert!(!read.is_error, "{}", read.text);
        assert!(read.text.contains("existing"), "{}", read.text);

        let created = root.join("sub/new.txt");
        let write = run(&ws, "write", json!({"path": created, "content": "made\n"}));
        assert!(!write.is_error, "{}", write.text);
        assert_eq!(std::fs::read_to_string(&created).unwrap(), "made\n");
    }
}
