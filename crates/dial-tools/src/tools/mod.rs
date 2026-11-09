//! Built-in, model-facing tools: read, write, edit, glob, grep and bash.
//!
//! Every file path a model supplies is resolved against the workspace root:
//! `..` escapes, absolute paths outside the root and symlinks that lead out of
//! it are rejected, and directory walks (glob, grep) never follow symlinks.
//! `bash` runs inside the platform sandbox (macOS sandbox-exec or Linux/WSL2 bubblewrap;
//! see [`sandbox`]) and refuses to run at all when no sandbox is available. There is no implicit
//! unsandboxed fallback; only an explicit operator opt-out (`Workspace::without_sandbox`) runs
//! unsandboxed, and that is a separate trust mode rather than something an approval grants.
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
//! * Native protected-path checks do not confine external language servers, MCP servers, remote SSH
//!   tools, or unsandboxed shell commands; those need their own execution/approval boundary.
//! * Timeouts kill the bash process group. A command that calls `setsid` can
//!   outlive the call (still sandboxed, but orphaned).
use crate::config::tools::EditSettings;

use crate::config::tools::BUILTIN;

/// How every edit grammar treats `.ipynb` files. It is shown with notebook reads and failed notebook
/// edits rather than in every `edit` schema.
pub const NOTEBOOK_EDIT_NOTE: &str = "Notebooks: ordinary .ipynb reads expose cell source as # %% [code|markdown|raw] cell:N (zero-based). Keep indexed markers to retain authentic cells; unindexed # %% markers create cells. Preserve source-literal marker lines with one extra %, e.g. # %%%. All edit modes operate on this projection and retain untouched raw JSON metadata, number lexemes and outputs; changed code cells clear stale outputs/execution counts. Metadata-only changes invalidate witnesses. Read :raw for literal JSON; whole-file write requires actual JSON, not the source projection. Notebook moves/deletes need complete relevant source witnesses.";

pub mod code;
pub mod edit;
pub mod exec;
pub mod file_handles;
pub mod fs;
pub mod integrations;
pub mod knowledge;
pub mod net;
pub mod surface;
#[cfg(test)]
pub(crate) mod testkit;
pub mod vision;

// External `dial::tools::*` paths used by src/main.rs, src/cmd and tests/integration.
pub use edit::hashline;
pub use exec::sandbox;
pub use fs::documents;
pub use fs::files::NativeReadSelection;
pub use integrations::ext;
pub use knowledge::{codebase, provenance};
pub mod vcs;

use crate::config::types::SandboxConfig;
use crate::runtime::schema::tool_schema;
use crate::runtime::tool_output::ToolOutput;
use crate::storage::artifact;
use anyhow::{Context, Result, bail, ensure};
use exec::sandbox::Sandbox;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// Selected tools withheld from the default per-request offer. They stay in the catalog, so the
/// system prompt lists them under "Loadable tools" and `tool_load` (or `tool_search`) enables them
/// for the next round; schemas and historical contracts are unchanged. Each is either an
/// overlapping variant of a tool that stays offered (`ast_search` is the structural variant of
/// `grep`/`find`; `git` overlaps `bash`) or a specialist most turns never call (language servers,
/// MCP runtime status,
/// archived-session search, independent-session coordination and side questions). `working_state`,
/// the record that survives compaction, stays offered so a long task does not pay a discovery round
/// to keep it. `codebase_memory`, `experience` and `edit_preview` are not in the default harness
/// selection at all and are likewise loadable on demand. The deferred schemas cost about 10 KB on
/// every request; a first use costs
/// one `tool_load` round, after which the tool stays offered for the session. An operator-pinned
/// `[harness] tools` list, a hermetic run and the selection lent to subagents keep every selected
/// tool offered.
pub const DEFERRED_BY_DEFAULT: &[&str] = &[
    "ast_search",
    "lsp",
    "git",
    "mcp_servers",
    "session_search",
    "peers",
    "side_request",
];

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

pub use surface::outcome::ToolStatus;

use crate::config::types::EditMode;
pub use edit::editing::{EditRepair, EditRepairRequest};
pub use edit::editing::{declared_targets as declared_edit_targets, targets as editing_targets};
pub use edit::editing::{
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
    /// An observer that attributes its writes to `owner` (a worker label), for grouped review.
    /// Observers that do not attribute return `None` and are shared as they are.
    fn scoped(&self, _owner: &str) -> Option<Arc<dyn WriteObserver>> {
        None
    }
}

/// Text file I/O that an ACP client performs for the agent (`fs/read_text_file`, `fs/write_text_file`),
/// so the editor's unsaved buffers are what is read and written. An operation the client did not
/// advertise reports `false` from `reads`/`writes`, and that operation stays local.
pub trait ClientFileIo: Send + Sync {
    fn reads(&self) -> bool;
    fn writes(&self) -> bool;
    fn read_text(&self, path: &Path) -> Result<String, String>;
    fn write_text(&self, path: &Path, content: &str) -> Result<(), String>;
    /// Typed write certainty. Older clients conservatively treat errors as unknown effects.
    fn write_text_confirmed(&self, path: &Path, content: &str) -> Result<(), ClientWriteError> {
        self.write_text(path, content)
            .map_err(ClientWriteError::Unknown)
    }
}

/// A client refusal before delivery differs from a lost acknowledgement after delivery.
#[derive(Debug, thiserror::Error)]
pub enum ClientWriteError {
    #[error("{0}")]
    NotWritten(String),
    #[error("{0}")]
    Unknown(String),
}

/// Installed client bridge; `None` keeps every file operation local.
#[derive(Clone, Default)]
struct ClientFilesSlot(Option<Arc<dyn ClientFileIo>>);

