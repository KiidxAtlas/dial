//! `ida`: IDA Pro databases, each opened in its own idalib worker process.
//!
//! idalib allows one open database per process, so every database gets a dedicated Python worker
//! (`worker.py`). The worker keeps the database and its `exec` namespace alive between calls.

#[cfg(test)]
mod tests;
mod worker;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::tools::ida::IdaConfig;
use crate::config::types::Config;
use crate::tools::integrations::ext::{Concurrency, Tool, ToolContext, ToolEvent};
use crate::tools::{
    ToolOutput, Workspace, opt_bool, opt_str, opt_u64, present, req_str, tool_schema,
};

use worker::{Worker, lock};

const DESCRIPTION: &str = "IDA Pro reverse engineering through idalib. Actions:\n\
- list: open databases.\n\
- open: db=<binary path>. Runs auto-analysis; large binaries take minutes. Returns a db id.\n\
- save / close: write the database; close also releases it (save defaults to true).\n\
- exec: run Python code in the database's persistent namespace (db, ida_domain and the ida_* modules are preloaded). The last expression's value is shown as `=> value`. timeout is in seconds.\n\
- rename, comment, set_type, make_function: target is an address (0x401000) or a symbol name.\n\
`db` is a db id (db1) or a binary path; it may be omitted when exactly one database is open. \
Requires IDA Pro with idalib on this machine (settings ida.python and ida.idadir).";

const ACTIONS: &[&str] = &[
    "list",
    "open",
    "save",
    "close",
    "exec",
    "rename",
    "comment",
    "set_type",
    "make_function",
];
/// Ceiling for open and close, which may run auto-analysis or write a large database.
const LONG_TIMEOUT: Duration = Duration::from_secs(1800);
const MAX_EXEC_TIMEOUT_SECS: u64 = 3600;
const MAX_INLINE_BYTES: usize = 32 * 1024;

pub struct Ida {
    config: IdaConfig,
    store: Mutex<Store>,
}

#[derive(Default)]
struct Store {
    next: u64,
    dbs: BTreeMap<String, Arc<Database>>,
}

struct Database {
    id: String,
    source: PathBuf,
    info: Info,
    worker: Mutex<Worker>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Info {
    idb: String,
    module: String,
    format: String,
    arch: String,
    bitness: u64,
}

/// Rendered action result; `failed` maps to a tool error.
struct Outcome {
    text: String,
    failed: bool,
}

impl Outcome {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            failed: false,
        }
    }
}

pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Ida::new(config.ida.clone()))]
}

impl Ida {
    fn new(config: IdaConfig) -> Self {
        Self {
            config,
            store: Mutex::new(Store::default()),
        }
    }

    fn python(&self) -> &str {
        if self.config.python.is_empty() {
            "python3"
        } else {
            &self.config.python
        }
    }

    fn edit_timeout(&self) -> Duration {
        Duration::from_secs(self.config.timeout_secs)
    }

    fn databases(&self) -> Vec<Arc<Database>> {
        lock(&self.store).dbs.values().cloned().collect()
    }

    fn list(&self) -> Outcome {
        let dbs = self.databases();
        if dbs.is_empty() {
            return Outcome::text("No IDA databases open.");
        }
        let lines: Vec<String> = dbs
            .iter()
            .map(|db| {
                format!(
                    "{}  {}  {}  {}/{}  {}",
                    db.id,
                    db.info.module,
                    db.info.format,
                    db.info.arch,
                    db.info.bitness,
                    db.info.idb
                )
            })
            .collect();
        Outcome::text(lines.join("\n"))
    }

