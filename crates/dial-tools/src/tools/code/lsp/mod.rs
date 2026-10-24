//! The `lsp` tool: code intelligence from real language servers (rust-analyzer, gopls, clangd,
//! typescript-language-server, pyright, ...) spoken to over stdio.
//!
//! Servers start lazily, on the first call for a language, and live for the session. Which
//! server handles a file comes from its extension: built-in defaults found on `PATH`, extended
//! or overridden by `[lsp.<language>]` in the config (see [`servers`]).
//!
//! **Approval.** Startup/reload executes project code outside the sandbox. Rename application,
//! code-action application and command execution also require approval. Commands are a separate
//! operation: their external effects cannot be rolled back and unsolicited `workspace/applyEdit`
//! is always refused. A code action's text edit never silently executes its command.
//!
//! Positions are 1-based `line` plus `column` (characters) or `symbol` (text on that line).
//! Rename previews remain read-only. Applied edits are validated as one observer-backed text
//! transaction; ordered file create/rename/delete operations share the same snapshots and rollback. Directory resources require the explicit rename_file workflow.
use crate::config::tools::lsp::LspConfig;
use crate::runtime::execution::Effects;

mod client;
mod format;
mod framing;
#[cfg(all(test, unix))]
mod reliability_tests;
mod servers;
#[cfg(all(test, unix))]
mod tests;
mod text;
mod workspace_edit;

use crate::config::types::Config;
use crate::tools::integrations::ext::{Tool, ToolContext, ToolEvent};
use crate::tools::{
    ToolOutput, Workspace, io_message, opt_bool, opt_str, opt_u64, req_str, tool_schema,
};
use client::{Client, ServerOptions, Timeouts};
use format::Lines;
use serde_json::{Value, json};
use servers::{Resolved, Servers};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};
pub use text::path_from_uri;
use text::{locate, read_source, uri_from_path};
use workspace_edit::{Plan, Snapshot};

/// Post-commit owners, one binding per workspace instance (see [`bind_write_owner`]). Each edit is
/// published only to the service of the session that executed it, never a sibling's.
type WriteThroughOwners = Vec<(Weak<Mutex<()>>, Weak<LspTool>)>;
static WRITE_THROUGH: LazyLock<Mutex<WriteThroughOwners>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// The `lsp` tool, ready to register. Only this instance (and its forks) publishes write-through;
/// the internal copy `find` builds for its metadata does not.
pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    let tool = Arc::new_cyclic(|owner| {
        let mut tool = LspTool::new(&config.lsp);
        tool.write_owner = Some(owner.clone());
        tool
    });
    vec![tool]
}

/// Why a committed transaction is published to the language servers. Only `Edit` formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOrigin {
    /// A tool's own edit: watched-file notices, format-on-write when enabled, then diagnostics.
    Edit,
    /// Rollback of a failed multi-step operation: notices and diagnostics, never formatting.
    Restored,
    /// format-on-write's own commit. The enclosing edit formats once and publishes diagnostics for
    /// the final text, so this origin sends notices and synchronization only.
    Formatted,
}

/// Synchronizes committed files with already-running language servers and returns the notes to
/// append to the edit result: watched-file notices, format-on-write status, and an advisory list of
/// the error diagnostics this edit introduced (bounded wait; none without a live server).
/// `kind` is the LSP `FileChangeType`: 1 created, 2 changed, 3 deleted. The write path starts project
/// code only when `[lsp] start_on_write` opts in, and never holds a workspace lock while a server answers.
/// The servers are those of the tool bound to `ws` (see [`bind_write_owner`]); other sessions' are untouched.
pub fn writethrough(ws: &Workspace, paths: &[(PathBuf, u8)], origin: WriteOrigin) -> String {
    let Some(tool) = write_owner(ws) else {
        return String::new();
    };
    let mut clients: Vec<Arc<Client>> = lock(&tool.running)
        .values()
        .filter(|client| client.root() == ws.root() && client.is_alive())
        .cloned()
        .collect();
    clients.sort_by(|a, b| a.name().cmp(b.name()));
    let mut output = String::new();
    if tool.start_on_write {
        start_for_changes(&tool, ws, &mut clients, paths, &mut output);
    }

    let changes: Vec<(&Path, u8)> = paths
        .iter()
        .map(|(path, kind)| (path.as_path(), *kind))
        .collect();
    for client in &clients {
        if let Err(error) = client.files_changed(&changes) {
            let _ = writeln!(
                output,
                "[LSP {}] watched-file notice failed: {error:#}",
                client.name()
            );
        }
    }

    if tool.format_on_write && origin == WriteOrigin::Edit {
        for (path, kind) in paths {
            if *kind == 3 {
                continue;
            }
            if let Err(note) = format_on_write(&tool, ws, &clients, path) {
                let _ = writeln!(output, "[LSP format {}] {note}", ws.rel(path));
            }
        }
    }
    // The formatted commit's own publish is skipped: the enclosing edit diagnoses the final text.
    if !tool.diagnostics_on_write || origin == WriteOrigin::Formatted {
        return output;
    }
    let mut errors = Vec::new();
    let mut unknown_baseline = false;
    // One budget for the whole edit, however many files and servers it touches.
    let deadline = std::time::Instant::now() + WRITE_DIAGNOSTICS_BUDGET;
    for (path, kind) in paths {
        if *kind == 3 {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        for client in &clients {
            let Some(language) = tool
                .servers
                .language_id_of(client.name(), &path.to_string_lossy())
            else {
                continue;
            };
            // What the server said before this edit: only errors beyond these are reported.
            let before = client.last_diagnostics(path);
            let mark = client.diagnostics_mark(path);
            // A document the server has not seen is opened here; `changed` is false when it already
            // holds this exact text, so the answer is the current publish rather than a new one.
            let Ok(changed) = client.sync_document(path, &language, &source) else {
                continue;
            };
            if client.did_save(path, &source).is_err() {
                continue;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                continue;
            }
            let Some(items) = client.diagnostics_bounded(path, mark, changed, left) else {
                continue;
            };
            let new = new_errors(before.as_deref().unwrap_or_default(), &items);
            unknown_baseline |= before.is_none() && !new.is_empty();
            errors.extend(new.into_iter().map(|item| diagnostic_line(ws, path, item)));
        }
    }
    output.push_str(&new_errors_note(&errors, unknown_baseline));
    output
}

/// Longest an edit waits for a live server's diagnostics of the new text, pull request included.
const WRITE_DIAGNOSTICS_BUDGET: std::time::Duration = std::time::Duration::from_millis(1500);
/// Most new-error lines appended to one edit result.
const WRITE_DIAGNOSTICS_LINES: usize = 8;

/// Error-severity items of `after` with no counterpart in `before`. Positions are ignored, since an
/// edit shifts lines; each earlier error with the same message, code and source absorbs one item.
fn new_errors<'a>(before: &[Value], after: &'a [Value]) -> Vec<&'a Value> {
    let is_error = |item: &&Value| item.get("severity").and_then(Value::as_u64) == Some(1);
    let key = |item: &Value| {
        (
            item.get("message")
                .and_then(Value::as_str)
                .map(str::to_owned),
            item.get("code").map(Value::to_string),
            item.get("source")
                .and_then(Value::as_str)
                .map(str::to_owned),
        )
    };
    let mut known: HashMap<_, usize> = HashMap::new();
    for item in before.iter().filter(is_error) {
        *known.entry(key(item)).or_default() += 1;
    }
    after
        .iter()
        .filter(is_error)
        .filter(|item| match known.get_mut(&key(item)) {
            Some(count) if *count > 0 => {
                *count -= 1;
                false
            }
            _ => true,
        })
        .collect()
}

