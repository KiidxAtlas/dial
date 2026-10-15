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

mod client;
mod config;
mod format;
mod framing;
mod servers;
#[cfg(all(test, unix))]
mod tests;
mod text;
mod workspace_edit;

use super::ext::{Tool, ToolContext, ToolEvent};
use super::{ToolOutput, Workspace, io_message, opt_bool, opt_str, opt_u64, req_str, tool_schema};
use crate::config::types::Config;
use client::{Client, ServerOptions, Timeouts};
pub use config::LspConfig;
use format::Lines;
use serde_json::{Value, json};
use servers::{Resolved, Servers};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};
pub(crate) use text::path_from_uri;
use text::{locate, read_source, uri_from_path};
use workspace_edit::{Plan, Snapshot};

static WRITE_THROUGH: LazyLock<Mutex<HashMap<PathBuf, Weak<LspTool>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// The `lsp` tool, ready to register.
pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    let tool = Arc::new(LspTool::new(&config.lsp));
    let mut map = lock(&WRITE_THROUGH);
    map.retain(|_, weak| weak.strong_count() > 0);
    map.insert(config.project_root.clone(), Arc::downgrade(&tool));
    vec![tool]
}
/// Synchronizes committed files with already-running language servers and returns the notes to
/// append to the edit result: watched-file notices, format-on-write status, and diagnostics.
/// `kind` is the LSP `FileChangeType`: 1 created, 2 changed, 3 deleted. The write path starts project
/// code only when `[lsp] start_on_write` opts in, and never holds a workspace lock while a server answers.
pub fn writethrough(ws: &Workspace, paths: &[(PathBuf, u8)]) -> String {
    let tool = lock(&WRITE_THROUGH).get(ws.root()).and_then(Weak::upgrade);
    let Some(tool) = tool else {
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

    if tool.format_on_write {
        for (path, kind) in paths {
            if *kind == 3 {
                continue;
            }
            if let Err(note) = format_on_write(&tool, ws, &clients, path) {
                let _ = writeln!(output, "[LSP format {}] {note}", ws.rel(path));
            }
        }
    }
    if !tool.diagnostics_on_write {
        return output;
    }
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
            let mark = client.diagnostics_mark(path);
            // A document the server has not seen is opened here; `changed` is false when it already
            // holds this exact text, so the answer is the current publish rather than a new one.
            let Ok(changed) = client.sync_document(path, &language, &source) else {
                continue;
            };
            if client.did_save(path, &source).is_err() {
                continue;
            }
            let Some(items) = client.diagnostics(path, mark, changed) else {
                continue;
            };
            for item in items {
                let message = item
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("diagnostic");
                let line = item
                    .pointer("/range/start/line")
                    .and_then(Value::as_u64)
                    .map_or(1, |n| n.saturating_add(1));
                let column = item
                    .pointer("/range/start/character")
                    .and_then(Value::as_u64)
                    .map_or(1, |n| n.saturating_add(1));
                let _ = writeln!(
                    output,
                    "[LSP {}:{}:{}] {message}",
                    ws.rel(path),
                    line,
                    column
                );
            }
        }
    }
    output
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
        match (tool.launch)(&resolved, ws.root()) {
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
        let mut notes = plan.commit(ws, &snapshot)?.join("\n");
        notes.push_str(&synchronize_applied(tool, client, &plan, client.name()));
        return Ok(notes);
    }
    Ok(String::new())
}

const DESCRIPTION: &str = "Code intelligence and refactoring from real language servers; startup, edits and commands need approval. \
`path` selects the server; position actions need 1-based `line` plus `column` or `symbol`. \
`rename_preview` previews `new_name`; `rename` applies every validated server-provided text edit \
as one observer/checkpoint-backed transaction. Resource create/rename/delete operations are refused. \
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
type Launcher = Arc<dyn Fn(&Resolved, &Path) -> anyhow::Result<Client> + Send + Sync>;

pub struct LspTool {
    servers: Servers,
    launch: Launcher,
    /// One live client per language, started on first use.
    running: Mutex<HashMap<String, Arc<Client>>>,
    actions: Mutex<HashMap<String, CachedAction>>,
    next_action: AtomicU64,
    session_id: u64,
    operation: Mutex<()>,
    diagnostics_on_write: bool,
    format_on_write: bool,
    start_on_write: bool,
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

impl LspTool {
    pub fn new(config: &LspConfig) -> Self {
        let mut tool = Self::with_launcher(
            config,
            Box::new(|server, root| {
                Client::spawn(
                    &server.language,
                    &server.program,
                    &server.args,
                    root,
                    Timeouts::default(),
                    ServerOptions {
                        settings: server.settings.clone(),
                        init_options: server.init_options.clone(),
                    },
                )
            }),
        );
        tool.diagnostics_on_write = config.diagnostics_on_write;
        tool.format_on_write = config.format_on_write;
        tool
    }

