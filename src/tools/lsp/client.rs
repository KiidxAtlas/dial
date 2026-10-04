//! A minimal Language Server Protocol client over a byte stream.
//!
//! A reader thread owns the incoming half: it routes responses to the request that is waiting
//! for them, remembers `publishDiagnostics`, and answers the requests servers send to clients
//! (configuration, progress, capability registration) with sane replies. Everything else is
//! ignored. The outgoing half is shared behind a mutex so frames never interleave.

use super::framing::{read_message, write_message};
use super::text::{path_from_uri, read_source, uri_from_path};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::os::unix::process::CommandExt;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use std::{fmt, io, thread};

#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// The `initialize` handshake; servers may load the project first.
    pub initialize: Duration,
    pub request: Duration,
    /// Grace for `shutdown` and for the process to exit before it is killed.
    pub shutdown: Duration,
    /// How long `diagnostics` waits for the server to publish.
    pub diagnostics: Duration,
    /// After the first publish, how long to keep listening for a follow-up publish.
    pub settle: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            initialize: Duration::from_secs(60),
            request: Duration::from_secs(30),
            shutdown: Duration::from_secs(2),
            diagnostics: Duration::from_secs(10),
            settle: Duration::from_millis(400),
        }
    }
}

/// A JSON-RPC error response from the server.
#[derive(Debug)]
struct RpcError {
    code: i64,
    message: String,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "language server error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

type Reply = Result<Value, RpcError>;

#[derive(Default)]
struct Diagnostics {
    /// Bumped on every publish so waiters can tell "new" from "stale".
    counter: u64,
    by_file: HashMap<PathBuf, (u64, Vec<Value>)>,
}

struct Shared {
    writer: Mutex<Box<dyn Write + Send>>,
    pending: Mutex<HashMap<i64, Sender<Reply>>>,
    diagnostics: Mutex<Diagnostics>,
    published: Condvar,
    closed: AtomicBool,
    root_uri: String,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Shared {
    fn send(&self, message: &Value) -> io::Result<()> {
        write_message(&mut **lock(&self.writer), message)
    }

    fn handle(&self, message: Value) {
        let id = message.get("id").filter(|id| !id.is_null()).cloned();
        match (message.get("method").and_then(Value::as_str), id) {
            (Some(method), Some(id)) => {
                let params = message.get("params").unwrap_or(&Value::Null);
                let reply = match answer_server_request(method, params, &self.root_uri) {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err((code, text)) => {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": text}})
                    }
                };
                let _ = self.send(&reply);
            }
            (Some("textDocument/publishDiagnostics"), None) => self.store_diagnostics(&message["params"]),
            (Some(_), None) => {}
            (None, Some(id)) => {
                let Some(tx) = id.as_i64().and_then(|id| lock(&self.pending).remove(&id)) else { return };
                let reply = match message.get("error") {
                    Some(error) => Err(RpcError {
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: error.get("message").and_then(Value::as_str).unwrap_or("unknown error").to_owned(),
                    }),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(reply);
            }
            (None, None) => {}
        }
    }

    fn store_diagnostics(&self, params: &Value) {
        let Some(path) = params.get("uri").and_then(Value::as_str).and_then(path_from_uri) else { return };
        let items = params.get("diagnostics").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut diagnostics = lock(&self.diagnostics);
        diagnostics.counter += 1;
        let counter = diagnostics.counter;
        diagnostics.by_file.insert(path, (counter, items));
        self.published.notify_all();
    }

    /// The stream ended: fail every waiting request and wake diagnostic waiters.
    fn close(&self) {
        let mut pending = lock(&self.pending);
        self.closed.store(true, Ordering::SeqCst);
        pending.clear();
        drop(pending);
        let _guard = lock(&self.diagnostics);
        self.published.notify_all();
    }
}

/// Reply to a request the server sent us. Servers only need to see that we are alive and have
/// nothing to configure; anything we do not understand gets `MethodNotFound`.
fn answer_server_request(method: &str, params: &Value, root_uri: &str) -> Result<Value, (i64, &'static str)> {
    match method {
        "workspace/configuration" => {
            let count = params.get("items").and_then(Value::as_array).map_or(0, Vec::len);
            Ok(Value::Array(vec![Value::Null; count]))
        }
        "workspace/workspaceFolders" => Ok(json!([{"uri": root_uri, "name": "workspace"}])),
        "window/workDoneProgress/create"
        | "client/registerCapability"
        | "client/unregisterCapability"
        | "window/showMessageRequest"
        | "window/showDocument" => Ok(Value::Null),
        "workspace/applyEdit" => Ok(json!({
            "applied": false,
            "failureReason": "dial never lets language servers write files; changes go through the edit tool",
        })),
        _ => Err((-32601, "method not found")),
    }
}

fn reader_loop(shared: &Shared, input: impl Read) {
    let mut input = BufReader::new(input);
    while let Ok(Some(message)) = read_message(&mut input) {
        shared.handle(message);
    }
    shared.close();
}

struct OpenDoc {
    version: i32,
    hash: u64,
    language_id: String,
}

fn hash_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

pub struct Client {
    name: String,
    shared: std::sync::Arc<Shared>,
    next_id: AtomicI64,
    docs: Mutex<HashMap<PathBuf, OpenDoc>>,
    child: Mutex<Option<Child>>,
    timeouts: Timeouts,
    /// Set once the handshake succeeded; only then does shutdown speak the protocol.
    ready: bool,
}

impl Client {
    /// Start `program` with `args` in `root` and run the handshake.
    pub fn spawn(name: &str, program: &Path, args: &[String], root: &Path, timeouts: Timeouts) -> Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|e| anyhow!("starting {} ({}): {e}", name, program.display()))?;
        let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
        let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
            kill_group(&mut child);
            bail!("{name}: could not attach to the server's stdio");
        };
        Self::connect(name, root, Box::new(stdout), Box::new(stdin), Some(child), timeouts)
    }