/// `path:line:col: message` for one diagnostic (1-based positions, first message line).
fn diagnostic_line(ws: &Workspace, path: &Path, item: &Value) -> String {
    let message = item
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("diagnostic");
    let message = message.lines().next().unwrap_or_default().trim();
    let position = |key: &str| {
        item.pointer(&format!("/range/start/{key}"))
            .and_then(Value::as_u64)
            .map_or(1, |n| n.saturating_add(1))
    };
    format!(
        "{}:{}:{}: {message}",
        ws.rel(path),
        position("line"),
        position("character")
    )
}

/// The advisory block appended to an edit result; empty when the edit added no errors.
fn new_errors_note(errors: &[String], unknown_baseline: bool) -> String {
    if errors.is_empty() {
        return String::new();
    }
    let mut note = format!(
        "[LSP advisory, not verification] {} new error(s) after this edit{}:\n",
        errors.len(),
        if unknown_baseline {
            " (file had no earlier diagnostics; some may predate the edit)"
        } else {
            ""
        }
    );
    for line in errors.iter().take(WRITE_DIAGNOSTICS_LINES) {
        note.push_str(line);
        note.push('\n');
    }
    if errors.len() > WRITE_DIAGNOSTICS_LINES {
        let _ = writeln!(
            note,
            "... {} more; lsp diagnostics lists all",
            errors.len() - WRITE_DIAGNOSTICS_LINES
        );
    }
    note
}

/// The tool bound to this workspace instance, if it is still alive.
fn write_owner(ws: &Workspace) -> Option<Arc<LspTool>> {
    let token = Arc::downgrade(&ws.operations);
    lock(&WRITE_THROUGH)
        .iter()
        .find(|(bound, _)| Weak::ptr_eq(bound, &token))
        .and_then(|(_, owner)| owner.upgrade())
}

/// Binds `owner` as the write-through owner of `ws`'s instance, replacing that instance's previous
/// binding and dropping entries whose owner or workspace is gone. The key is the workspace's
/// serialization domain: a `rebased` child copy gets its own, while clones of one session share it.
/// Binding never stops another owner's servers; it only chooses who receives this instance's edits.
fn bind_write_owner(ws: &Workspace, owner: &Weak<LspTool>) {
    let token = Arc::downgrade(&ws.operations);
    let mut bindings = lock(&WRITE_THROUGH);
    bindings.retain(|(bound, live)| {
        bound.strong_count() > 0 && live.strong_count() > 0 && !Weak::ptr_eq(bound, &token)
    });
    bindings.push((token, owner.clone()));
}

/// Starts the server that serves each changed file when `[lsp] start_on_write` opted in. A server
/// with a live session is reused; a failed start is reported in the notes instead of being dropped.
fn start_for_changes(
    tool: &LspTool,
    ws: &Workspace,
    clients: &mut Vec<Arc<Client>>,
    paths: &[(PathBuf, u8)],
    output: &mut String,
) {
    let dirs = servers::path_dirs();
    for (path, kind) in paths {
        if *kind == 3 || tool.servers.language_for(&path.to_string_lossy()).is_err() {
            continue;
        }
        let resolved = match tool.servers.select(ws.root(), path, &dirs) {
            Ok(resolved) => resolved,
            Err(error) => {
                let _ = writeln!(output, "[LSP] {}: {error}", ws.rel(path));
                continue;
            }
        };
        if lock(&tool.running)
            .get(&resolved.language)
            .is_some_and(|client| client.is_alive())
        {
            continue;
        }
        match (tool.launch)(&resolved, ws.root(), &|| false) {
            Ok(client) => {
                let client = Arc::new(client);
                let (kept, duplicate) = {
                    let mut running = lock(&tool.running);
                    match running
                        .get(&resolved.language)
                        .filter(|c| c.is_alive())
                        .cloned()
                    {
                        Some(live) => (live, Some(client)),
                        None => {
                            running.insert(resolved.language.clone(), Arc::clone(&client));
                            (client, None)
                        }
                    }
                };
                // A concurrent start won the race; dropping the duplicate stops it outside the map lock.
                drop(duplicate);
                clients.push(kept);
            }
            Err(error) => {
                let _ = writeln!(
                    output,
                    "[LSP {}] start_on_write could not start the {} language server: {error:#}",
                    ws.rel(path),
                    resolved.language
                );
            }
        }
    }
}

/// Applies the first running server's whole-document formatting for `path` as a checkpointed
/// transaction, then re-syncs the servers so their diagnostics see the formatted text.
fn format_on_write(
    tool: &LspTool,
    ws: &Workspace,
    clients: &[Arc<Client>],
    path: &Path,
) -> Result<String, String> {
    let source = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let file = path.to_string_lossy();
    for client in clients {
        let Some(language) = tool.servers.language_id_of(client.name(), &file) else {
            continue;
        };
        client
            .sync_document(path, &language, &source)
            .map_err(|error| format!("{}: {error:#}", client.name()))?;
        let edits = client
            .format(path, &source)
            .map_err(|error| format!("{}: {error:#}", client.name()))?;
        let Some(edits) = edits else {
            continue;
        };
        if edits.is_empty() {
            return Ok(String::new());
        }
        let snapshot = Snapshot::capture(ws)?;
        let value = json!({"changes": {text::uri_from_path(path): edits}});
        let plan = Plan::prepare(ws, client, &snapshot, &value)?;
        let mut notes = plan
            .commit_as(ws, &snapshot, WriteOrigin::Formatted)?
            .join("\n");
        notes.push_str(&synchronize_applied(tool, client, &plan, client.name()));
        return Ok(notes);
    }
    Ok(String::new())
}