impl std::fmt::Debug for ClientFilesSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "Some(ClientFileIo)"
        } else {
            "None"
        })
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
    /// Small/lean local tier: edits that add parser errors to a high-confidence grammar are refused.
    syntax_block: bool,
    root: PathBuf,
    additional_roots: Vec<PathBuf>,
    /// Explicit process-local native read grants, never shell write grants.
    read_only_roots: Vec<PathBuf>,
    /// Private application trees excluded independently of shell sandbox mode.
    protected_paths: Vec<PathBuf>,
    /// Explicit process-local native-file exceptions; never remove shell denies.
    sensitive_roots: Vec<PathBuf>,
    /// Credentials cannot be included in a sensitive storage grant.
    credential_paths: Vec<PathBuf>,
    sandbox: Option<Sandbox>,
    allow_network: bool,
    /// Domain rules for sandboxed commands' network (`sandbox.network_allow`/`network_deny`);
    /// `None` leaves allowed network unfiltered.
    network_rules: Option<exec::sandbox::netproxy::NetworkRules>,
    /// Why web fetches and URL reads are refused, or `None` when they may reach the network.
    /// A workspace with no session network policy attached refuses them.
    web_network_block: Option<&'static str>,
    policy: Option<exec::sandbox::Policy>,
    unsandboxed: bool,
    /// Shell that runs `bash` commands (`shell.backend`).
    shell_backend: crate::config::types::ShellBackend,
    /// Shrink the output of recognised noisy commands (`shell.minimize_output`).
    minimize_output: bool,
    /// Which commands the git guard refuses (`shell.git_guard`).
    git_guard: crate::config::types::GitGuard,
    /// This workspace is a delegated worker's view of the parent's live checkout, not an isolated
    /// copy. Set by the worker spawner; never inherited by [`Workspace::rebased`].
    shared_checkout: bool,
    edit_mode: EditMode,
    edit_settings: EditSettings,
    /// Bash intent interceptor and the tool names offered this session (a redirect applies only
    /// when its target is offered). `None` leaves bash commands unchecked.
    bash_interceptor: Option<(
        crate::config::tools::bash_interceptor::BashInterceptorConfig,
        Vec<String>,
    )>,
    /// User SSH settings (`Config.ssh`) applied to `ssh://` reads and writes this session.
    ssh: crate::config::tools::ssh::SshConfig,
    /// Connection targets for `ssh://` hosts, keyed by name (see [`Workspace::with_ssh_hosts`]).
    ssh_hosts: Arc<BTreeMap<String, integrations::ssh::Target>>,
    /// Resolver for host-served resource schemes (see [`integrations::host_uri`]); `None` serves none.
    host_uris: Option<Arc<dyn integrations::host_uri::HostUriResolver>>,
    edit_repair: edit::editing::RepairSlot,
    edit_blackbox_dir: Option<PathBuf>,
    native_edits: Arc<edit::edit_native::store::EditStore>,
    observer: ObserverSlot,
    /// ACP client file bridge (see [`ClientFileIo`]); `None` leaves reads and writes local.
    client_files: ClientFilesSlot,
    /// Session snapshots / seen lines / registers for hashline; shared by clones.
    edits: Arc<Mutex<edit::hashline::EditStore>>,
    file_handles: Arc<Mutex<file_handles::Registry>>,
    /// `ast_edit` dry-run previews awaiting an `apply_token`; shared by clones.
    staged_rewrites: Arc<edit::ast::StagedPreviews>,
    artifacts: Option<artifact::ArtifactStore>,
    /// Session name scoping Cargo build output (see [`exec::cargo_target`]); `None` leaves it unset.
    cargo_session: Option<String>,
    /// Shared ownership prevents maintenance while this workspace or its jobs can use the target.
    cargo_lease: Option<Arc<std::fs::File>>,
    operations: Arc<Mutex<()>>,
    shell_sessions: Arc<Mutex<BTreeMap<String, crate::embedded_shell::state::State>>>,
    /// Named sessions a background shell job holds, or settled since their last use.
    shell_busy: Arc<Mutex<BTreeMap<String, exec::shell::session::SessionBusy>>>,
    pub speculation: Arc<exec::speculation::Pool>,
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
            syntax_block: false,
            root,
            additional_roots: Vec::new(),
            read_only_roots: Vec::new(),
            protected_paths: Vec::new(),
            sensitive_roots: Vec::new(),
            credential_paths: Vec::new(),
            sandbox: None,
            allow_network: false,
            network_rules: None,
            web_network_block: Some("no session network policy is attached"),
            policy: None,
            unsandboxed: false,
            shell_backend: crate::config::types::ShellBackend::Auto,
            minimize_output: false,
            git_guard: crate::config::types::GitGuard::Workers,
            shared_checkout: false,
            edit_mode: EditMode::default(),
            edit_settings: EditSettings::default(),
            bash_interceptor: None,
            ssh: crate::config::tools::ssh::SshConfig::default(),
            ssh_hosts: Arc::default(),
            host_uris: None,
            edit_repair: edit::editing::RepairSlot::default(),
            edit_blackbox_dir: None,
            native_edits: Arc::default(),
            observer: ObserverSlot::default(),
            client_files: ClientFilesSlot::default(),
            edits: Arc::default(),
            file_handles: Arc::default(),
            staged_rewrites: Arc::default(),
            artifacts: None,
            cargo_session: None,
            cargo_lease: None,
            operations: Arc::default(),
            shell_sessions: Arc::default(),
            shell_busy: Arc::default(),
            speculation: Arc::default(),
        })
    }

    pub(crate) fn speculative_workspace(&self) -> Self {
        let mut isolated = self.clone();
        isolated.speculation = Arc::default();
        isolated.native_edits = Arc::default();
        isolated.edits = Arc::default();
        isolated.staged_rewrites = Arc::default();
        isolated.observer = ObserverSlot::default();
        isolated.client_files = ClientFilesSlot::default();
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
    /// Scopes Cargo output and leases its directory for this workspace and its clones.
    /// Invalid names leave isolation off; an unavailable lease refuses setup before export.
    pub fn with_cargo_session(mut self, session: &str) -> Result<Self> {
        self.cargo_session = exec::cargo_target::valid_session(session).then(|| session.to_owned());
        self.cargo_lease = self
            .cargo_session
            .as_deref()
            .map(|name| exec::cargo_target::session_lease(&self.root, name))
            .transpose()?;
        Ok(self)
    }
    /// Cargo target directory for commands run in this workspace, or `None` without a session scope.
    pub fn cargo_target_dir(&self) -> Option<PathBuf> {
        self.cargo_session
            .as_deref()
            .map(|session| exec::cargo_target::dir_for(&self.root, session))
    }
    pub fn serial_operation(&self) -> MutexGuard<'_, ()> {
        self.operations.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(crate) fn try_serial_operation(&self) -> Option<MutexGuard<'_, ()>> {
        match self.operations.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => Some(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    /// Wait for workspace ownership without losing cancellation or deadline checks. The caller
    /// retains the guard while effects run; cancellation never detaches an active operation.
    pub fn serial_operation_checked<E>(
        &self,
        mut check: impl FnMut() -> std::result::Result<(), E>,
    ) -> std::result::Result<MutexGuard<'_, ()>, E> {
        loop {
            check()?;
            if let Some(guard) = self.try_serial_operation() {
                check()?;
                return Ok(guard);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
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

    /// Route sandboxed commands' network through the filtering proxy for `rules`.
    pub fn with_network_rules(
        mut self,
        rules: Option<exec::sandbox::netproxy::NetworkRules>,
    ) -> Self {
        self.network_rules = rules;
        self
    }

    pub fn network_rules(&self) -> Option<&exec::sandbox::netproxy::NetworkRules> {
        self.network_rules.as_ref()
    }

    /// The running filtering proxy sandboxed commands must use, started on first need; `None`
    /// when no rules are configured.
    #[cfg(unix)]
    pub(crate) fn network_proxy(
        &self,
    ) -> std::result::Result<Option<Arc<exec::sandbox::netproxy::Endpoint>>, String> {
        self.network_rules
            .as_ref()
            .map(exec::sandbox::netproxy::endpoint)
            .transpose()
    }

    /// Set the web network gate from the session's sandbox policy (`None` allows the network).
    pub(in crate::tools) fn with_web_network_block(mut self, block: Option<&'static str>) -> Self {
        self.web_network_block = block;
        self
    }

    pub(in crate::tools) fn web_network_block(&self) -> Option<&'static str> {
        self.web_network_block
    }

    /// Run bash under an explicit sandbox policy (strict or developer).
    pub fn with_policy(mut self, policy: exec::sandbox::Policy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// The sandbox policy in force at exec time. Attached roots get normal write access,
    /// except roots inside protected storage: those can only receive native-file grants.
    /// Fold roots in here so attachment/removal takes effect on the next command.
    pub fn sandbox_policy(&self) -> exec::sandbox::Policy {
        let roots = self
            .additional_roots
            .iter()
            .filter(|root| {
                !self.protected_paths.iter().any(|protected| {
                    Self::canonical_prefix(root).starts_with(Self::canonical_prefix(protected))
                })
            })
            .cloned();
        match &self.policy {
            Some(exec::sandbox::Policy::Dev {
                deny_read,
                extra_write,
                allow_network,
            }) => exec::sandbox::Policy::Dev {
                deny_read: deny_read
                    .iter()
                    .chain(&self.protected_paths)
                    .cloned()
                    .collect(),
                extra_write: extra_write.iter().cloned().chain(roots).collect(),
                allow_network: *allow_network,
            },
            Some(exec::sandbox::Policy::Strict {
                allow_network,
                deny_read,
                extra_write,
            }) => exec::sandbox::Policy::Strict {
                allow_network: *allow_network,
                deny_read: deny_read
                    .iter()
                    .chain(&self.protected_paths)
                    .cloned()
                    .collect(),
                extra_write: extra_write.iter().cloned().chain(roots).collect(),
            },
            None => exec::sandbox::Policy::Strict {
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

    /// Apply the whole `[shell]` section: backend, output minimizer and git guard.
    pub fn with_shell_config(mut self, shell: &crate::config::types::ShellConfig) -> Self {
        self.shell_backend = shell.backend;
        self.minimize_output = shell.minimize_output;
        self.git_guard = shell.git_guard;
        self
    }

    /// Mark this workspace as a delegated worker running in the parent's live checkout, which is
    /// what `shell.git_guard = "workers"` protects.
    pub fn with_shared_checkout(mut self, shared: bool) -> Self {
        self.shared_checkout = shared;
        self
    }

    pub(crate) fn minimizing_output(&self) -> bool {
        self.minimize_output
    }

    /// Whether `bash` and `bg` refuse git commands that discard uncommitted work here.
    pub(crate) fn git_guard_active(&self) -> bool {
        use crate::config::types::GitGuard;
        match self.git_guard {
            GitGuard::Always => true,
            GitGuard::Workers => self.shared_checkout,
            GitGuard::Off => false,
        }
    }

    pub(crate) fn git_guard_policy(&self) -> crate::config::types::GitGuard {
        self.git_guard
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
        config: crate::config::tools::bash_interceptor::BashInterceptorConfig,
        available_tools: Vec<String>,
    ) -> Self {
        self.bash_interceptor = Some((config, available_tools));
        self
    }
    pub fn bash_interceptor(
        &self,
    ) -> Option<&(
        crate::config::tools::bash_interceptor::BashInterceptorConfig,
        Vec<String>,
    )> {
        self.bash_interceptor.as_ref()
    }
    /// Applies the user's SSH settings (`Config.ssh`) to `ssh://` reads and writes.
    pub fn with_ssh_config(mut self, ssh: crate::config::tools::ssh::SshConfig) -> Self {
        self.ssh = ssh;
        self
    }
    pub fn ssh_config(&self) -> &crate::config::tools::ssh::SshConfig {
        &self.ssh
    }
    /// Applies the `[ssh_hosts]` table, merged with `~/.ssh/config` aliases, to `ssh://` targets.
    pub fn with_ssh_hosts(
        mut self,
        configured: &BTreeMap<String, crate::config::types::SshHostConfig>,
    ) -> Self {
        self.ssh_hosts = Arc::new(match std::env::var_os("HOME").map(PathBuf::from) {
            Some(home) => integrations::ssh::host_targets(&home, configured),
            None => BTreeMap::new(),
        });
        self
    }
    pub fn ssh_hosts(&self) -> &BTreeMap<String, integrations::ssh::Target> {
        &self.ssh_hosts
    }
    /// Installs (or with `None` removes) the resolver for host-served resource schemes.
    pub fn set_host_uris(
        &mut self,
        resolver: Option<Arc<dyn integrations::host_uri::HostUriResolver>>,
    ) {
        self.host_uris = resolver;
    }
    pub fn host_uris(&self) -> Option<&Arc<dyn integrations::host_uri::HostUriResolver>> {
        self.host_uris.as_ref()
    }
    pub fn with_edit_repair(mut self, repair: Arc<dyn EditRepair>) -> Self {
        self.edit_repair = edit::editing::RepairSlot(Some(repair));
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

    /// Route this session's text reads and writes through the ACP client (see [`ClientFileIo`]).
    /// Copies from [`Workspace::rebased`] and speculative workspaces never inherit the bridge: an
    /// isolated copy is a different tree, and the editor's buffers belong to the live project.
    pub fn set_client_files(&mut self, client: Arc<dyn ClientFileIo>) {
        self.client_files = ClientFilesSlot(Some(client));
    }

    pub(crate) fn client_files(&self) -> Option<&dyn ClientFileIo> {
        self.client_files.0.as_deref()
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
    /// Refuse (instead of only reporting) edits that add parser errors; the controller sets it on the
    /// small and lean local tiers. It needs the syntax guard, and `allow_syntax_errors` overrides it.
    pub fn with_syntax_block(mut self, enabled: bool) -> Self {
        self.syntax_block = enabled;
        self
    }
    pub fn syntax_blocking(&self) -> bool {
        self.syntax_guard && self.syntax_block
    }
    pub(crate) fn syntax_advisory(
        &self,
        path: &Path,
        before: &str,
        proposed: &str,
    ) -> Option<String> {
        if self.syntax_guard {
            edit::syntax::advisory(path, before, proposed)
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
        copy.network_rules = self.network_rules.clone();
        copy.web_network_block = self.web_network_block;
        copy.host_uris = self.host_uris.clone();
        copy.policy = self.policy.clone();
        copy.protected_paths = self.protected_paths.clone();
        copy.credential_paths = self.credential_paths.clone();
        copy.unsandboxed = self.unsandboxed;
        copy.bash_interceptor = self.bash_interceptor.clone();
        copy.shell_backend = self.shell_backend;
        copy.minimize_output = self.minimize_output;
        copy.git_guard = self.git_guard;
        copy.edit_mode = self.edit_mode;
        copy.edit_settings = self.edit_settings.clone();
        copy.edit_repair = self.edit_repair.clone();
        copy.edit_blackbox_dir = self.edit_blackbox_dir.clone();
        copy.syntax_guard = self.syntax_guard;
        copy.syntax_block = self.syntax_block;
        copy.ssh = self.ssh.clone();
        copy.ssh_hosts = Arc::clone(&self.ssh_hosts);
        copy.artifacts = self.artifacts.clone();
        if let Some(session) = &self.cargo_session {
            copy = copy.with_cargo_session(session)?;
        }
        Ok(copy)
    }

    /// The same session configuration (sandbox, edit mode, SSH, artifacts) over another primary
    /// root, for `/move` in a live session. Attached roots belonged to the old project and are dropped.
    pub fn reroot(&self, root: &Path) -> Result<Workspace> {
        let fresh = Workspace::new(root)?;
        let mut next = self.clone();
        next.root = fresh.root;
        next.file_handles = Arc::new(Mutex::new(
            self.file_handles
                .lock()
                .expect("file handle registry poisoned")
                .retired(),
        ));
        next.additional_roots.clear();
        next.read_only_roots.clear();
        next.sensitive_roots.clear();
        if let Some(session) = &self.cargo_session {
            next = next.with_cargo_session(session)?;
        }
        Ok(next)
    }

    pub(crate) fn staged_rewrites(&self) -> &edit::ast::StagedPreviews {
        &self.staged_rewrites
    }
    pub(crate) fn store(&self) -> MutexGuard<'_, edit::hashline::EditStore> {
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
        self.record_witnesses(path, source, complete_ranges, false)
    }
    /// [`Self::record_edit_witnesses`] for a broad search page: new files enter at the old end of
    /// the witness store's eviction order (see `Store::record_search`).
    pub fn record_search_witnesses(
        &self,
        path: &Path,
        source: &str,
        complete_ranges: &[(usize, usize)],
    ) -> Result<(), String> {
        self.record_witnesses(path, source, complete_ranges, true)
    }
    fn record_witnesses(
        &self,
        path: &Path,
        source: &str,
        complete_ranges: &[(usize, usize)],
        search: bool,
    ) -> Result<(), String> {
        if complete_ranges.is_empty() {
            return Ok(());
        }
        if source.len() > edit::hashline::MAX_FILE as usize {
            return Err("edit witness source exceeds 4 MiB".into());
        }
        let resolved = self.resolve_read(&self.rel(path))?;
        if resolved != path {
            return Err("edit witness target is not a canonical workspace path".into());
        }
        let lf = edit::hashline::normalize(source);
        let count = edit::hashline::split_lines(&lf).len();
        if complete_ranges
            .iter()
            .any(|&(a, b)| a == 0 || b < a || b > count)
        {
            return Err("edit witness ranges are out of bounds".into());
        }
        if lf.len() > edit::hashline::MAX_FILE as usize {
            return Err("edit witness source exceeds 4 MiB".into());
        }
        let current = edit::edit_transaction::read(self, path)?
            .ok_or("edit witness target no longer exists")?;
        if current.text != source {
            return Err("edit witness source changed before publication; not recorded".into());
        }
        if search {
            self.store().record_search(path, &lf, complete_ranges);
        } else {
            self.store().record(path, &lf, complete_ranges);
        }
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
        edit::edit_source::record_cell(self, path, raw, index, coverage)
    }

    pub fn observe_write(&self, abs: &Path) -> Result<(), String> {
        self.check_protected_mutation(abs)?;
        code::navigation::invalidate(abs);
        match &self.observer.0 {
            Some(observer) => observer
                .before_write(abs)
                .map_err(|e| format!("write to {:?} blocked by the harness: {e:#}", self.rel(abs))),
            None => Ok(()),
        }
    }

    pub fn observe_delete(&self, abs: &Path) -> Result<(), String> {
        self.check_protected_mutation(abs)?;
        code::navigation::invalidate(abs);
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

    /// Advisory changes to previously observed native text in the primary workspace.
    /// Does not refresh snapshots, grant edit witnesses or provide overwrite protection.
    /// Round-robin polling considers at most 64 files, reads at most 4 MiB, and returns
    /// at most 16 paths. Files over 256 KiB and notebooks are skipped. The 50 ms budget
    /// is cooperative between filesystem calls (slow filesystems can exceed it).
    /// Missing/unreadable versions are deduplicated by error kind until recovery or reread.
    pub fn changed_observations(&self) -> Vec<String> {
        edit::hashline::changed_observations(self)
    }

    /// Every root this workspace may touch: the primary root first, then the
    /// additional roots attached with `with_additional_roots`.
    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.root.as_path())
            .chain(self.additional_roots.iter().map(PathBuf::as_path))
    }

    fn read_roots(&self) -> impl Iterator<Item = &Path> {
        self.roots()
            .chain(self.read_only_roots.iter().map(PathBuf::as_path))
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
    /// Validate a requested existing directory without granting it. Sensitive requests must
    /// name a directory inside protected storage, never an ancestor (for example home).
    pub fn directory_request(&self, dir: &Path, sensitive: bool) -> Result<PathBuf> {
        let absolute = if dir.is_absolute() {
            dir.to_owned()
        } else {
            self.root.join(dir)
        };
        let canonical = Self::canonical_dir(&absolute)?;
        ensure!(
            canonical.parent().is_some(),
            "the filesystem root cannot be added"
        );
        if sensitive {
            ensure!(
                self.protected_paths
                    .iter()
                    .any(|p| canonical.starts_with(Self::canonical_prefix(p))),
                "sensitive access must name a directory inside protected Dial storage, not its ancestor"
            );
            ensure!(
                !self.credential_paths.iter().any(|p| {
                    let p = Self::canonical_prefix(p);
                    canonical.starts_with(&p) || p.starts_with(&canonical)
                }),
                "sensitive directory includes credentials; request a narrower non-credential directory"
            );
        }
        Ok(canonical)
    }

    /// Apply an explicit human-approved native-file exception. Callers must display the
    /// canonical path and obtain a separate decision; ordinary root attachment never calls this.
    pub fn grant_sensitive_directory(&mut self, canonical: &Path) -> Result<()> {
        let checked = self.directory_request(canonical, true)?;
        ensure!(checked == canonical, "directory changed since approval");
        if checked != self.root && !self.additional_roots.contains(&checked) {
            self.additional_roots.push(checked.clone());
        }
        if !self.sensitive_roots.contains(&checked) {
            self.sensitive_roots.push(checked);
        }
        Ok(())
    }

    fn sensitive_access(&self, path: &Path) -> bool {
        // Only an explicitly granted sensitive root can apply; skip canonicalizing for the common case.
        if self.sensitive_roots.is_empty() {
            return false;
        }
        let canonical = Self::canonical_prefix(path);
        !self
            .credential_paths
            .iter()
            .any(|p| canonical.starts_with(Self::canonical_prefix(p)))
            && self
                .sensitive_roots
                .iter()
                .any(|root| canonical.starts_with(root))
    }

    pub(crate) fn is_protected(&self, path: &Path) -> bool {
        if self.protected_paths.is_empty() {
            return false;
        }
        let canonical = Self::canonical_prefix(path);
        if self.sensitive_access(&canonical) {
            return false;
        }
        self.protected_paths.iter().any(|protected| {
            path.starts_with(protected)
                || canonical.starts_with(protected)
                || canonical.starts_with(Self::canonical_prefix(protected))
        })
    }

    fn check_protected_mutation(&self, path: &Path) -> Result<(), String> {
        let canonical = Self::canonical_prefix(path);
        if !self.roots().any(|root| canonical.starts_with(root)) {
            return Err("mutation is outside writable workspace roots (read-only access does not grant writes)".into());
        }
        // Git configuration and hooks are executed later by host-side Git. Native mutations
        // respect the same metadata boundary as sandboxed shell commands, including linked
        // worktrees, aliases, and deleting an ancestor of the metadata directory.
        let roots: Vec<_> = self.roots().map(Path::to_path_buf).collect();
        let canonical = Self::canonical_prefix(path);
        if [path, canonical.as_path()].iter().any(|path| {
            path.components()
                .any(|component| component.as_os_str() == ".git")
        }) {
            return Err("mutation would affect protected Git metadata; use an approved git operation instead".into());
        }
        if sandbox::git_metadata(&roots)?.iter().any(|metadata| {
            path.starts_with(metadata)
                || metadata.starts_with(path)
                || canonical.starts_with(metadata)
                || metadata.starts_with(&canonical)
        }) {
            return Err("mutation would affect protected Git metadata; use an approved git operation instead".into());
        }
        if self.protected_paths.is_empty() {
            return Ok(());
        }
        if self.sensitive_access(path) {
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
        self.resolve_checked(input)
            .map_err(|error| error.to_string())
    }

    fn resolve_checked(&self, input: &str) -> io::Result<PathBuf> {
        let expanded = self.expand_file_handle_checked(input)?;
        if file_handles::is_handle_with_selector(input) {
            return Err(io::Error::other(
                "read selectors are not accepted by path resolution",
            ));
        }
        self.resolve_literal(&expanded, false)
    }

    fn resolve_literal(&self, input: &str, read_only: bool) -> io::Result<PathBuf> {
        if input.contains('\0') {
            return Err(io::Error::other("path contains a NUL byte"));
        }
        let path = Path::new(input);
        // An explicit sensitive root may be nested beneath the primary workspace.
        // Resolve it from that most-specific root, not through a protected ancestor.
        if !path.is_absolute() && !self.sensitive_roots.is_empty() {
            let absolute = Self::canonical_prefix(&self.root.join(path));
            if self.sensitive_access(&absolute) {
                return self.resolve_literal(&absolute.to_string_lossy(), read_only);
            }
        }
        let (root, relative): (&Path, PathBuf) = if path.is_absolute() {
            let within = |candidate: &Path| {
                self.roots()
                    .chain(
                        self.read_only_roots
                            .iter()
                            .map(PathBuf::as_path)
                            .filter(|_| read_only),
                    )
                    .filter_map(|root| {
                        candidate
                            .strip_prefix(root)
                            .ok()
                            .map(|rest| (root, rest.to_path_buf()))
                    })
                    .max_by_key(|(root, _)| root.components().count())
            };
            // Roots are canonical (`/private/var` on macOS); input may name the same place through a symlink.
            within(path)
                .or_else(|| within(&Self::canonical_prefix(path)))
                .ok_or_else(|| {
                    if self.is_protected(&Self::canonical_prefix(path)) {
                        return io::Error::new(io::ErrorKind::PermissionDenied, Self::protected_refusal(input));
                    }
                    // Name the directory to add, so the model can ask for it instead of giving up.
                    let project = path
                        .ancestors()
                        .skip(1)
                        .find(|dir| dir.join(".git").exists())
                        .or_else(|| path.parent())
                        .unwrap_or(path);
                    io::Error::new(io::ErrorKind::PermissionDenied, format!(
                        "path {input:?} is outside the workspace; call request_directory with path {:?} to ask for a yes/no access approval, then retry the tool. Normal directory access does not unlock protected Dial storage; that requires a separate sensitive request.",
                        project.display()
                    ))
                })?
        } else {
            (self.root.as_path(), path.to_path_buf())
        };
        let mut current = root.to_path_buf();
        if self.is_protected(&current) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                Self::protected_refusal(input),
            ));
        }
        let mut missing = false;
        for component in relative.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if missing || current.as_path() == root {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!("path {input:?} escapes the workspace"),
                        ));
                    }
                    current.pop();
                }
                Component::Normal(name) => {
                    current.push(name);
                    if self.is_protected(&current) {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            Self::protected_refusal(input),
                        ));
                    }
                    if missing {
                        continue;
                    }
                    match std::fs::symlink_metadata(&current) {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            current = std::fs::canonicalize(&current).map_err(|error| {
                                io::Error::new(
                                    error.kind(),
                                    format!("path {input:?} goes through a dangling symlink"),
                                )
                            })?;
                            if !current.starts_with(root) {
                                return Err(io::Error::new(
                                    io::ErrorKind::PermissionDenied,
                                    format!(
                                        "path {input:?} follows a symlink out of the workspace"
                                    ),
                                ));
                            }
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == io::ErrorKind::NotFound => missing = true,
                        Err(e) => {
                            return Err(io::Error::new(
                                e.kind(),
                                format!("cannot access {input:?}: {e}"),
                            ));
                        }
                    }
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(io::Error::other(format!(
                        "path {input:?} is not a valid workspace path"
                    )));
                }
            }
            if self.is_protected(&current) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    Self::protected_refusal(input),
                ));
            }
        }
        Ok(current)
    }

    /// The refusal for a path in protected Dial storage, judged by the requested `input` (the
    /// walk may stop at a protected ancestor). Session transcripts are the usual target, and
    /// `session_search` reads them under Dial's own authority.
    fn protected_refusal(input: &str) -> String {
        let path = Path::new(input);
        let transcript = path.extension().is_some_and(|ext| ext == "jsonl")
            && path.components().any(|part| part.as_os_str() == "sessions");
        if transcript {
            format!(
                "path {input:?} is a Dial session transcript in protected Dial storage; search past sessions with the session_search tool instead"
            )
        } else {
            format!("path {input:?} enters protected Dial storage")
        }
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

    /// Grant native read/search access to existing directories for this process only.
    /// Relative directories resolve against the primary workspace. Private storage and symlink
    /// checks still apply. This never changes shell access or writable roots.
    ///
    /// # Errors
    /// Refuses missing directories, filesystem roots and protected Dial storage.
    pub fn with_read_only_roots(mut self, dirs: &[PathBuf]) -> Result<Self> {
        self.read_only_roots.clear();
        for dir in dirs {
            let canonical = self.directory_request(dir, false)?;
            ensure!(
                !self.is_protected(&canonical),
                "read-only directory enters protected Dial storage"
            );
            if !self.read_only_roots.contains(&canonical) {
                self.read_only_roots.push(canonical);
            }
        }
        Ok(self)
    }

    /// Resolve a native read/search path without granting mutation authority.
    pub fn resolve_read(&self, input: &str) -> Result<PathBuf, String> {
        let expanded = self
            .expand_file_handle_checked(input)
            .map_err(|e| e.to_string())?;
        if file_handles::is_handle_with_selector(input) {
            return Err("read selectors are not accepted by path resolution".into());
        }
        self.resolve_literal(&expanded, true)
            .map_err(|e| e.to_string())
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
        self.sensitive_roots.retain(|root| *root != target);
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

    /// Resolve a native read path, including selectors, even when its target is missing.
    /// Missing targets remain confined paths; this does not grant read access or a witness.
    pub fn resolve_read_target(&self, input: &str) -> Result<PathBuf, String> {
        let (path, _) = fs::select::split_target(self, input)?;
        self.resolve_read(path)
    }

    /// Bounded, workspace-confined identity for a read observation, including selectors.
    /// Directories hash the same visible listing as `read`, not metadata or hidden descendants.
    /// Used only to expire recovery feedback; this is not access, a witness or verification evidence.
    pub fn read_identity(&self, input: &str) -> Option<[u8; 32]> {
        use sha2::{Digest, Sha256};
        let path = self.resolve_read_target(input).ok()?;
        if path.is_dir() {
            return fs::files::list_dir(self, &path)
                .ok()
                .map(|listing| Sha256::digest(listing.as_bytes()).into());
        }
        self.file_identity(&path).map(|(_, digest)| digest)
    }

    /// Canonical workspace path and SHA-256 of an exact file path (never a read selector, since
    /// file names may contain `:`). `None` for paths outside every root, protected storage,
    /// non-UTF-8 names, missing or unreadable targets, directories and files over 16 MiB.
    /// Recovery bookkeeping only; this is not verification evidence.
    pub fn file_identity(&self, input: &Path) -> Option<(PathBuf, [u8; 32])> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        const LIMIT: u64 = 16 * 1024 * 1024;
        let path = self.resolve_read(input.to_str()?).ok()?;
        let file = edit::edit_transaction::open_read(self, &path).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.len() > LIMIT {
            return None;
        }
        let mut data = Vec::new();
        file.take(LIMIT + 1).read_to_end(&mut data).ok()?;
        (data.len() as u64 <= LIMIT).then(|| (path, Sha256::digest(&data).into()))
    }

    pub fn rel(&self, path: &Path) -> String {
        match path.strip_prefix(&self.root) {
            Ok(rest) if rest.as_os_str().is_empty() => ".".to_string(),
            Ok(rest) => rest.to_string_lossy().into_owned(),
            Err(_) => path.to_string_lossy().into_owned(),
        }
    }

    /// The workspace root that contains `path` (already resolved by [`Self::resolve`]), preferring
    /// the deepest one when an added root sits inside another. `None` when no root contains it.
    pub(crate) fn root_containing(&self, path: &Path) -> Option<&Path> {
        self.read_roots()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
    }
}