    fn with_launcher(
        config: &LspConfig,
        launch: Box<dyn Fn(&Resolved, &Path) -> anyhow::Result<Client> + Send + Sync>,
    ) -> Self {
        Self {
            servers: Servers::new(config),
            launch: Arc::from(launch),
            running: Mutex::new(HashMap::new()),
            actions: Mutex::new(HashMap::new()),
            next_action: AtomicU64::new(1),
            session_id: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            operation: Mutex::new(()),
            diagnostics_on_write: config.diagnostics_on_write,
            format_on_write: config.format_on_write,
            start_on_write: config.start_on_write,
        }
    }
    fn is_running(&self, language: &str) -> bool {
        lock(&self.running)
            .get(language)
            .is_some_and(|client| client.is_alive())
    }

    /// The live client for `server`'s language, starting (or restarting a dead) one if needed.
    fn client(&self, ctx: &ToolContext, server: &Resolved) -> Result<Arc<Client>, String> {
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
        let client = (self.launch)(server, ws.root()).map_err(|e| {
            format!(
                "could not start the {} language server ({}): {e:#}",
                server.language,
                server.program.display()
            )
        })?;
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
        let pairs = rename_pairs(ws, &from, &to)?;

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
            let client = self.client(ctx, &server)?;
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
        let snapshot = Snapshot::capture(ws)?;
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
        let mut notes = plan.commit(ws, &snapshot)?.join("\n");
        if let Some((_, client, _)) = started.first() {
            notes.push_str(&synchronize_applied(self, client, &plan, &language));
        }

        let units: Vec<(PathBuf, PathBuf)> = if from.is_dir() {
            vec![(from.clone(), to.clone())]
        } else {
            pairs.clone()
        };
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

    fn run(&self, ctx: &ToolContext, args: &Value, cancel: &AtomicBool) -> Result<String, String> {
        let ws = ctx.workspace;
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

        let selected = self
            .servers
            .select_all(ws.root(), &abs, &servers::path_dirs())?;
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
            return self.run_cached(ctx, args, &abs, language, action, cancel);
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
        let client = self.client(ctx, &server)?;
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
            })).map_err(|e| format!("command may have executed before failure: {e:#}; external effects are not transactional"))?;
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
                    if !Arc::ptr_eq(other, client) {
                        if let Err(error) = other.sync_if_open(path, &text) {
                            errors.push(format!("{}: {error:#}", path.display()));
                        }
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
    if to.exists() {
        return Err(format!("destination already exists: {}", ws.rel(to)));
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

/// Moves each `(source, target)` unit in order. A failed move reverses the completed ones and
/// reports any reversal that also failed, so callers know exactly what remains on disk.
fn move_units(units: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    let mut done: Vec<&(PathBuf, PathBuf)> = Vec::new();
    for unit in units {
        let (source, target) = unit;
        if let Err(error) = move_one(source, target) {
            let mut stuck = Vec::new();
            for (undo_source, undo_target) in done.iter().rev().map(|unit| (&unit.0, &unit.1)) {
                if let Err(restore) = std::fs::rename(undo_target, undo_source) {
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
    std::fs::rename(source, target)
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
        let clients: Vec<Arc<Client>> = lock(&self.running).values().cloned().collect();
        for client in clients {
            client.cancel_pending_requests();
        }
    }

    fn fork_session(&self) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self {
            servers: self.servers.clone(),
            launch: Arc::clone(&self.launch),
            running: Mutex::new(HashMap::new()),
            actions: Mutex::new(HashMap::new()),
            next_action: AtomicU64::new(1),
            session_id: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            operation: Mutex::new(()),
            diagnostics_on_write: self.diagnostics_on_write,
            format_on_write: self.format_on_write,
            start_on_write: self.start_on_write,
        }))
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        match self.run(ctx, args, &AtomicBool::new(false)) {
            Ok(text) => ToolOutput::ok(text),
            Err(message) => ToolOutput::err(message),
        }
    }

    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        _on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        match self.run(ctx, args, cancel) {
            Ok(text) => ToolOutput::ok(text),
            Err(message) => ToolOutput::err(message),
        }
    }
}