const DESCRIPTION: &str = "Code intelligence and refactoring from real language servers; startup, edits and commands need approval. \
`path` selects the server; position actions need 1-based `line` plus `column` or `symbol`. \
`rename_preview` previews `new_name`; `rename` applies every validated server-provided text edit \
as one observer/checkpoint-backed transaction. Server-provided resource create/rename/delete operations are applied only by these approved calls, in the same validated transaction as their text edits; directory resources are refused (use `rename_file`). \
`rename_file` moves `path` (a file, or a directory of files) to workspace-relative `new_name`; each language server for `path` first answers `workspace/willRenameFiles` (starting it under this approved call), then those edits and the move commit as one checkpointed transaction. `apply:false` previews without writing. \
`code_actions` lists actions at the cursor (optional end_line/end_column range and query kind); \
use its session-local `action_id` with `code_action_resolve` to preview, or approved `code_action_apply` \
to apply text edits. Commands are NEVER automatically run: `execute_command` requires a separate \
approved call with action_id; unsandboxed command effects cannot be rolled back and server applyEdit \
requests are refused. Stale versions/files, invalid UTF-16 ranges, overlaps and outside-root edits \
fail before writing. `status` lists existing sessions without starting servers; `capabilities` \
shows a selected live session's real initialize/dynamic capabilities; approved `reload` stops and \
restarts the selected session. First startup and reload run project code outside the sandbox.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Definition,
    References,
    Hover,
    Implementation,
    TypeDefinition,
    DocumentSymbols,
    WorkspaceSymbols,
    Diagnostics,
    RenamePreview,
    Rename,
    RenameFile,
    CodeActions,
    CodeActionResolve,
    CodeActionApply,
    ExecuteCommand,
    Status,
    Reload,
    Capabilities,
}

const ACTIONS: [(&str, Action); 18] = [
    ("definition", Action::Definition),
    ("references", Action::References),
    ("hover", Action::Hover),
    ("implementation", Action::Implementation),
    ("type_definition", Action::TypeDefinition),
    ("document_symbols", Action::DocumentSymbols),
    ("workspace_symbols", Action::WorkspaceSymbols),
    ("diagnostics", Action::Diagnostics),
    ("rename_preview", Action::RenamePreview),
    ("rename", Action::Rename),
    ("rename_file", Action::RenameFile),
    ("code_actions", Action::CodeActions),
    ("code_action_resolve", Action::CodeActionResolve),
    ("code_action_apply", Action::CodeActionApply),
    ("execute_command", Action::ExecuteCommand),
    ("status", Action::Status),
    ("reload", Action::Reload),
    ("capabilities", Action::Capabilities),
];

impl Action {
    fn parse(name: &str) -> Result<Self, String> {
        ACTIONS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, action)| *action)
            .ok_or_else(|| {
                let names: Vec<&str> = ACTIONS.iter().map(|(n, _)| *n).collect();
                format!("unknown action {name:?}; use one of: {}", names.join(", "))
            })
    }

    fn needs_position(self) -> bool {
        matches!(
            self,
            Self::Definition
                | Self::References
                | Self::Hover
                | Self::Implementation
                | Self::TypeDefinition
                | Self::RenamePreview
                | Self::Rename
                | Self::CodeActions
        )
    }
}

/// Starts a language server for `Resolved` in the given workspace root.
type BoxedLauncher =
    Box<dyn Fn(&Resolved, &Path, &dyn Fn() -> bool) -> anyhow::Result<Client> + Send + Sync>;
type Launcher =
    Arc<dyn Fn(&Resolved, &Path, &dyn Fn() -> bool) -> anyhow::Result<Client> + Send + Sync>;

pub struct LspTool {
    servers: Servers,
    launch: Launcher,
    /// One live client per language, started on first use.
    running: Mutex<HashMap<String, Arc<Client>>>,
    starting: Mutex<Option<Arc<AtomicBool>>>,
    actions: Mutex<HashMap<String, CachedAction>>,
    next_action: AtomicU64,
    session_id: u64,
    operation: Mutex<()>,
    diagnostics_on_write: bool,
    format_on_write: bool,
    start_on_write: bool,
    /// Set on a registered instance and its forks: the owner that publishes committed edits.
    write_owner: Option<Weak<LspTool>>,
}

#[derive(Clone)]
struct CachedAction {
    client: Arc<Client>,
    source: std::path::PathBuf,
    language: String,
    snapshot: Arc<Snapshot>,
    action: Value,
    applied: bool,
    command_executed: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err("LSP operation cancelled".into())
    } else {
        Ok(())
    }
}

fn operation_lock<'a>(
    mutex: &'a Mutex<()>,
    cancel: &AtomicBool,
) -> Result<MutexGuard<'a, ()>, String> {
    loop {
        check_cancel(cancel)?;
        match mutex.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
        }
    }
}