const HASHLINE_READ_DESC: &str = "read: view a file or directory in any format (text, CSV, PDF, Office, \
images, video, notebooks, archives). Ranges as a path suffix: `:50-100`, `:50+20`, `:50-`, `:-30`, \
`:5-16,960-973`; `:outline` lists declarations, `:raw` is non-editable text; `paths` reads up to 16 targets at once. \
Also `ssh://host/path`, `http(s)://`, `pr://N`, `issue://N`, `conflict://` and `artifact://H` (lines, `:info`, \
`:annotations`, `:bytes=O+N`). Output is \
a `[PATH#TAG]` header then `LINE:TEXT` rows; edit needs that TAG and may touch only shown lines. Stops at \
48 KiB; the footer names the next range. Large or unfamiliar files: `grep` or `:outline` first, then read only the needed ranges, never page through. Excerpts already shown by glob/grep previews or earlier reads stay current: reuse them instead of rereading.";

const HASHLINE_WRITE_DESC: &str = "write: create a new file or replace one whose content changes almost entirely. Max 1 MiB; parent directories are created. Replacing an existing file requires reading all of it first; for changes to an existing file use edit (one call carries every operation) instead of regenerating it. Content must be complete, never a placeholder; do not also paste it into chat. Notebooks take literal JSON. Copied read prefixes are stripped; truncated projections are refused. Returns the fresh `[PATH#TAG]`.";

