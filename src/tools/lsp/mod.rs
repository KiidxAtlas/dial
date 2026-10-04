//! The `lsp` tool: code intelligence from real language servers (rust-analyzer, gopls, clangd,
//! typescript-language-server, pyright, ...) spoken to over stdio.
//!
//! Servers start lazily, on the first call for a language, and live for the session. Which
//! server handles a file comes from its extension: built-in defaults found on `PATH`, extended
//! or overridden by `[lsp.<language>]` in the config (see [`servers`]).
//!
//! **Approval.** A call that would start a server needs approval; once the server runs, calls
//! are read-only and do not. Language servers execute project code on startup (build scripts,
//! proc-macros, `cargo check`) and that code is not under dial's sandbox, so it is the operator
//! who decides, once per language, whether to allow it.
//!
//! Positions are 1-based `line` plus `column` (characters) or `symbol` (text on that line).
//! Results are bounded and never applied: `rename_preview` lists the edits for the `edit` tool.

mod client;
mod format;
mod framing;
mod servers;
mod text;
#[cfg(test)]
mod tests;

use super::ext::{Tool, ToolContext};
use super::{ToolOutput, Workspace, io_message, opt_str, opt_u64, req_str, tool_schema};
use crate::config::types::{Config, LspServerConfig};
use client::{Client, Timeouts};
use format::Lines;
use serde_json::{Value, json};
use servers::{Resolved, Servers, extension_of};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use text::{locate, read_source, uri_from_path};

/// The `lsp` tool, ready to register.
pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(LspTool::new(&config.lsp))]
}

const DESCRIPTION: &str = "Code intelligence from the project's language server (types, definitions, references, \
symbols, compiler diagnostics). Every action takes `path` (a file in the project; it picks the server by extension). \
Position actions (definition, references, hover, implementation, type_definition, rename_preview) also need `line` \
(1-based) and either `column` (1-based character) or `symbol` (text on that line, easiest). `workspace_symbols` \
searches the whole project by `query`; `diagnostics` reports the errors and warnings of `path`; `document_symbols` \
outlines it; `rename_preview` (needs `new_name`) lists the edits a rename would make WITHOUT applying them - apply \
them with `edit`. Results look like `path:line:col  text`. The first call for a language starts its server, which runs \
project code (build scripts, proc-macros) outside dial's sandbox, so that first call needs approval; later calls do not.";

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
}

const ACTIONS: [(&str, Action); 9] = [
    ("definition", Action::Definition),
    ("references", Action::References),
    ("hover", Action::Hover),
    ("implementation", Action::Implementation),
    ("type_definition", Action::TypeDefinition),
    ("document_symbols", Action::DocumentSymbols),
    ("workspace_symbols", Action::WorkspaceSymbols),
    ("diagnostics", Action::Diagnostics),
    ("rename_preview", Action::RenamePreview),
];

impl Action {
    fn parse(name: &str) -> Result<Self, String> {
        ACTIONS.iter().find(|(n, _)| *n == name).map(|(_, action)| *action).ok_or_else(|| {
            let names: Vec<&str> = ACTIONS.iter().map(|(n, _)| *n).collect();
            format!("unknown action {name:?}; use one of: {}", names.join(", "))
        })
    }

    fn needs_position(self) -> bool {
        matches!(
            self,
            Self::Definition | Self::References | Self::Hover | Self::Implementation | Self::TypeDefinition | Self::RenamePreview
        )
    }
}

/// Starts a language server for `Resolved` in the given workspace root.
type Launcher = Box<dyn Fn(&Resolved, &Path) -> anyhow::Result<Client> + Send + Sync>;