fn workspace_lock<'a>(
    ws: &'a Workspace,
    cancel: &AtomicBool,
) -> Result<MutexGuard<'a, ()>, String> {
    loop {
        check_cancel(cancel)?;
        if let Some(guard) = ws.try_serial_operation() {
            return Ok(guard);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Installs a missing built-in language server for `abs` the first time it is needed.
impl LspTool {
    /// `select_all`, and when no server for the file is installed, installs the first built-in one
    /// that has a Dial recipe (into Dial's tools directory, never with sudo) and selects again.
    /// The install is gated like a server start; failures are folded into the original error.
    fn select_all_installing(
        &self,
        ctx: &ToolContext,
        abs: &Path,
        cancel: &AtomicBool,
    ) -> Result<Vec<Resolved>, String> {
        let root = ctx.workspace.root();
        let error = match self.servers.select_all(root, abs, &servers::path_dirs()) {
            Ok(found) => return Ok(found),
            Err(error) => error,
        };
        let wanted = self.servers.installable(root, abs, &servers::path_dirs());
        if wanted.is_empty() {
            return Err(error);
        }
        ctx.authorize_mutation("lsp", &[], true)?;
        let env = crate::setup::deps::Env::current(&crate::config::types::default_user_dir());
        let mut failures = Vec::new();
        for program in &wanted {
            check_cancel(cancel)?;
            match crate::setup::deps::run::install_with_requirements(
                program,
                &env,
                cancel,
                &mut |_| {},
            ) {
                Ok(_) => {
                    return self.servers.select_all(root, abs, &servers::path_dirs());
                }
                Err(failure) => failures.push(format!("{program}: {failure:#}")),
            }
        }
        Err(format!(
            "{error}; automatic install failed ({})",
            failures.join("; ")
        ))
    }
}

impl LspTool {
    pub fn new(config: &LspConfig) -> Self {
        let mut tool = Self::with_launcher(
            config,
            Box::new(|server, root, cancel| {
                Client::spawn_cancellable(
                    &server.language,
                    &server.program,
                    &server.args,
                    root,
                    Timeouts::default(),
                    ServerOptions {
                        settings: server.settings.clone(),
                        init_options: server.init_options.clone(),
                    },
                    cancel,
                )
            }),
        );
        tool.diagnostics_on_write = config.diagnostics_on_write;
        tool.format_on_write = config.format_on_write;
        tool
    }

    fn with_launcher(config: &LspConfig, launch: BoxedLauncher) -> Self {
        Self {
            servers: Servers::new(config),
            launch: Arc::from(launch),
            running: Mutex::new(HashMap::new()),
            starting: Mutex::new(None),
            actions: Mutex::new(HashMap::new()),
            next_action: AtomicU64::new(1),
            session_id: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            operation: Mutex::new(()),
            diagnostics_on_write: config.diagnostics_on_write,
            format_on_write: config.format_on_write,
            start_on_write: config.start_on_write,
            write_owner: None,
        }
    }
    fn is_running(&self, language: &str) -> bool {
        lock(&self.running)
            .get(language)
            .is_some_and(|client| client.is_alive())
    }

    /// The live client for `server`'s language, starting (or restarting a dead) one if needed.
    fn client(
        &self,
        ctx: &ToolContext,
        server: &Resolved,
        cancel: &AtomicBool,
    ) -> Result<Arc<Client>, String> {
        let ws = ctx.workspace;
        let existing = lock(&self.running).get(&server.language).cloned();
        if let Some(client) = existing {
            if client.root() != ws.root() {
                return Err("language-server session belongs to a different workspace root; fork_session is required".into());
            }
            if client.is_alive() {
                return Ok(client);
            }
            lock(&self.running).remove(&server.language);
        }
        // A formerly live server can die after needs_approval was evaluated. Gate the actual
        // lazy spawn, not just the caller's initially predicted action.
        ctx.authorize_mutation("lsp", &[], true)?;
        let startup_cancel = Arc::new(AtomicBool::new(false));
        *lock(&self.starting) = Some(Arc::clone(&startup_cancel));
        if let Err(error) = check_cancel(cancel) {
            lock(&self.starting).take();
            return Err(error);
        }
        let launched = (self.launch)(server, ws.root(), &|| {
            startup_cancel.load(Ordering::Acquire) || cancel.load(Ordering::Acquire)
        });
        let mut starting = lock(&self.starting);
        // Keep the start token registered until the client is visible to cancellation.
        // Cancellation takes this same lock before snapshotting the running clients.
        starting.take();
        let client = launched.map_err(|e| {
            format!(
                "could not start the {} language server ({}): {e:#}",
                server.language,
                server.program.display()
            )
        })?;
        if startup_cancel.load(Ordering::Acquire) || cancel.load(Ordering::Acquire) {
            client.terminate();
            return Err("LSP startup cancelled".into());
        }
        let client = Arc::new(client);
        lock(&self.running).insert(server.language.clone(), Arc::clone(&client));
        Ok(client)
    }

    /// `rename_file` moves a file or a directory of files. Each language server for the moved files
    /// answers `workspace/willRenameFiles` first; its text edits and the moves are committed together.
    /// A failure before the moves leaves the workspace unchanged, and a failed move restores both.
    fn rename_file(
        &self,
        ctx: &ToolContext,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<String, String> {
        let ws = ctx.workspace;
        let file = req_str(args, "path")?;
        let destination = req_str(args, "new_name")?;
        let apply = match args.get("apply") {
            None | Some(Value::Null) => true,
            Some(_) => opt_bool(args, "apply")?,
        };
        let _workspace = workspace_lock(ws, cancel)?;
        let from = ws.resolve(file)?;
        let to = ws.resolve(destination)?;
        ws.check_protected_mutation(&from)?;
        ws.check_protected_mutation(&to)?;
        let pairs = rename_pairs(ws, &from, &to)?;
        // Startup and willRenameFiles can run project code or wait while external writers act.
        // Capture before either, including sources a server does not return text edits for.
        let snapshot = Snapshot::capture(ws)?;
        let units = if from.is_dir() {
            vec![(from.clone(), to.clone())]
        } else {
            pairs.clone()
        };

        // Files no server understands need no request; configured-but-missing servers still error.
        // Each server is asked only about the pairs it is responsible for.
        let mut resolved: Vec<Resolved> = Vec::new();
        let mut routed: Vec<Vec<usize>> = Vec::new();
        for (index, (source, _)) in pairs.iter().enumerate() {
            if self.servers.names_for(&source.to_string_lossy()).is_err() {
                continue;
            }
            for server in self
                .servers
                .select_all(ws.root(), source, &servers::path_dirs())?
            {
                match resolved.iter().position(|known| *known == server) {
                    Some(slot) => routed[slot].push(index),
                    None => {
                        resolved.push(server);
                        routed.push(vec![index]);
                    }
                }
            }
        }
        let mut started: Vec<(Resolved, Arc<Client>, Vec<usize>)> =
            Vec::with_capacity(resolved.len());
        for (server, indices) in resolved.into_iter().zip(routed) {
            let client = self.client(ctx, &server, cancel)?;
            started.push((server, client, indices));
        }

        // One merged `WorkspaceEdit` per server, validated against that server's own documents.
        let mut server_edits: Vec<Value> = Vec::with_capacity(started.len());
        for (server, client, indices) in &started {
            let mut values = Vec::with_capacity(indices.len());
            for &index in indices {
                let (source, target) = &pairs[index];
                let value = client.will_rename(source, target).map_err(|error| {
                    format!(
                        "aborted rename; workspace/willRenameFiles failed on {}: {error:#}. No files were moved.",
                        server.language
                    )
                })?;
                values.push(value);
            }
            server_edits.push(merge_workspace_edits(&values)?);
        }
        for (source, _) in &pairs {
            snapshot.validate(ws, source)?;
        }
        let sources: Vec<(&Client, &Value)> = started
            .iter()
            .zip(&server_edits)
            .map(|((_, client, _), value)| (&**client, value))
            .collect();
        let plan = if sources.is_empty() {
            Plan::empty()
        } else {
            Plan::prepare_sources(ws, &sources, &snapshot)?
        };

        if !apply {
            let listing = pairs
                .iter()
                .map(|(source, target)| format!("{} -> {}", ws.rel(source), ws.rel(target)))
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(format!(
                "Rename preview (nothing written): {} file(s)\n{listing}\nLanguage-server edits:\n{}",
                pairs.len(),
                plan.summary(ws)
            ));
        }

        check_cancel(cancel)?;
        let mut targets: Vec<PathBuf> = plan.targets().to_vec();
        for (source, target) in &pairs {
            targets.push(source.clone());
            targets.push(target.clone());
        }
        ctx.authorize_mutation("lsp", &targets, false)?;
        let language = started
            .first()
            .map(|(server, _, _)| server.language.clone())
            .unwrap_or_default();
        // Approval may itself wait; revalidate even sources absent from the text-edit plan.
        for (source, _) in &pairs {
            snapshot.validate(ws, source)?;
        }
        let mut notes = plan.commit(ws, &snapshot)?.join("\n");
        if let Some((_, client, _)) = started.first() {
            notes.push_str(&synchronize_applied(self, client, &plan, &language));
        }

        // The moves are file mutations: record each target as created and each source as removed
        // before any of them happens, so `/undo` can restore them with the text edits.
        let observed = pairs.iter().try_for_each(|(source, target)| {
            ws.observe_write(target)?;
            ws.observe_delete(source)
        });
        if let Err(error) = observed.and_then(|()| move_units(&units)) {
            let revert = match plan.revert(ws) {
                Ok(warnings) if warnings.is_empty() => "text edits were restored".to_string(),
                Ok(warnings) => format!("text edits were restored ({})", warnings.join("; ")),
                Err(reason) => format!("text edits could not be restored: {reason}"),
            };
            if let Some((_, client, _)) = started.first() {
                notes.push_str(&synchronize_applied(self, client, &plan, &language));
            }
            return Err(format!("{error}; {revert}{notes}"));
        }

        for (server, client, indices) in &started {
            for &index in indices {
                let (source, target) = &pairs[index];
                if let Err(error) = client.did_rename(source, target) {
                    notes.push_str(&format!(
                        "\n{}: didRenameFiles failed: {error:#}",
                        server.language
                    ));
                }
            }
        }
        {
            let mut store = ws.store();
            for (source, target) in &pairs {
                store.forget(source);
                store.forget(target);
            }
        }
        Ok(format!(
            "Renamed {} -> {} ({} file(s))\nLanguage-server edits:\n{}{notes}",
            ws.rel(&from),
            ws.rel(&to),
            pairs.len(),
            plan.summary(ws)
        ))
    }

    fn run(
        &self,
        ctx: &ToolContext,
        args: &Value,
        cancel: &AtomicBool,
        effects: &mut Effects,
    ) -> Result<String, String> {
        let ws = ctx.workspace;
        if let Some(owner) = &self.write_owner {
            bind_write_owner(ws, owner);
        }
        let action = Action::parse(req_str(args, "action")?)?;
        if action == Action::Status {
            let sessions: Vec<Value> = lock(&self.running)
                .values()
                .filter(|client| client.root() == ws.root())
                .map(|client| client.status())
                .collect();
            // Candidate names are a pure lookup: naming them never starts a server.
            let candidates = match opt_str(args, "path")? {
                Some(file) => match self.servers.names_for(file) {
                    Ok(names) => json!(names),
                    Err(error) => json!({"error": error}),
                },
                None => Value::Null,
            };
            return Ok(json!({
                "sessions": sessions,
                "starts_servers": false,
                "candidates": candidates,
            })
            .to_string());
        }
        let _session = operation_lock(&self.operation, cancel)?;
        if action == Action::RenameFile {
            return self.rename_file(ctx, args, cancel);
        }
        let file = req_str(args, "path")?;
        let abs = ws.resolve(file)?;
        if !abs.is_file() {
            return Err(format!("{file:?} is not a file in the workspace"));
        }
        let source = read_source(&abs).map_err(|e| io_message("read", file, &e))?;
        // Resolve the position first so a bad request never starts a server.
        let cursor = if action.needs_position() {
            Some(cursor(args, &source)?)
        } else {
            None
        };
        let query = match action {
            Action::WorkspaceSymbols => Some(
                opt_str(args, "query")?
                    .filter(|q| !q.is_empty())
                    .ok_or("workspace_symbols needs a non-empty `query`")?,
            ),
            _ => None,
        };
        let new_name = match action {
            Action::RenamePreview | Action::Rename => Some(
                opt_str(args, "new_name")?
                    .filter(|n| !n.is_empty())
                    .ok_or("rename and rename_preview need `new_name`")?,
            ),
            _ => None,
        };

        let selected = self.select_all_installing(ctx, &abs, cancel)?;
        let language = selected
            .first()
            .map(|server| server.language.as_str())
            .ok_or("no language server selected")?;
        if action == Action::Capabilities {
            let running = lock(&self.running);
            let client = running.get(language)
                .filter(|client| client.root() == ws.root() && client.is_alive())
                .ok_or("no live session for this workspace/language; capabilities does not start a server")?;
            return Ok(
                json!({"session": client.status(), "capabilities": client.capabilities()})
                    .to_string(),
            );
        }
        if matches!(
            action,
            Action::CodeActionResolve | Action::CodeActionApply | Action::ExecuteCommand
        ) {
            return self.run_cached(ctx, args, &abs, language, action, cancel, effects);
        }
        // Startup/reload can execute project code too, so own the workspace barrier before it.
        let _workspace = workspace_lock(ws, cancel)?;
        let server = self
            .servers
            .select(ws.root(), &abs, &servers::path_dirs())?;
        if action == Action::Reload {
            ctx.authorize_mutation("lsp", &[], true)?;
        }
        if action == Action::Reload {
            if lock(&self.running)
                .get(&server.language)
                .is_some_and(|client| client.root() != ws.root())
            {
                return Err("cannot reload another workspace's language server".into());
            }
            lock(&self.actions).retain(|_, cached| cached.language != server.language);
            let previous = lock(&self.running).remove(&server.language);
            if let Some(client) = previous {
                client.stop();
            }
        }
        check_cancel(cancel)?;
        let client = self.client(ctx, &server, cancel)?;
        check_cancel(cancel)?;
        if action == Action::Reload {
            return Ok(
                json!({"reloaded": server.language, "session": client.status(),
                "capabilities": client.capabilities()})
                .to_string(),
            );
        }
        let io = |e: anyhow::Error| format!("{e:#}");
        let mark = client.diagnostics_mark(&abs);
        let stale = client.refresh_open_documents(&abs);
        let fresh = client
            .sync_document(&abs, &server.language_id_for(&abs), &source)
            .map_err(io)?;

        let document = json!({"uri": uri_from_path(&abs)});
        let at = cursor.as_ref().map(|c| json!({"textDocument": document, "position": {"line": c.line, "character": c.character}}));
        let mut lines = Lines::new(ws);
        let text = match action {
            Action::Definition | Action::Implementation | Action::TypeDefinition => {
                let (method, noun) = match action {
                    Action::Definition => ("textDocument/definition", "definition"),
                    Action::Implementation => ("textDocument/implementation", "implementation"),
                    _ => ("textDocument/typeDefinition", "type definition"),
                };
                let result = client.request(method, at.unwrap_or_default()).map_err(io)?;
                format::locations(&mut lines, &result, noun)
            }
            Action::References => {
                let mut params = at.unwrap_or_default();
                params["context"] = json!({"includeDeclaration": true});
                let result = client
                    .request("textDocument/references", params)
                    .map_err(io)?;
                format::locations(&mut lines, &result, "reference")
            }
            Action::Hover => format::hover(
                &client
                    .request("textDocument/hover", at.unwrap_or_default())
                    .map_err(io)?,
            ),
            Action::DocumentSymbols => {
                let result = client
                    .request(
                        "textDocument/documentSymbol",
                        json!({"textDocument": document}),
                    )
                    .map_err(io)?;
                format::document_symbols(&mut lines, &abs, &result)
            }
            Action::WorkspaceSymbols => {
                let result = client
                    .request("workspace/symbol", json!({"query": query}))
                    .map_err(io)?;
                format::workspace_symbols(&mut lines, &result)
            }
            Action::Diagnostics => match client.diagnostics(&abs, mark, fresh) {
                Some(items) => format::diagnostics(&mut lines, &abs, &items),
                None => format!(
                    "the {} language server published no diagnostics for {file} in time (it may still be indexing); try again shortly",
                    server.language
                ),
            },
            Action::RenamePreview | Action::Rename => {
                let snapshot = Snapshot::capture(ws)?;
                snapshot.validate_source(ws, &abs, &source)?;
                let mut params = at.unwrap_or_default();
                params["newName"] = json!(new_name);
                check_cancel(cancel)?;
                let result = client.request("textDocument/rename", params).map_err(io)?;
                snapshot.validate(ws, &abs)?;
                let plan = Plan::prepare(ws, &client, &snapshot, &result)?;
                if action == Action::RenamePreview {
                    format!(
                        "{}\nValidated text transaction:\n{}",
                        format::workspace_edit(&mut lines, &result),
                        plan.summary(ws)
                    )
                } else {
                    check_cancel(cancel)?;
                    ctx.authorize_mutation("lsp", plan.targets(), false)?;
                    let transaction_warnings = plan.commit(ws, &snapshot)?;
                    let mut warnings = synchronize_applied(self, &client, &plan, &server.language);
                    if !transaction_warnings.is_empty() {
                        warnings.push_str(&format!("\n{}", transaction_warnings.join("\n")));
                    }
                    format!("Applied rename:\n{}{}", plan.summary(ws), warnings)
                }
            }
            Action::CodeActions => {
                let start = cursor.as_ref().ok_or("code_actions needs a position")?;
                let end = if let Some(line) = opt_u64(args, "end_line")? {
                    locate(&source, line, opt_u64(args, "end_column")?, None)?
                } else {
                    if crate::tools::present(args, "end_column") {
                        return Err("end_column needs end_line".into());
                    }
                    text::Located {
                        line: start.line,
                        character: start.character,
                        column: start.column,
                    }
                };
                if (end.line, end.character) < (start.line, start.character) {
                    return Err("code-action range is reversed".into());
                }
                let snapshot = Arc::new(Snapshot::capture(ws)?);
                let diagnostics = client.diagnostics(&abs, mark, fresh);
                let mut context = json!({"diagnostics": diagnostics.clone().unwrap_or_default(), "triggerKind": 1});
                if let Some(kind) = opt_str(args, "query")?.filter(|kind| !kind.is_empty()) {
                    context["only"] = json!([kind]);
                }
                snapshot.validate_source(ws, &abs, &source)?;
                check_cancel(cancel)?;
                let result = client
                    .request(
                        "textDocument/codeAction",
                        json!({
                            "textDocument": document,
                            "range": {"start": {"line": start.line, "character": start.character},
                                "end": {"line": end.line, "character": end.character}},
                            "context": context,
                        }),
                    )
                    .map_err(io)?;
                snapshot.validate(ws, &abs)?;
                let returned = if result.is_null() {
                    &[][..]
                } else {
                    result
                        .as_array()
                        .ok_or("server returned a non-array code-action result")?
                        .as_slice()
                };
                let mut listed = Vec::new();
                let mut built = Vec::new();
                for item in returned.iter().take(128) {
                    let title = item
                        .get("title")
                        .and_then(Value::as_str)
                        .ok_or("code action has no title")?;
                    let id = format!(
                        "lsp-action-{}-{}",
                        self.session_id,
                        self.next_action.fetch_add(1, Ordering::Relaxed)
                    );
                    listed.push(
                        json!({"action_id": id, "title": title, "kind": item.get("kind"),
                        "disabled": item.get("disabled"), "has_edit": item.get("edit").is_some(),
                        "has_command": command_of(item).is_some()}),
                    );
                    built.push((
                        id,
                        CachedAction {
                            client: Arc::clone(&client),
                            source: abs.clone(),
                            language: server.language.clone(),
                            snapshot: Arc::clone(&snapshot),
                            action: item.clone(),
                            applied: false,
                            command_executed: false,
                        },
                    ));
                }
                let mut actions = lock(&self.actions);
                actions.clear();
                actions.extend(built);
                json!({"actions": listed, "total": returned.len(), "retained": listed.len(),
                    "diagnostics_context_available": diagnostics.is_some(),
                    "limitation": "IDs expire on the next listing or reload; only the first 128 actions are retained. Use query to narrow by kind. Commands require a separate approved execute_command call."}).to_string()
            }
            Action::CodeActionResolve
            | Action::CodeActionApply
            | Action::ExecuteCommand
            | Action::Status
            | Action::Reload
            | Action::Capabilities
            | Action::RenameFile => unreachable!("handled before document requests"),
        };
        if stale.is_empty() {
            Ok(text)
        } else {
            Ok(format!(
                "{text}\nSome open documents could not be re-synchronized and were closed; they reopen on next use: {}",
                stale.join("; ")
            ))
        }
    }

    fn run_cached(
        &self,
        ctx: &ToolContext,
        args: &Value,
        abs: &Path,
        language: &str,
        operation: Action,
        cancel: &AtomicBool,
        effects: &mut Effects,
    ) -> Result<String, String> {
        let ws = ctx.workspace;
        let id = req_str(args, "action_id")?;
        let mut cached = lock(&self.actions)
            .get(id)
            .cloned()
            .ok_or("unknown or expired action_id; list code_actions in this session first")?;
        if cached.client.root() != ws.root() || cached.source != abs || cached.language != language
        {
            return Err("action_id belongs to a different workspace, file or language".into());
        }
        if !cached.client.is_alive() {
            return Err("action's language-server session has exited; list actions again".into());
        }
        if cached
            .action
            .get("disabled")
            .is_some_and(|value| !value.is_null())
        {
            return Err(format!(
                "code action is disabled: {}",
                cached.action["disabled"]
            ));
        }
        let _workspace = workspace_lock(ws, cancel)?;
        if operation == Action::ExecuteCommand {
            if cached.command_executed {
                return Err(
                    "this action's command was already executed; list actions again".into(),
                );
            }
            if cached
                .action
                .get("edit")
                .is_some_and(|edit| !edit.is_null())
                && !cached.applied
            {
                return Err("apply the action's text edit first; its command is a separate approved operation".into());
            }
            let command =
                command_of(&cached.action).ok_or("this action has no resolved command")?;
            let name = command
                .get("command")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or("command has no nonempty name")?;
            let arguments = match command.get("arguments") {
                None => json!([]),
                Some(arguments) if arguments.is_array() => arguments.clone(),
                _ => return Err("command arguments must be an array".into()),
            };
            check_cancel(cancel)?;
            ctx.authorize_mutation("lsp", &[], true)?;
            // Mark before sending: a timeout is not proof the server failed to execute.
            cached.command_executed = true;
            lock(&self.actions).insert(id.to_owned(), cached.clone());
            let result = cached.client.request("workspace/executeCommand", json!({
                "command": name, "arguments": arguments,
            })).map_err(|e| {
                if e.downcast_ref::<client::UnsettledRequest>().is_some() {
                    *effects = Effects::Unknown;
                    format!("command may have executed before failure: {e:#}; external effects are not transactional")
                } else {
                    format!("command failed with a terminal response: {e:#}; external effects are not transactional")
                }
            })?;
            return Ok(json!({"executed_command": name, "result": result,
                "limitations": "Separate approved unsandboxed operation; direct server filesystem/process/network effects cannot be rolled back. workspace/applyEdit requests remain refused."}).to_string());
        }
        if cached.applied {
            return Err("this action's edit was already applied; list actions again".into());
        }
        cached.snapshot.validate(ws, abs)?;
        if cached.action["command"].is_string() {
            return Ok(format!(
                "Command-only action; no text edits were applied. Use a separate approved execute_command with action_id {id}."
            ));
        }
        if cached.client.can_resolve_code_action()
            && (operation == Action::CodeActionResolve
                || cached.action.get("data").is_some()
                || cached.action.get("edit").is_none_or(Value::is_null))
        {
            check_cancel(cancel)?;
            let resolved = cached
                .client
                .request("codeAction/resolve", cached.action.clone())
                .map_err(|e| format!("{e:#}"))?;
            if !resolved.is_object() || resolved["title"].as_str().is_none() {
                return Err("server returned an invalid resolved code action".into());
            }
            cached.action = resolved;
        }
        if cached
            .action
            .get("disabled")
            .is_some_and(|value| !value.is_null())
        {
            return Err(format!(
                "resolved code action is disabled: {}",
                cached.action["disabled"]
            ));
        }
        let edit = cached.action.get("edit").unwrap_or(&Value::Null);
        let plan = Plan::prepare(ws, &cached.client, &cached.snapshot, edit)?;
        let title = cached.action["title"]
            .as_str()
            .unwrap_or("code action")
            .to_owned();
        let command_pending = command_of(&cached.action).is_some();
        lock(&self.actions).insert(id.to_owned(), cached.clone());
        let text = if operation == Action::CodeActionApply {
            if edit.is_null() {
                return Err(if command_pending {
                    format!(
                        "action {title:?} has only a command; use code_action_resolve then a separate approved execute_command with action_id {id}"
                    )
                } else {
                    format!("action {title:?} has no text edit or command")
                });
            }
            check_cancel(cancel)?;
            ctx.authorize_mutation("lsp", plan.targets(), false)?;
            let transaction_warnings = plan.commit(ws, &cached.snapshot)?;
            cached.applied = true;
            let mut warnings = synchronize_applied(self, &cached.client, &plan, &cached.language);
            if !transaction_warnings.is_empty() {
                warnings.push_str(&format!("\n{}", transaction_warnings.join("\n")));
            }
            format!(
                "Applied {title:?}:\n{}{}\nCommand pending: {command_pending}. Commands are never automatic; use separate approved execute_command with action_id {id}.",
                plan.summary(ws),
                warnings
            )
        } else {
            let mut lines = Lines::new(ws);
            format!(
                "Code action {title:?}:\n{}\nValidated text transaction:\n{}\nCommand pending: {command_pending}. action_id: {id}",
                format::workspace_edit(&mut lines, edit),
                plan.summary(ws)
            )
        };
        lock(&self.actions).insert(id.to_owned(), cached);
        Ok(text)
    }
}

fn command_of(action: &Value) -> Option<&Value> {
    match action.get("command")? {
        Value::String(_) => Some(action),
        command if command.is_object() => Some(command),
        _ => None,
    }
}

/// Successful disk commits stay successful if a dead server cannot receive didChange. Report
/// that distinction explicitly and let the next intelligence call refresh its view from disk.
fn synchronize_applied(
    tool: &LspTool,
    client: &Arc<Client>,
    plan: &Plan,
    language: &str,
) -> String {
    let clients: Vec<Arc<Client>> = lock(&tool.running)
        .values()
        .filter(|other| other.root() == client.root() && other.is_alive())
        .cloned()
        .collect();
    let mut errors = Vec::new();
    for path in plan.paths() {
        if plan.removed(path) {
            client.close_document(path);
            for other in &clients {
                other.close_document(path);
            }
            continue;
        }
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("");
        match read_source(path) {
            Ok(text) => {
                if let Err(error) =
                    client.sync_document(path, servers::language_id(extension, language), &text)
                {
                    errors.push(format!("{}: {error:#}", path.display()));
                }
                for other in &clients {
                    if !Arc::ptr_eq(other, client)
                        && let Err(error) = other.sync_if_open(path, &text)
                    {
                        errors.push(format!("{}: {error:#}", path.display()));
                    }
                }
            }
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
    }
    if errors.is_empty() {
        String::new()
    } else {
        format!(
            "\nFiles committed, but language-server synchronization failed: {}",
            errors.join("; ")
        )
    }
}

/// Largest directory `rename_file` will expand into per-file server requests.
const MAX_RENAME_FILES: usize = 1000;

/// Source and destination pairs. A directory expands to its files so each gets its own
/// `willRenameFiles` request; `.git` is moved with the directory but never listed.
fn rename_pairs(ws: &Workspace, from: &Path, to: &Path) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    if !from.exists() {
        return Err(format!("source path does not exist: {}", ws.rel(from)));
    }
    if from == to {
        return Err("source and destination paths are identical".into());
    }
    match std::fs::symlink_metadata(to) {
        Ok(_) => return Err(format!("destination already exists: {}", ws.rel(to))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "cannot inspect destination {}: {error}",
                ws.rel(to)
            ));
        }
    }
    if from.is_file() {
        return Ok(vec![(from.to_path_buf(), to.to_path_buf())]);
    }
    if to.starts_with(from) {
        return Err("destination is inside the source directory".into());
    }
    let mut files = Vec::new();
    collect_files(from, &mut files)?;
    if files.is_empty() {
        return Err("no files to rename".into());
    }
    let mut pairs = Vec::with_capacity(files.len());
    for file in files {
        let relative = file
            .strip_prefix(from)
            .map_err(|error| error.to_string())?
            .to_path_buf();
        pairs.push((file, to.join(relative)));
    }
    Ok(pairs)
}

/// Recursively gathers regular files under `dir`, skipping `.git`, up to `MAX_RENAME_FILES`.
fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if kind.is_dir() {
            if entry.file_name() != ".git" {
                collect_files(&path, files)?;
            }
        } else if kind.is_file() {
            if files.len() == MAX_RENAME_FILES {
                return Err(format!(
                    "too many files to rename (more than {MAX_RENAME_FILES}); rename a smaller directory"
                ));
            }
            files.push(path);
        }
    }
    Ok(())
}