    /// Run the handshake over an existing stream pair. Dropping the client tears the server down:
    /// `shutdown`, `exit`, then a kill of `child` (and its process group) if it lingers.
    pub fn connect(
        name: &str,
        root: &Path,
        input: Box<dyn Read + Send>,
        output: Box<dyn Write + Send>,
        child: Option<Child>,
        timeouts: Timeouts,
    ) -> Result<Self> {
        let root_uri = uri_from_path(root);
        let shared = std::sync::Arc::new(Shared {
            writer: Mutex::new(output),
            pending: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(Diagnostics::default()),
            published: Condvar::new(),
            closed: AtomicBool::new(false),
            root_uri: root_uri.clone(),
        });
        let reader = std::sync::Arc::clone(&shared);
        thread::Builder::new().name(format!("dial-lsp-{name}")).spawn(move || reader_loop(&reader, input))?;
        let mut client = Self {
            name: name.to_owned(),
            shared,
            next_id: AtomicI64::new(1),
            docs: Mutex::new(HashMap::new()),
            child: Mutex::new(child),
            timeouts,
            ready: false,
        };
        client.initialize(root, &root_uri)?;
        client.ready = true;
        Ok(client)
    }

    fn initialize(&self, root: &Path, root_uri: &str) -> Result<()> {
        let name = root.file_name().and_then(|n| n.to_str()).unwrap_or("workspace");
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "dial"},
            "rootUri": root_uri,
            "workspaceFolders": [{"uri": root_uri, "name": name}],
            "capabilities": {
                "general": {"positionEncodings": ["utf-16"]},
                "workspace": {"workspaceFolders": true, "configuration": true, "symbol": {}},
                "window": {"workDoneProgress": true},
                "textDocument": {
                    "synchronization": {"didSave": false, "dynamicRegistration": false},
                    "publishDiagnostics": {"relatedInformation": false},
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "definition": {"linkSupport": true},
                    "typeDefinition": {"linkSupport": true},
                    "implementation": {"linkSupport": true},
                    "references": {},
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                    "rename": {"prepareSupport": false},
                },
            },
        });
        self.request_within("initialize", params, self.timeouts.initialize)?;
        self.notify("initialized", json!({}))
    }

    pub fn is_alive(&self) -> bool {
        !self.shared.closed.load(Ordering::SeqCst)
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.shared
            .send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .map_err(|e| anyhow!("{} is not accepting messages: {e}", self.name))
    }

    pub fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_within(method, params, self.timeouts.request)
    }

    fn request_within(&self, method: &str, params: Value, limit: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        {
            let mut pending = lock(&self.shared.pending);
            if !self.is_alive() {
                bail!("{} has exited", self.name);
            }
            pending.insert(id, tx);
        }
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.shared.send(&message) {
            lock(&self.shared.pending).remove(&id);
            bail!("{} is not accepting messages: {e}", self.name);
        }
        match rx.recv_timeout(limit) {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(rpc)) => Err(rpc.into()),
            Err(RecvTimeoutError::Timeout) => {
                lock(&self.shared.pending).remove(&id);
                let _ = self.notify("$/cancelRequest", json!({"id": id}));
                bail!("{} did not answer {method} within {}s", self.name, limit.as_secs().max(1))
            }
            Err(RecvTimeoutError::Disconnected) => bail!("{} exited while handling {method}", self.name),
        }
    }

    /// Make the server's view of `path` match `text`: `didOpen` the first time, a full-text
    /// `didChange` when the content differs from what it last saw. Returns whether anything was sent.
    pub fn sync_document(&self, path: &Path, language_id: &str, text: &str) -> Result<bool> {
        let hash = hash_text(text);
        let uri = uri_from_path(path);
        let mut docs = lock(&self.docs);
        match docs.get_mut(path) {
            None => {
                self.notify(
                    "textDocument/didOpen",
                    json!({"textDocument": {"uri": uri, "languageId": language_id, "version": 1, "text": text}}),
                )?;
                docs.insert(path.to_owned(), OpenDoc { version: 1, hash, language_id: language_id.to_owned() });
            }
            Some(doc) if doc.hash == hash => return Ok(false),
            Some(doc) => {
                self.notify(
                    "textDocument/didChange",
                    json!({"textDocument": {"uri": uri, "version": doc.version + 1}, "contentChanges": [{"text": text}]}),
                )?;
                doc.version += 1;
                doc.hash = hash;
            }
        }
        Ok(true)
    }

    /// Bring every other open document in line with the disk: changed files are re-synced,
    /// deleted or unreadable ones are closed.
    pub fn refresh_open_documents(&self, except: &Path) {
        let open: Vec<(PathBuf, String)> = lock(&self.docs)
            .iter()
            .filter(|(path, _)| path.as_path() != except)
            .map(|(path, doc)| (path.clone(), doc.language_id.clone()))
            .collect();
        for (path, language_id) in open {
            match read_source(&path) {
                Ok(text) => {
                    let _ = self.sync_document(&path, &language_id, &text);
                }
                Err(_) => self.close_document(&path),
            }
        }
    }

    pub fn close_document(&self, path: &Path) {
        if lock(&self.docs).remove(path).is_some() {
            let _ = self.notify("textDocument/didClose", json!({"textDocument": {"uri": uri_from_path(path)}}));
        }
    }

    /// Counter value of the last publish for `path` (0 if none yet).
    pub fn diagnostics_mark(&self, path: &Path) -> u64 {
        lock(&self.shared.diagnostics).by_file.get(path).map_or(0, |(seq, _)| *seq)
    }

    /// Diagnostics for `path`. When `fresh` (the document was just opened or changed) wait for a
    /// publish newer than `mark` and let follow-up publishes settle; otherwise return what the
    /// server last said, waiting only if it has said nothing yet. `None`: nothing arrived in time.
    pub fn diagnostics(&self, path: &Path, mark: u64, fresh: bool) -> Option<Vec<Value>> {
        let seq_of = |d: &Diagnostics| d.by_file.get(path).map_or(0, |(seq, _)| *seq);
        let deadline = Instant::now() + self.timeouts.diagnostics;
        let mut guard = lock(&self.shared.diagnostics);
        let needed = if fresh { mark } else { 0 };
        guard = self.wait_diagnostics(guard, deadline, |d| seq_of(d) > needed);
        // Servers often publish a quick syntax pass and then a fuller one; take the last of a burst.
        loop {
            let seen = seq_of(&guard);
            if seen <= mark {
                break;
            }
            let quiet = Instant::now() + self.timeouts.settle;
            guard = self.wait_diagnostics(guard, quiet.min(deadline), |d| seq_of(d) != seen);
            if seq_of(&guard) == seen {
                break;
            }
        }
        let (seq, items) = guard.by_file.get(path)?;
        (*seq > 0 && (!fresh || *seq > mark)).then(|| items.clone())
    }

    /// Block until `done` holds, the server exits, or `until` passes.
    fn wait_diagnostics<'a>(
        &self,
        mut guard: MutexGuard<'a, Diagnostics>,
        until: Instant,
        done: impl Fn(&Diagnostics) -> bool,
    ) -> MutexGuard<'a, Diagnostics> {
        while !done(&guard) && self.is_alive() {
            let Some(left) = until.checked_duration_since(Instant::now()) else { break };
            guard = self.shared.published.wait_timeout(guard, left).unwrap_or_else(|p| p.into_inner()).0;
        }
        guard
    }
}