pub struct LspTool {
    servers: Servers,
    launch: Launcher,
    /// One live client per language, started on first use.
    running: Mutex<HashMap<String, Arc<Client>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl LspTool {
    pub fn new(config: &BTreeMap<String, LspServerConfig>) -> Self {
        Self::with_launcher(
            config,
            Box::new(|server, root| {
                Client::spawn(&server.language, &server.program, &server.args, root, Timeouts::default())
            }),
        )
    }

    fn with_launcher(config: &BTreeMap<String, LspServerConfig>, launch: Launcher) -> Self {
        Self { servers: Servers::new(config), launch, running: Mutex::new(HashMap::new()) }
    }

    fn is_running(&self, language: &str) -> bool {
        lock(&self.running).get(language).is_some_and(|client| client.is_alive())
    }

    /// The live client for `server`'s language, starting (or restarting a dead) one if needed.
    fn client(&self, ws: &Workspace, server: &Resolved) -> Result<Arc<Client>, String> {
        let mut running = lock(&self.running);
        if let Some(client) = running.get(&server.language) {
            if client.is_alive() {
                return Ok(Arc::clone(client));
            }
            running.remove(&server.language);
        }
        let client = (self.launch)(server, ws.root())
            .map_err(|e| format!("could not start the {} language server ({}): {e:#}", server.language, server.program.display()))?;
        let client = Arc::new(client);
        running.insert(server.language.clone(), Arc::clone(&client));
        Ok(client)
    }

    fn run(&self, ws: &Workspace, args: &Value) -> Result<String, String> {
        let action = Action::parse(req_str(args, "action")?)?;
        let file = req_str(args, "path")?;
        let abs = ws.resolve(file)?;
        if !abs.is_file() {
            return Err(format!("{file:?} is not a file in the workspace"));
        }
        let source = read_source(&abs).map_err(|e| io_message("read", file, &e))?;
        // Resolve the position first so a bad request never starts a server.
        let cursor = if action.needs_position() { Some(cursor(args, &source)?) } else { None };
        let query = match action {
            Action::WorkspaceSymbols => Some(opt_str(args, "query")?.filter(|q| !q.is_empty()).ok_or("workspace_symbols needs a non-empty `query`")?),
            _ => None,
        };
        let new_name = match action {
            Action::RenamePreview => Some(opt_str(args, "new_name")?.filter(|n| !n.is_empty()).ok_or("rename_preview needs `new_name`")?),
            _ => None,
        };

        let server = self.servers.select(file, &servers::path_dirs())?;
        let client = self.client(ws, &server)?;
        let io = |e: anyhow::Error| format!("{e:#}");
        let extension = extension_of(file).unwrap_or_default();
        let mark = client.diagnostics_mark(&abs);
        client.refresh_open_documents(&abs);
        let fresh = client
            .sync_document(&abs, servers::language_id(&extension, &server.language), &source)
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
                let result = client.request("textDocument/references", params).map_err(io)?;
                format::locations(&mut lines, &result, "reference")
            }
            Action::Hover => format::hover(&client.request("textDocument/hover", at.unwrap_or_default()).map_err(io)?),
            Action::DocumentSymbols => {
                let result = client.request("textDocument/documentSymbol", json!({"textDocument": document})).map_err(io)?;
                format::document_symbols(&mut lines, &abs, &result)
            }
            Action::WorkspaceSymbols => {
                let result = client.request("workspace/symbol", json!({"query": query})).map_err(io)?;
                format::workspace_symbols(&mut lines, &result)
            }
            Action::Diagnostics => match client.diagnostics(&abs, mark, fresh) {
                Some(items) => format::diagnostics(&mut lines, &abs, &items),
                None => format!(
                    "the {} language server published no diagnostics for {file} in time (it may still be indexing); try again shortly",
                    server.language
                ),
            },
            Action::RenamePreview => {
                let mut params = at.unwrap_or_default();
                params["newName"] = json!(new_name);
                let result = client.request("textDocument/rename", params).map_err(io)?;
                format::workspace_edit(&mut lines, &result)
            }
        };
        Ok(text)
    }
}

/// The cursor the model asked for: `line` plus `column` and/or `symbol`.
fn cursor(args: &Value, source: &str) -> Result<text::Located, String> {
    let line = opt_u64(args, "line")?.ok_or("this action needs `line` (1-based)")?;
    locate(source, line, opt_u64(args, "column")?, opt_str(args, "symbol")?)
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
                "path": {"type": "string", "description": "File in the project; selects the language server."},
                "line": {"type": "integer", "minimum": 1, "description": "1-based line (position actions)."},
                "column": {"type": "integer", "minimum": 1, "description": "1-based character column on `line`."},
                "symbol": {"type": "string", "description": "Text on `line` to put the cursor on instead of `column`."},
                "query": {"type": "string", "description": "workspace_symbols: name to search for."},
                "new_name": {"type": "string", "description": "rename_preview: the new name."},
            }),
            &["action", "path"],
        )
    }

    /// True exactly when the call would start a language server that is not running yet.
    fn needs_approval(&self, args: &Value) -> bool {
        let Some(file) = args.get("path").and_then(Value::as_str) else { return false };
        match self.servers.select(file, &servers::path_dirs()) {
            Ok(server) => !self.is_running(&server.language),
            Err(_) => false,
        }
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        match self.run(ctx.workspace, args) {
            Ok(text) => ToolOutput::ok(text),
            Err(message) => ToolOutput::err(message),
        }
    }
}