/// Moves each `(source, target)` unit without replacing existing names, including on rollback.
/// A failed move reverses completed ones and reports any reversal that also failed; a recreated
/// source is preserved and the moved original remains at the reported target for recovery.
fn move_units(units: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    let mut done: Vec<&(PathBuf, PathBuf)> = Vec::new();
    for unit in units {
        let (source, target) = unit;
        if let Err(error) = move_one(source, target) {
            let mut stuck = Vec::new();
            for (undo_source, undo_target) in done.iter().rev().map(|unit| (&unit.0, &unit.1)) {
                if let Err(restore) =
                    crate::platform::fs::rename_noreplace(undo_target, undo_source)
                {
                    stuck.push(format!("{}: {restore}", undo_target.display()));
                }
            }
            let restored = if stuck.is_empty() {
                "moved files were restored".to_string()
            } else {
                format!("could not restore: {}", stuck.join("; "))
            };
            return Err(format!(
                "cannot move {} to {}: {error}; {restored}",
                source.display(),
                target.display()
            ));
        }
        done.push(unit);
    }
    Ok(())
}

fn move_one(source: &Path, target: &Path) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::platform::fs::rename_noreplace(source, target)
}

/// Merges every server's `WorkspaceEdit` into one `documentChanges` list, so `Plan::prepare`
/// validates and stages it exactly like a `rename` edit. `Null` means no server edited anything.
fn merge_workspace_edits(values: &[Value]) -> Result<Value, String> {
    let mut document_changes = Vec::new();
    // Annotated edits name their annotation by id; the merged edit must carry those definitions
    // or `Plan::prepare` rejects every annotated edit as unknown.
    let mut annotations = serde_json::Map::new();
    for value in values {
        if value.is_null() {
            continue;
        }
        if let Some(items) = value.get("changeAnnotations").and_then(Value::as_object) {
            for (id, annotation) in items {
                annotations
                    .entry(id.clone())
                    .or_insert_with(|| annotation.clone());
            }
        }
        if let Some(changes) = value.get("changes") {
            let changes = changes
                .as_object()
                .ok_or("WorkspaceEdit.changes must be an object")?;
            for (uri, edits) in changes {
                document_changes
                    .push(json!({"textDocument": {"uri": uri, "version": null}, "edits": edits}));
            }
        }
        if let Some(entries) = value.get("documentChanges") {
            let entries = entries
                .as_array()
                .ok_or("WorkspaceEdit.documentChanges must be an array")?;
            document_changes.extend(entries.iter().cloned());
        }
    }
    if document_changes.is_empty() {
        Ok(Value::Null)
    } else if annotations.is_empty() {
        Ok(json!({"documentChanges": document_changes}))
    } else {
        Ok(json!({"documentChanges": document_changes, "changeAnnotations": annotations}))
    }
}