const HASHLINE_EDIT_DESC: &str = r#"edit: change existing files with a line-anchored patch in `input`; whole files use write.
`input` holds sections: a header `[PATH#TAG]` (TAG from that file's latest read, edit or write), then operations. Line numbers refer to THAT snapshot, never to earlier operations in the call; touch only lines a read has shown.
PUT N.=M:  replace lines N..M with the `+` rows that follow (N.=N for one line)
PUT <N:    insert before line N (PUT <1: = top of file)
PUT >N:    insert after line N (PUT >$: = end of file)
CUT N.=M   delete lines N..M
PUT N*:    replace the block starting at line N (function, class, { } or [ ] body, indented body, markdown section)
PUT >N*:   insert after that block;  CUT N* deletes it
REM        delete the file;  MV new/path  move/rename it (after any edits)
Rows: `+` then the exact text with indentation; a lone `+` is a blank line; text starting with + or - is `++x`/`+-x`. Rows are final text; never write `-` rows.
Move code: `CUT 3.=5 @tmp` then `PUT >9 @tmp` (across files too; `PUT N.=M @tmp` replaces; without @name one anonymous register).
Example: `[src/main.rs#3FA9]` then `PUT 4.=4:` and `+    let total = a + b;` on separate lines.
One section per file; put every planned change to every file in one call. Success returns the fresh [PATH#TAG] and changed lines. If the file changed since your read, edits re-anchor when their lines are unchanged; otherwise you get the current lines: after a stale-anchor failure, re-read the target range (or use those lines and their new TAG) before retrying."#;

const REPLACE_READ_DESC: &str = "read: view a file or directory in any format (text, CSV, PDF, Office, \
images, video, notebooks, archives). Ranges as a path suffix: `:50-100`, `:50+150`, `:50-`, `:-60`, \
`:5-16,960-973`; `:outline` lists declarations; `paths` reads up to 16 targets. Also `artifact://H` (lines, \
`:info`, `:annotations`, `:bytes=O+N`). Lines are numbered. Stops at \
48 KiB; the footer names the next range. Large or unfamiliar files: `grep` or `:outline` first, then read only the needed ranges, never page through.";

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
                    "path": {"type": "string", "description": "Workspace path with an optional selector. Also: `ssh://host/path` and `http(s)://` URLs (approval required; archives take `:member`), `pr://N[/diff]` and `issue://N` (GitHub via gh), `conflict://` (unmerged paths) or `conflict://PATH` (merge hunks with edit anchors), `artifact://H` retained output (line ranges, `:info` retention, `:annotations` human feedback, `:bytes=O+N` exact base64 bytes)."},
                    "offset": {"type": "integer", "minimum": 1, "description": "1-based line to start at (default 1); not together with a path selector."},
                    "limit": {"type": "integer", "minimum": 1, "description": "Maximum number of lines (default 2000)."},
                    "paths": {"type": "array", "items": {"type": "string"}, "maxItems": 16, "description": "Several targets, each with its own selector, read in one call; use instead of path."},
                    "format": {"type":"string","enum":["text","base64"],"description":"Lossless local data, at most 1 MiB, in data.content; text output is a receipt. No selectors."},
                    "retain": {"type":"boolean","description":"With format: preserve exact source bytes as an artifact before recovery operations."},
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
                "write: create a new file or replace one whose content changes almost entirely. Max 1 MiB; \
                 parent directories are created. For changes to an existing file use edit instead of \
                 regenerating it. Content must be complete, never a placeholder; do not also \
                 paste it into chat."
            },
            json!({
                "path": {"type": "string", "description": "Workspace-relative file path or an `ssh://host/path` remote file (requires approval)."},
                "content": {"type": "string", "description": "Full file content."},
                "encoding": {"type":"string","enum":["base64"],"description":"Create a new binary file from base64, at most 1 MiB; parent must exist; never overwrite."},
                "allow_syntax_errors": {"type": "boolean", "description": "Set true only when valid code is refused for new syntax errors (parser dialect gap)."},
                "verify": edit::editing::verify_property(),
            }),
            &["path", "content"],
        ),
        "edit" => edit::editing::schema(mode),
        "glob" => tool_schema(
            "glob",
            "glob: list paths matching a pattern such as `src/**/*.rs`. A bare pattern matches names at any depth; sorted, at most 500 \
             per page, the footer gives next_cursor. .git, scratch and symlinks are never listed; preview \
             adds editable excerpts of a few code files.",
            json!({
                "pattern": {"type": "string", "description": "Glob such as `src/**/*.rs` or `*.py`, relative to the workspace root."},
                "path": {"type": "string", "description": "Scope: file, directory or semicolon-separated scopes (default workspace root). Scopes inside .git or the workspace scratch directory are refused."},
                "hidden": {"type": "boolean", "description": "Include hidden entries (default true)."},
                "gitignore": {"type": "boolean", "description": "Honor ignore rules and skip dependency/build directories (default true)."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 500, "description": "Paths per page (default 500)."},
                "preview": {"type": "boolean", "description": "Excerpts of up to 3 code files among the first 16 paths (default false). Only returned rows are edit witnesses."},
                "skip": {"type": "integer", "minimum": 0, "description": "Paths to skip; resume with remaining_skip."},
                "cursor": {"type": "object", "properties": {"path": {"type": "string"}, "match": {"type": "integer", "minimum": 0, "maximum": 0}}, "required": ["path", "match"], "additionalProperties": false, "description": "next_cursor from the previous page, unchanged."},
            }),
            &["pattern"],
        ),
        "grep" => tool_schema(
            "grep",
            "grep: search file contents with a Rust regex (no lookaround or backreferences). `path` \
             takes files, directories or `;`-separated scopes. Returns path:line:text; the footer names skipped or capped scopes and next_cursor.",
            json!({
                "pattern": {"type": "string", "description": "Non-empty Rust regex such as `fn \\w+\\(` or `TODO|FIXME`; set literal for literal text."},
                "literal": {"type": "boolean", "description": "Treat pattern as literal text (default false)."},
                "path": {"type": "string", "description": "File, directory or semicolon-separated scopes, one `artifact://<hash>` retained output, or one `ssh://host/path` file (approval required)."},
                "glob": {"type": "string", "description": "Only files whose workspace-relative path matches this glob."},
                "ignore_case": {"type": "boolean", "description": "Case-insensitive (default false)."},
                "context": {"type": "integer", "minimum": 0, "description": "Lines before and after each match (default 0, max 10)."},
                "preview": {"type": "boolean", "description": "Append bounded enclosing-source previews for the first matching code files (default false). Complete fresh numbered rows are editable; omitted ranges still need a read."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Most matching lines (default 400; otherwise at most 20 per file)."},
                "output": {"type": "string", "enum": ["lines", "files", "count"], "description": "lines (default); files lists matching files with counts; count gives totals."},
                "multiline": {"type": "boolean", "description": "Match across lines (automatic when the pattern has \\n); use (?s) for dot to match newline."},
                "hidden": {"type": "boolean", "description": "Include hidden entries (default true)."},
                "gitignore": {"type": "boolean", "description": "Honor ignore rules and skip dependency/build directories (default true)."},
                "skip": {"type": "integer", "minimum": 0, "description": "Matches to skip; resume with remaining_skip."},
                "cursor": {"type": "object", "properties": {"path": {"type": "string"}, "match": {"type": "integer", "minimum": 0}}, "required": ["path", "match"], "additionalProperties": false, "description": "next_cursor from the previous page, unchanged."},
            }),
            &["pattern"],
        ),
        "bash" => tool_schema(
            "bash",
            if cfg!(windows) {
                "bash: run a shell command. Set `cwd` instead of prefixing `cd`; `background` starts a job now, `pty` a terminal job. Uses Git for Windows bash when installed, otherwise Dial's built-in bash-compatible shell (PowerShell only with `shell.backend = \"powershell\"`). Windows has no isolation: execution requires `sandbox.mode = \"off\"` and runs with the user's privileges. Output merges stdout/stderr and keeps its start and end. Foreground commands return their result when they finish or reach `timeout`; use `background` only for servers, watchers or long jobs you will work beside. Save a script you will rerun to a file once instead of resending it inline."
            } else {
                "bash: run a shell command with `/bin/bash -c`. Set `cwd` instead of prefixing `cd`; `background` starts a job now, `pty` a terminal job. May be sandboxed: writes confined to the project, network possibly blocked, temp files in `$TMPDIR`. Output merges stdout/stderr and keeps its start and end. Foreground commands return their result when they finish or reach `timeout`; use `background` only for servers, watchers or long jobs you will work beside. Save a script you will rerun to a file once instead of resending it inline."
            },
            json!({
                "command": {"type": "string", "description": "Shell command to run."},
                "check": {"type":"boolean", "description":"This foreground command is an acceptance check: record its actual exit status as verification. Preserve failures (no || echo or trailing successful command); use assertions or cmp -s for generated artifacts. Omit for ordinary commands."},
                "check_paths": {"type":"array","items":{"type":"string"},"maxItems":32,"description":"Files this check exercises. A passing foreground check releases rewrite pressure only for these paths."},
                "cwd": {"type": "string", "description": "Working directory: relative to the workspace root, or absolute in an added root. Defaults to the root; do not prefix `cd <root> &&`."},
                "session": {"type":"string", "description":"Embedded backend only: keep cwd and exported variables across commands with the same session name."},
                "pty": {"type": "boolean", "description": "Start as a terminal `bg` job (default false). Incompatible with session and timeout."},
                "timeout": {"type": "integer", "minimum": 1, "description": "Seconds to wait before the command is killed (or moved to `bg` when `shell.auto_background_secs` is enabled and shorter)."},
                "background": {"type": "boolean", "description": "Start as a `bg` job immediately (servers, watchers)."},
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
    exec::execution::run(&std::sync::atomic::AtomicBool::new(false), || {
        dispatch(ws, name, args, timeout_secs).unwrap_or_else(ToolOutput::err)
    })
}

/// Read through the same confined parser pipeline while allowing managed
/// converter/cache misses to observe the execution owner's cancellation. `budget` is the
/// model-facing byte budget of this result (see [`model_output_limit`]): pages stop there and
/// only the rows they show are witnessed for editing.
pub fn read_tool_streaming(
    ws: &Workspace,
    args: &Value,
    cancel: &std::sync::atomic::AtomicBool,
    budget: usize,
) -> ToolOutput {
    read_streaming(ws, args, cancel, budget, None)
}

/// Preserve bounded, authorized task inputs without declaring workspace edits.
pub fn preserve_task_inputs(
    ws: &Workspace,
    checkpoints: &crate::storage::checkpoint::Checkpoints,
    turn: u64,
) -> Value {
    fs::data_io::preserve_inputs(ws, checkpoints, turn)
}

/// Whether a legacy media target requires explicit external-process approval.
pub fn media_read_needs_approval(path: &str) -> bool {
    fs::media::needs_approval(path)
}

/// Controller-owned read entry point; external media processes require explicit authorization.
pub fn read_with_context(
    context: &integrations::ext::ToolContext<'_>,
    args: &Value,
    cancel: &std::sync::atomic::AtomicBool,
    budget: usize,
) -> ToolOutput {
    read_streaming(context.workspace, args, cancel, budget, Some(context))
}

fn read_streaming(
    ws: &Workspace,
    args: &Value,
    cancel: &std::sync::atomic::AtomicBool,
    budget: usize,
    context: Option<&integrations::ext::ToolContext<'_>>,
) -> ToolOutput {
    if !args.is_object() {
        return ToolOutput::err("tool arguments must be a JSON object");
    }
    exec::execution::run(cancel, || {
        if args.get("format").is_some() {
            return fs::data_io::read(ws, args).unwrap_or_else(ToolOutput::err);
        }
        if args.get("retain").is_some() {
            return ToolOutput::err("retain requires a data read format");
        }
        if let Some(result) = fs::media::read(ws, args, cancel, context) {
            return result.unwrap_or_else(ToolOutput::err);
        }
        match fs::files::read_tool_streaming(ws, args, cancel, budget) {
            Ok(text) => ToolOutput::ok(text),
            Err(error) => ToolOutput::err(error),
        }
    })
}

/// Preview uses the same complete payload preparation but never repairs or writes.
pub fn preview_edit(ws: &Workspace, args: &Value) -> ToolOutput {
    if let Err(error) = edit::editing::validate_shape(ws.edit_mode, args) {
        return ToolOutput::err(error);
    }
    let mut preview = ws.clone();
    preview.native_edits = Arc::new(edit::edit_native::store::EditStore::default());
    preview.edits = Arc::new(Mutex::new(ws.store().clone()));
    let no_change = std::cell::Cell::new(false);
    let execution = edit::editing::Execution {
        preview_no_change: Some(&no_change),
        ..Default::default()
    };
    let result = if ws.edit_mode == EditMode::Hashline {
        edit::hashline::input_arg(args)
            .and_then(|input| edit::hashline::edit_with(&preview, input, false, &execution))
    } else {
        edit::editing::prepare_with_preview(&preview, args, Some(&no_change))
            .and_then(|prepared| {
                edit::editing::finish_prepared(&preview, prepared, false, &execution)
            })
            .map(|output| output.text)
            .map_err(|error| error.to_string())
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
    if let Err(error) = edit::editing::validate_shape(ws.edit_mode, args) {
        return ToolOutput::err(error);
    }
    let mut preview = ws.clone();
    preview.native_edits = Arc::new(edit::edit_native::store::EditStore::default());
    preview.edits = Arc::new(Mutex::new(ws.store().clone()));
    let no_change = std::cell::Cell::new(false);
    let execution = edit::editing::Execution {
        preview_no_change: Some(&no_change),
        ..Default::default()
    };
    let result = if ws.edit_mode == EditMode::Hashline {
        edit::hashline::input_arg(args)
            .and_then(|input| edit::hashline::edit_with(&preview, input, false, &execution))
    } else {
        edit::editing::prepare_streaming_preview(&preview, args, Some(&no_change))
            .and_then(|prepared| {
                edit::editing::finish_prepared(&preview, prepared, false, &execution)
            })
            .map(|output| output.text)
            .map_err(|error| error.to_string())
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
/// Preserve the resolver's permission classification before any mutation is attempted. Other
/// binding errors remain ordinary call errors, never guessed from diagnostic text or path shape.
fn path_refusal(error: io::Error) -> ToolOutput {
    let blocked = error.kind() == io::ErrorKind::PermissionDenied;
    let output = ToolOutput::err(error.to_string());
    if blocked {
        output
            .with_status(ToolStatus::Blocked)
            .with_effects(surface::outcome::Effects::None)
    } else {
        output
    }
}
fn bind_edit_targets(
    ws: &Workspace,
    paths: Vec<String>,
) -> io::Result<Vec<(String, PathBuf, PathBuf)>> {
    paths
        .into_iter()
        .map(|literal| {
            let canonical = ws.resolve_checked(&literal)?;
            let path = Path::new(&literal);
            // The lexical spelling is authorized alongside the canonical target, so it must name a
            // workspace root as spelled. A path that reached a root only through another spelling
            // (a symlinked parent, or `C:\` against a verbatim `\\?\C:\` root on Windows) has no such
            // form; its canonical-prefixed path is the one resolution accepted.
            let lexical = if !path.is_absolute() {
                ws.root().join(path)
            } else if ws.roots().any(|root| path.starts_with(root)) {
                path.to_path_buf()
            } else {
                Workspace::canonical_prefix(path)
            };
            Ok((literal, canonical, lexical))
        })
        .collect()
}
fn authorize_edit_targets(
    ctx: &integrations::ext::ToolContext<'_>,
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
    ctx: &integrations::ext::ToolContext<'_>,
    args: &Value,
    repair: Option<edit::syntax::RepairCallback<'_>>,
    cancel: &std::sync::atomic::AtomicBool,
) -> ToolOutput {
    edit_for_workspace(ctx, ctx.workspace, args, repair, cancel)
}
/// Only the trusted terminal-recovery branch calls this; ordinary schemas stay configured.
pub fn edit_recovered_with_context(
    ctx: &integrations::ext::ToolContext<'_>,
    args: &Value,
    repair: Option<edit::syntax::RepairCallback<'_>>,
    cancel: &std::sync::atomic::AtomicBool,
) -> ToolOutput {
    let workspace = ctx.workspace.clone().with_edit_mode(EditMode::Sloppy);
    edit_for_workspace(ctx, &workspace, args, repair, cancel)
}
fn repair_source_authority(
    ctx: &integrations::ext::ToolContext<'_>,
    bound: &[(String, PathBuf, PathBuf)],
    request: &EditRepairRequest,
) -> Result<(), String> {
    if request.before.is_empty() {
        return Ok(());
    }
    authorize_source_targets(ctx, bound)
}
fn authorize_source_targets(
    ctx: &integrations::ext::ToolContext<'_>,
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
    ctx: &integrations::ext::ToolContext<'_>,
    workspace: &Workspace,
    args: &Value,
    repair: Option<edit::syntax::RepairCallback<'_>>,
    cancel: &std::sync::atomic::AtomicBool,
) -> ToolOutput {
    let bound = match edit::editing::targets(workspace.edit_mode, args)
        .map_err(io::Error::other)
        .and_then(|paths| bind_edit_targets(workspace, paths))
    {
        Ok(bound) => bound,
        Err(error) => return path_refusal(error),
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
    let refuse_syntax_errors = match edit::editing::syntax_gate(workspace, args) {
        Ok(refuse) => refuse,
        Err(error) => return ToolOutput::err(error),
    };
    let execution = edit::editing::Execution {
        repair: effective_repair,
        authorize: Some(&authorize),
        cancel: Some(cancel),
        preview_no_change: None,
        refuse_syntax_errors,
    };
    if workspace.edit_mode == EditMode::Hashline {
        return edit::hashline::input_arg(args)
            .and_then(|input| edit::hashline::edit_output_with(workspace, input, true, &execution))
            .unwrap_or_else(|error| {
                ToolOutput::err(fs::suggest::annotate_edit(workspace, args, error))
            });
    }
    let result = edit::editing::execute_prepared(workspace, args, &execution);
    match result {
        Ok(output) => output,
        Err(error) => ToolOutput::err(fs::suggest::annotate_edit(workspace, args, error)),
    }
}
pub fn write_with_context(
    ctx: &integrations::ext::ToolContext<'_>,
    args: &Value,
    repair: Option<edit::syntax::RepairCallback<'_>>,
    cancel: &std::sync::atomic::AtomicBool,
) -> ToolOutput {
    if let Some(encoding) = args.get("encoding") {
        if encoding != "base64" {
            return ToolOutput::err("write encoding must be base64 when supplied");
        }
        return exec::execution::run(cancel, || {
            fs::data_io::write(ctx, args, cancel).unwrap_or_else(ToolOutput::err)
        });
    }
    if let Some(path) = args
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| integrations::ssh::url::is_ssh_url(path))
    {
        let content = match args.get("content").and_then(Value::as_str) {
            Some(content) => content,
            None => return ToolOutput::err("missing string argument \"content\""),
        };
        if let Err(error) = ctx.authorize_mutation("write", &[], true) {
            return ToolOutput::err(error);
        }
        let user_dir = match integrations::ssh::user_dir() {
            Ok(path) => path,
            Err(error) => return ToolOutput::err(error),
        };
        return match integrations::ssh::write_url(
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
        .map_err(io::Error::other)
        .and_then(|path| bind_edit_targets(ctx.workspace, vec![path.to_owned()]))
    {
        Ok(bound) => bound,
        Err(error) => return path_refusal(error),
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
    let execution = edit::editing::Execution {
        repair: effective_repair,
        authorize: Some(&authorize),
        cancel: Some(cancel),
        preview_no_change: None,
        refuse_syntax_errors: false,
    };
    match fs::files::write_with(ctx.workspace, args, &execution) {
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
        "read" if args.get("format").is_some() => fs::data_io::read(ws, args),
        "read" => match fs::media::read(ws, args, &std::sync::atomic::AtomicBool::new(false), None)
        {
            Some(result) => result,
            None => fs::files::read_tool(ws, args).map(ToolOutput::ok),
        },
        "write" => {
            if args.get("encoding").is_some() {
                return Err("binary writes require controller mutation admission".into());
            }
            if let Err(error) = req_str(args, "path")
                .map_err(io::Error::other)
                .and_then(|path| ws.resolve_checked(path))
            {
                return Ok(path_refusal(error));
            }
            fs::files::write_tool(ws, args).map(ToolOutput::ok)
        }
        "edit" => edit::editing::edit_output(ws, args)
            .map_err(|error| fs::suggest::annotate_edit(ws, args, error)),
        "glob" => fs::search::glob_tool(ws, args).map(ToolOutput::ok),
        "grep" => fs::search::grep_tool(ws, args).map(ToolOutput::ok),
        "bash" => exec::shell::bash_tool(ws, args, timeout_secs),
        _ => Err(format!(
            "unknown tool {name:?}; available tools: {}",
            BUILTIN.join(", ")
        )),
    }
}

/// A model-callable argument is present only when set to a non-null value. Strict-tools schemas make
/// every optional property nullable, so `null` means omitted, never a value.
pub fn present(args: &Value, key: &str) -> bool {
    args.get(key).is_some_and(|value| !value.is_null())
}

pub fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
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
    use exec::sandbox::Policy;
    let ws = match protected {
        Some((state, user)) => ws.with_protected_paths(&[
            state.to_owned(),
            user.to_owned(),
            crate::config::paths::subscription_auth_dir(user)?,
        ]),
        None => ws,
    };
    let mut ws = ws
        .with_web_network_block(net::web::network_disabled(config))
        .with_network_rules(
            exec::sandbox::netproxy::NetworkRules::new(&config.network_allow, &config.network_deny)
                .map_err(anyhow::Error::msg)?,
        );
    if let Some((_, user)) = protected {
        let auth = crate::config::paths::subscription_auth_dir(user)?;
        ws.credential_paths = vec![
            Workspace::canonical_prefix(&user.join("credentials.json")),
            Workspace::canonical_prefix(&crate::llm::chatgpt::credential_path(&auth)),
            Workspace::canonical_prefix(&crate::llm::anthropic_oauth::credential_path(&auth)),
        ];
    }
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
                deny_read.push(crate::config::paths::subscription_auth_dir(user)?);
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
mod tests;

#[cfg(test)]
mod directory_access_tests;
#[cfg(test)]
mod image_tests;
#[cfg(test)]
mod multi_root_tests;

impl crate::storage::experience::SourceRoot for Workspace {
    fn resolve(&self, input: &str) -> Result<PathBuf, String> {
        Workspace::resolve(self, input)
    }
}