fn kill_group(child: &mut Child) {
    // The leader is not reaped until after killpg, so its PID cannot be reused.
    unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.is_alive() {
            if self.ready {
                let _ = self.request_within("shutdown", Value::Null, self.timeouts.shutdown);
            }
            let _ = self.notify("exit", Value::Null);
        }
        let Some(mut child) = lock(&self.child).take() else { return };
        let deadline = Instant::now() + self.timeouts.shutdown;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        kill_group(&mut child);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_requests_get_empty_but_well_formed_replies() {
        let root = "file:///work";
        let config = answer_server_request("workspace/configuration", &json!({"items": [{}, {}, {}]}), root);
        assert_eq!(config.unwrap(), json!([null, null, null]));
        let folders = answer_server_request("workspace/workspaceFolders", &Value::Null, root).unwrap();
        assert_eq!(folders[0]["uri"], root);
        for method in ["window/workDoneProgress/create", "client/registerCapability", "client/unregisterCapability"] {
            assert_eq!(answer_server_request(method, &Value::Null, root).unwrap(), Value::Null, "{method}");
        }
        let apply = answer_server_request("workspace/applyEdit", &Value::Null, root).unwrap();
        assert_eq!(apply["applied"], false);
        assert_eq!(answer_server_request("custom/thing", &Value::Null, root).unwrap_err().0, -32601);
    }
}