    fn open(
        &self,
        ctx: &ToolContext,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<Outcome, String> {
        let raw = req_str(args, "db")?;
        let source = resolve_source(ctx.workspace.root(), raw)?;
        if let Some(existing) = self.databases().into_iter().find(|db| db.source == source) {
            return Err(format!("{raw} is already open as {}", existing.id));
        }
        ctx.authorize_mutation("ida", std::slice::from_ref(&source), true)?;
        let mut worker = Worker::start(self.python(), &self.config.idadir, cancel)?;
        let call = worker.call(
            "open",
            json!({"path": source.to_string_lossy()}),
            LONG_TIMEOUT,
            cancel,
        )?;
        if let Some(reason) = call.stopped {
            return Err(format!("open {reason}"));
        }
        let info: Info = serde_json::from_value(call.result)
            .map_err(|error| format!("IDA worker returned a malformed open reply: {error}"))?;
        let mut store = lock(&self.store);
        store.next += 1;
        let id = format!("db{}", store.next);
        let text = format!(
            "Opened {id} ({}, {}, {} {}-bit) → {}",
            info.module, info.format, info.arch, info.bitness, info.idb
        );
        store.dbs.insert(
            id.clone(),
            Arc::new(Database {
                id,
                source,
                info,
                worker: Mutex::new(worker),
            }),
        );
        Ok(Outcome::text(text))
    }

    fn close(
        &self,
        ctx: &ToolContext,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<Outcome, String> {
        let db = self.select(ctx.workspace.root(), args)?;
        let save = if present(args, "save") {
            opt_bool(args, "save")?
        } else {
            true
        };
        if save {
            ctx.authorize_mutation("ida", std::slice::from_ref(&db.source), false)?;
        }
        lock(&self.store).dbs.remove(&db.id);
        let mut worker = lock(&db.worker);
        let outcome = worker.call("close", json!({"save": save}), LONG_TIMEOUT, cancel);
        if outcome.is_ok() {
            worker.shutdown();
        } else {
            worker.kill();
        }
        let verb = if save {
            "saved"
        } else {
            "discarded unsaved changes"
        };
        outcome.map(|_| Outcome::text(format!("Closed {} ({verb})", db.id)))
    }

    fn edit(
        &self,
        ctx: &ToolContext,
        action: &str,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<Outcome, String> {
        let db = self.select(ctx.workspace.root(), args)?;
        let timeout = self.edit_timeout();
        let (method, params, timeout, unbounded) = match action {
            "save" => ("save", json!({}), timeout, false),
            "exec" => {
                let code = req_str(args, "code")?;
                let timeout = match opt_u64(args, "timeout")? {
                    Some(secs) if (1..=MAX_EXEC_TIMEOUT_SECS).contains(&secs) => {
                        Duration::from_secs(secs)
                    }
                    Some(_) => {
                        return Err(format!(
                            "timeout must be 1..={MAX_EXEC_TIMEOUT_SECS} seconds"
                        ));
                    }
                    None => timeout,
                };
                ("exec", json!({"code": code}), timeout, true)
            }
            "rename" => {
                let params =
                    json!({"target": req_str(args, "target")?, "name": req_str(args, "name")?});
                ("rename", params, timeout, false)
            }
            "comment" => {
                let params = json!({
                    "target": req_str(args, "target")?,
                    "text": req_str(args, "text")?,
                    "repeatable": opt_bool(args, "repeatable")?,
                });
                ("comment", params, timeout, false)
            }
            "set_type" => {
                let params =
                    json!({"target": req_str(args, "target")?, "decl": req_str(args, "decl")?});
                ("set_type", params, timeout, false)
            }
            "make_function" => (
                "make_function",
                json!({"target": req_str(args, "target")?}),
                timeout,
                false,
            ),
            other => return Err(format!("unknown ida action {other:?}")),
        };
        ctx.authorize_mutation("ida", std::slice::from_ref(&db.source), unbounded)?;
        let mut worker = lock(&db.worker);
        let outcome = worker.call(method, params, timeout, cancel);
        let alive = worker.is_alive();
        drop(worker);
        if !alive {
            lock(&self.store).dbs.remove(&db.id);
        }
        render(ctx.workspace, action, args, outcome?)
    }

    /// The database named by `db` (id or path), or the only open one when `db` is omitted.
    fn select(&self, root: &Path, args: &Value) -> Result<Arc<Database>, String> {
        let dbs = self.databases();
        if let Some(raw) = opt_str(args, "db")? {
            if let Some(db) = dbs.iter().find(|db| db.id == raw) {
                return Ok(Arc::clone(db));
            }
            let source = resolve_source(root, raw)
                .map_err(|_| format!("unknown db {raw:?}; open databases: {}", ids(&dbs)))?;
            if let Some(db) = dbs.iter().find(|db| db.source == source) {
                return Ok(Arc::clone(db));
            }
            return Err(format!("{raw} is not open; open databases: {}", ids(&dbs)));
        }
        match dbs.as_slice() {
            [] => Err(
                "no IDA database is open; run action open with db=<binary path> first".to_owned(),
            ),
            [only] => Ok(Arc::clone(only)),
            _ => Err(format!(
                "multiple IDA databases are open ({}); pass db",
                ids(&dbs)
            )),
        }
    }
}

fn ids(dbs: &[Arc<Database>]) -> String {
    if dbs.is_empty() {
        return "none".to_owned();
    }
    dbs.iter()
        .map(|db| db.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolve a binary path against the workspace root and require it to be an existing file.
fn resolve_source(root: &Path, raw: &str) -> Result<PathBuf, String> {
    let path = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        root.join(raw)
    };
    match path.canonicalize() {
        Ok(canonical) if canonical.is_file() => Ok(canonical),
        _ => Err(format!("db not found: {raw}")),
    }
}

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn render(
    ws: &Workspace,
    action: &str,
    args: &Value,
    call: worker::Call,
) -> Result<Outcome, String> {
    let result = &call.result;
    let outcome = match action {
        "save" => Outcome::text(format!("Saved {}", field(result, "idb"))),
        "exec" => exec_outcome(ws, result, call.stopped.as_deref()),
        "rename" => {
            let old = field(result, "old");
            let old = if old.is_empty() { "(unnamed)" } else { old };
            Outcome::text(format!(
                "Renamed {} {old} → {}",
                field(result, "ea"),
                field(result, "new")
            ))
        }
        "comment" => {
            let repeatable = opt_bool(args, "repeatable")?;
            let kind = if repeatable {
                "repeatable comment"
            } else {
                "comment"
            };
            Outcome::text(format!("Set {kind} at {}", field(result, "ea")))
        }
        "set_type" => Outcome::text(format!(
            "Typed {} as {}",
            field(result, "ea"),
            field(result, "type")
        )),
        "make_function" => Outcome::text(format!(
            "Created function {} at {}",
            field(result, "name"),
            field(result, "ea")
        )),
        other => return Err(format!("unknown ida action {other:?}")),
    };
    Ok(outcome)
}

fn exec_outcome(ws: &Workspace, result: &Value, stopped: Option<&str>) -> Outcome {
    let mut parts = Vec::new();
    let output = field(result, "output").trim_end();
    if !output.is_empty() {
        parts.push(output.to_owned());
    }
    if let Some(value) = result.get("value").and_then(Value::as_str) {
        parts.push(format!("=> {value}"));
    }
    let error = result.get("error").and_then(Value::as_str);
    if let Some(error) = error {
        parts.push(error.trim_end().to_owned());
    }
    if let Some(reason) = stopped {
        parts.push(format!("exec {reason}"));
    }
    let text = if parts.is_empty() {
        "(no output)".to_owned()
    } else {
        parts.join("\n")
    };
    Outcome {
        text: inline(ws, text),
        failed: error.is_some() || stopped.is_some(),
    }
}

/// Keep inline text bounded; the full text is retained as an artifact when the workspace has a store.
fn inline(ws: &Workspace, text: String) -> String {
    if text.len() <= MAX_INLINE_BYTES {
        return text;
    }
    let mut end = MAX_INLINE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_owned();
    out.push_str("\n…[output truncated");
    if let Some(store) = ws.artifacts()
        && let Ok(uri) = store.retain(&text)
    {
        out.push_str(&format!("; full text: {uri}"));
    }
    out.push(']');
    out
}

fn action_of(args: &Value) -> Option<&str> {
    args.get("action").and_then(Value::as_str)
}

fn schema() -> Value {
    let properties = json!({
        "action": {"type": "string", "enum": ACTIONS, "description": "Operation to run"},
        "db": {"type": "string", "description": "Database id (db1) or binary path; required for open"},
        "target": {"type": "string", "description": "Address (0x401000) or symbol name"},
        "name": {"type": "string", "description": "New symbol name (rename)"},
        "text": {"type": "string", "description": "Comment text (comment)"},
        "repeatable": {"type": "boolean", "description": "Repeatable comment (comment)"},
        "decl": {"type": "string", "description": "C declaration (set_type)"},
        "code": {"type": "string", "description": "Python source (exec)"},
        "save": {"type": "boolean", "description": "Save before closing (close; default true)"},
        "timeout": {"type": "number", "description": "exec deadline in seconds, 1..3600"}
    });
    tool_schema("ida", DESCRIPTION, properties, &["action"])
}

impl Tool for Ida {
    fn name(&self) -> &str {
        "ida"
    }

    fn schema(&self) -> Value {
        schema()
    }

    fn needs_approval(&self, args: &Value) -> bool {
        action_of(args) != Some("list")
    }

    /// Every non-list action can write databases (`.i64`/`.idb`) next to the binary.
    fn may_modify_workspace(&self, args: &Value) -> bool {
        action_of(args) != Some("list")
    }

    fn concurrency(&self, args: &Value) -> Concurrency {
        if action_of(args) == Some("list") {
            Concurrency::Shared
        } else {
            Concurrency::Exclusive
        }
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
        let action = match req_str(args, "action") {
            Ok(action) => action,
            Err(error) => return ToolOutput::err(error),
        };
        let result = match action {
            "list" => Ok(self.list()),
            "open" => self.open(ctx, args, cancel),
            "close" => self.close(ctx, args, cancel),
            "save" | "exec" | "rename" | "comment" | "set_type" | "make_function" => {
                self.edit(ctx, action, args, cancel)
            }
            other => Err(format!("unknown ida action {other:?}")),
        };
        match result {
            Ok(outcome) if outcome.failed => ToolOutput::err(outcome.text),
            Ok(outcome) => ToolOutput::ok(outcome.text),
            Err(error) => ToolOutput::err(error),
        }
    }
}