/// The cursor the model asked for: `line` plus `column` and/or `symbol`.
fn cursor(args: &Value, source: &str) -> Result<text::Located, String> {
    let line = opt_u64(args, "line")?.ok_or("this action needs `line` (1-based)")?;
    locate(
        source,
        line,
        opt_u64(args, "column")?,
        opt_str(args, "symbol")?,
    )
}

impl Tool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }

    fn schema(&self) -> Value {
        let actions: Vec<&str> = ACTIONS.iter().map(|(name, _)| *name).collect();
        tool_schema(
            "lsp",
            DESCRIPTION,
            json!({
                "action": {"type": "string", "enum": actions},
                "path": {"type": "string", "description": "File in the project; selects the language server. Not needed for status."},
                "line": {"type": "integer", "minimum": 1, "description": "1-based line (position actions)."},
                "column": {"type": "integer", "minimum": 1, "description": "1-based character column on `line`."},
                "symbol": {"type": "string", "description": "Text on `line` to put the cursor on instead of `column`."},
                "query": {"type": "string", "description": "workspace_symbols: name; code_actions: exact LSP action kind to filter."},
                "new_name": {"type": "string", "description": "rename/rename_preview: the new name. rename_file: workspace-relative destination path."},
                "apply": {"type": "boolean", "description": "rename_file only: false previews the willRenameFiles edits and move without writing. Defaults to true."},
                "action_id": {"type": "string", "description": "Session-local ID from code_actions; required for code_action_resolve/code_action_apply/execute_command."},
                "end_line": {"type": "integer", "minimum": 1, "description": "code_actions: optional 1-based end line; needs end_column."},
                "end_column": {"type": "integer", "minimum": 1, "description": "code_actions: 1-based end character column."},
            }),
            &["action"],
        )
    }

    /// Startup and every externally effective operation remain explicitly approved.
    fn needs_approval(&self, args: &Value) -> bool {
        match args
            .get("action")
            .and_then(Value::as_str)
            .and_then(|name| Action::parse(name).ok())
        {
            Some(
                Action::Rename
                | Action::RenameFile
                | Action::CodeActionApply
                | Action::ExecuteCommand
                | Action::Reload,
            ) => return true,
            Some(Action::Status | Action::Capabilities | Action::CodeActionResolve) => {
                return false;
            }
            _ => {}
        }
        let Some(file) = args.get("path").and_then(Value::as_str) else {
            return false;
        };
        match self.servers.language_for(file) {
            Ok(language) => !self.is_running(&language),
            Err(_) => false,
        }
    }
    fn may_modify_workspace(&self, args: &Value) -> bool {
        match args
            .get("action")
            .and_then(Value::as_str)
            .and_then(|name| Action::parse(name).ok())
        {
            Some(Action::Rename | Action::CodeActionApply | Action::RenameFile) => false, // every write is observed
            Some(Action::ExecuteCommand | Action::Reload) => true, // project code outside sandbox
            _ => self.needs_approval(args),
        }
    }

    fn manages_workspace_serialization(&self) -> bool {
        true
    }

    fn cancel_operation(&self) {
        if let Some(cancel) = lock(&self.starting).as_ref() {
            cancel.store(true, Ordering::Release);
        }
        let clients: Vec<Arc<Client>> = lock(&self.running).values().cloned().collect();
        for client in clients {
            client.cancel_pending_requests();
        }
    }

    fn fork_session(&self) -> Option<Arc<dyn Tool>> {
        Some(Arc::new_cyclic(|owner| Self {
            servers: self.servers.clone(),
            launch: Arc::clone(&self.launch),
            running: Mutex::new(HashMap::new()),
            starting: Mutex::new(None),
            actions: Mutex::new(HashMap::new()),
            next_action: AtomicU64::new(1),
            session_id: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            operation: Mutex::new(()),
            diagnostics_on_write: self.diagnostics_on_write,
            format_on_write: self.format_on_write,
            start_on_write: self.start_on_write,
            write_owner: self.write_owner.as_ref().map(|_| owner.clone()),
        }))
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.call_streaming(ctx, args, &|_| {}, &AtomicBool::new(false))
    }

    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        _on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        let mut effects = Effects::Known;
        let output = match self.run(ctx, args, cancel, &mut effects) {
            Ok(text) => ToolOutput::ok(text),
            Err(message) => ToolOutput::err(message),
        };
        output.with_effects(effects)
    }
}
