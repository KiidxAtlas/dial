//! A minimal Language Server Protocol client over a byte stream.
//!
//! A reader thread owns the incoming half: it routes responses to the request that is waiting
//! for them, remembers `publishDiagnostics`, tracks `$/progress` (indexing) and answers the
//! requests servers send to clients (configuration from the server's settings, progress,
//! capability registration, diagnostic refresh). A dedicated writer thread owns the outgoing
//! half: callers hand it framed bytes and wait for its acknowledgement with a timeout, so a
//! server that stops reading its stdin can never wedge a caller or hold a lock across a blocking
//! write. Open documents are bounded; the least recently used one is closed past the limit.

use super::framing::{read_message, write_message};
use super::text::{path_from_uri, read_source, uri_from_path};
use anyhow::{Result, anyhow, bail};
use globset::GlobBuilder;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use std::{fmt, io, thread};

/// How long a caller waits for the writer thread to hand a frame to the server.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Open documents kept synchronized per server; older ones get `didClose`.
pub const MAX_OPEN_DOCUMENTS: usize = 64;
/// `ContentModified`: the document changed while the server computed; retry.
const CONTENT_MODIFIED: i64 = -32801;
/// `ServerCancelled`: the server gave up and asks to be asked again.
const SERVER_CANCELLED: i64 = -32802;
const RETRIES: u32 = 3;

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

/// Per-server configuration sent during and after the handshake.
#[derive(Debug, Clone, Default)]
pub struct ServerOptions {
    /// `workspace/configuration` answers and the initial `didChangeConfiguration`.
    pub settings: Value,
    /// `initializationOptions`.
    pub init_options: Value,
}

/// A JSON-RPC error response from the server.
#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
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
    by_file: HashMap<PathBuf, (u64, Option<i64>, Vec<Value>)>,
    /// Pull-model result ids for `previousResultId`, with the items each id stands for.
    result_ids: HashMap<PathBuf, (String, Vec<Value>)>,
}

struct Outgoing {
    frame: Vec<u8>,
    ack: Option<Sender<io::Result<()>>>,
}

struct Shared {
    outgoing: Mutex<Option<Sender<Outgoing>>>,
    pending: Mutex<HashMap<i64, Sender<Reply>>>,
    diagnostics: Mutex<Diagnostics>,
    registrations: Mutex<HashMap<String, Value>>,
    published: Condvar,
    closed: AtomicBool,
    root_uri: String,
    settings: Value,
    /// Active `$/progress` tokens: the server is indexing or checking.
    progress: Mutex<HashSet<String>>,
    /// The server sent `workspace/diagnostic/refresh`: pulled results are stale.
    refresh: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn frame(message: &Value) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    write_message(&mut bytes, message)?;
    Ok(bytes)
}

/// `section` of `settings`: an exact key first, then a dotted path.
fn setting(settings: &Value, section: &str) -> Value {
    if let Some(value) = settings.get(section) {
        return value.clone();
    }
    section
        .split('.')
        .try_fold(settings, |value, key| value.get(key))
        .cloned()
        .unwrap_or(Value::Null)
}

impl Shared {
    fn answer_request(&self, method: &str, params: &Value) -> Result<Value, (i64, &'static str)> {
        match method {
            "client/registerCapability" => {
                let entries = params
                    .get("registrations")
                    .and_then(Value::as_array)
                    .ok_or((-32602, "registrations must be an array"))?;
                let mut registrations = lock(&self.registrations);
                for entry in entries {
                    if let Some(id) = entry.get("id").and_then(Value::as_str) {
                        registrations.insert(id.to_owned(), entry.clone());
                    }
                }
                Ok(Value::Null)
            }
            "client/unregisterCapability" => {
                let entries = params
                    .get("unregisterations")
                    .or_else(|| params.get("unregistrations"))
                    .and_then(Value::as_array)
                    .ok_or((-32602, "unregistrations must be an array"))?;
                let mut registrations = lock(&self.registrations);
                for entry in entries {
                    if let Some(id) = entry.get("id").and_then(Value::as_str) {
                        registrations.remove(id);
                    }
                }
                Ok(Value::Null)
            }
            "workspace/configuration" => Ok(configuration(&self.settings, params)),
            "workspace/diagnostic/refresh" => {
                self.refresh.store(true, Ordering::SeqCst);
                let _guard = lock(&self.diagnostics);
                self.published.notify_all();
                Ok(Value::Null)
            }
            _ => answer_server_request(method, params, &self.root_uri),
        }
    }

    /// Queue a frame and wait for the writer thread to deliver it.
    fn send(&self, message: &Value) -> io::Result<()> {
        let (ack, done) = mpsc::channel();
        self.enqueue(message, Some(ack))?;
        match done.recv_timeout(WRITE_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "server did not read its input within {}s",
                    WRITE_TIMEOUT.as_secs()
                ),
            )),
            Err(RecvTimeoutError::Disconnected) => {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "writer stopped"))
            }
        }
    }

    /// Queue a frame without waiting (replies sent from the reader thread).
    fn send_nowait(&self, message: &Value) {
        let _ = self.enqueue(message, None);
    }

    fn enqueue(&self, message: &Value, ack: Option<Sender<io::Result<()>>>) -> io::Result<()> {
        let frame = frame(message)?;
        let sender = lock(&self.outgoing)
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"))?;
        sender
            .send(Outgoing { frame, ack })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer stopped"))
    }

    fn handle(&self, message: Value) {
        let id = message.get("id").filter(|id| !id.is_null()).cloned();
        match (message.get("method").and_then(Value::as_str), id) {
            (Some(method), Some(id)) => {
                let params = message.get("params").unwrap_or(&Value::Null);
                let reply = match self.answer_request(method, params) {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err((code, text)) => {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": text}})
                    }
                };
                self.send_nowait(&reply);
            }
            (Some("textDocument/publishDiagnostics"), None) => {
                self.store_diagnostics(&message["params"])
            }
            (Some("$/progress"), None) => self.track_progress(&message["params"]),
            (Some(_), None) => {}
            (None, Some(id)) => {
                let Some(tx) = id.as_i64().and_then(|id| lock(&self.pending).remove(&id)) else {
                    return;
                };
                let reply = match message.get("error") {
                    Some(error) => Err(RpcError {
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                            .to_owned(),
                    }),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(reply);
            }
            (None, None) => {}
        }
    }

    fn track_progress(&self, params: &Value) {
        let token = match params.get("token") {
            Some(Value::String(token)) => token.clone(),
            Some(token) if !token.is_null() => token.to_string(),
            _ => return,
        };
        match params["value"]["kind"].as_str() {
            Some("begin") => {
                lock(&self.progress).insert(token);
            }
            Some("end") => {
                lock(&self.progress).remove(&token);
                let _guard = lock(&self.diagnostics);
                self.published.notify_all();
            }
            _ => {}
        }
    }

    fn store_diagnostics(&self, params: &Value) {
        let Some(path) = params
            .get("uri")
            .and_then(Value::as_str)
            .and_then(path_from_uri)
        else {
            return;
        };
        let items = params
            .get("diagnostics")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.store(path, params.get("version").and_then(Value::as_i64), items);
    }

    fn store(&self, path: PathBuf, version: Option<i64>, items: Vec<Value>) {
        let mut diagnostics = lock(&self.diagnostics);
        diagnostics.counter += 1;
        let counter = diagnostics.counter;
        diagnostics.by_file.insert(path, (counter, version, items));
        self.published.notify_all();
    }

    /// The stream ended: fail every waiting request and wake diagnostic waiters.
    fn close(&self) {
        let mut pending = lock(&self.pending);
        self.closed.store(true, Ordering::SeqCst);
        pending.clear();
        drop(pending);
        lock(&self.outgoing).take();
        let _guard = lock(&self.diagnostics);
        self.published.notify_all();
    }
}

fn configuration(settings: &Value, params: &Value) -> Value {
    let items = params
        .get("items")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    Value::Array(
        items
            .iter()
            .map(|item| match item.get("section").and_then(Value::as_str) {
                Some(section) if !section.is_empty() => setting(settings, section),
                _ => settings.clone(),
            })
            .collect(),
    )
}

/// Reply to a request the server sent us that needs no session state.
fn answer_server_request(
    method: &str,
    params: &Value,
    root_uri: &str,
) -> Result<Value, (i64, &'static str)> {
    match method {
        "workspace/configuration" => Ok(configuration(&Value::Null, params)),
        "workspace/workspaceFolders" => Ok(json!([{"uri": root_uri, "name": "workspace"}])),
        "window/workDoneProgress/create"
        | "client/registerCapability"
        | "client/unregisterCapability"
        | "workspace/diagnostic/refresh"
        | "workspace/semanticTokens/refresh"
        | "workspace/inlayHint/refresh"
        | "workspace/inlineValue/refresh"
        | "workspace/codeLens/refresh"
        | "window/showMessageRequest"
        | "window/showDocument" => Ok(Value::Null),
        "workspace/applyEdit" => Ok(json!({
            "applied": false,
            "failureReason": "unsolicited workspace/applyEdit is refused; use an explicitly approved lsp rename or code_action_apply transaction",
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

fn writer_loop(queue: Receiver<Outgoing>, mut output: Box<dyn Write + Send>) {
    for Outgoing { frame, ack } in queue {
        let result = output.write_all(&frame).and_then(|()| output.flush());
        if let Some(ack) = ack {
            let _ = ack.send(result);
        }
    }
}

struct OpenDoc {
    version: i32,
    hash: u64,
    language_id: String,
    used: u64,
}

fn hash_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn is_retryable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RpcError>()
        .is_some_and(|rpc| matches!(rpc.code, CONTENT_MODIFIED | SERVER_CANCELLED))
}

/// Whether a pure wait should stop early because a new user message is queued.
pub fn wait_interrupted() -> bool {
    crate::tools::exec::signals::wait_interrupted()
}

pub struct Client {
    name: String,
    root: PathBuf,
    capabilities: Mutex<Value>,
    shared: std::sync::Arc<Shared>,
    next_id: AtomicI64,
    /// Bumped by `cancel_pending_requests`, so a retry loop sleeping through a cancel stops.
    cancellations: AtomicU64,
    docs: Mutex<HashMap<PathBuf, OpenDoc>>,
    clock: AtomicU64,
    child: Mutex<Option<Child>>,
    timeouts: Timeouts,
    /// Set once the handshake succeeded; only then does shutdown speak the protocol.
    ready: bool,
}

impl Client {
    /// Start `program` with `args` in `root`, run the handshake with `options`, and return the client.
    pub fn spawn(
        name: &str,
        program: &Path,
        args: &[String],
        root: &Path,
        timeouts: Timeouts,
        options: ServerOptions,
    ) -> Result<Self> {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = crate::platform::process::spawn_group(&mut command)
            .map_err(|e| anyhow!("starting {} ({}): {e}", name, program.display()))?;
        let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
        let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
            kill_group(&mut child);
            bail!("{name}: could not attach to the server's stdio");
        };
        Self::connect(
            name,
            root,
            Box::new(stdout),
            Box::new(stdin),
            Some(child),
            timeouts,
            options,
        )
    }

    /// Run the handshake over an existing stream pair with `options`. Dropping the client tears the server down:
    /// `shutdown`, `exit`, then a kill of `child` (and its process group) if it lingers.
    pub fn connect(
        name: &str,
        root: &Path,
        input: Box<dyn Read + Send>,
        output: Box<dyn Write + Send>,
        child: Option<Child>,
        timeouts: Timeouts,
        options: ServerOptions,
    ) -> Result<Self> {
        let root_uri = uri_from_path(root);
        let (queue, outgoing) = mpsc::channel();
        let shared = std::sync::Arc::new(Shared {
            outgoing: Mutex::new(Some(queue)),
            pending: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(Diagnostics::default()),
            registrations: Mutex::new(HashMap::new()),
            published: Condvar::new(),
            closed: AtomicBool::new(false),
            root_uri: root_uri.clone(),
            settings: options.settings.clone(),
            progress: Mutex::new(HashSet::new()),
            refresh: AtomicBool::new(false),
        });
        let reader = std::sync::Arc::clone(&shared);
        let spawned = thread::Builder::new()
            .name(format!("dial-lsp-{name}-writer"))
            .spawn(move || writer_loop(outgoing, output))
            .and_then(|_| {
                thread::Builder::new()
                    .name(format!("dial-lsp-{name}"))
                    .spawn(move || reader_loop(&reader, input))
            });
        let mut child = child;
        if let Err(error) = spawned {
            if let Some(child) = child.as_mut() {
                kill_group(child);
            }
            return Err(error.into());
        }
        let mut client = Self {
            name: name.to_owned(),
            root: root.to_owned(),
            capabilities: Mutex::new(Value::Null),
            shared,
            next_id: AtomicI64::new(1),
            cancellations: AtomicU64::new(0),
            docs: Mutex::new(HashMap::new()),
            clock: AtomicU64::new(0),
            child: Mutex::new(child),
            timeouts,
            ready: false,
        };
        client.initialize(root, &root_uri, &options)?;
        client.ready = true;
        Ok(client)
    }

    fn initialize(&self, root: &Path, root_uri: &str, options: &ServerOptions) -> Result<()> {
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace");
        let mut params = json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "dial"},
            "rootUri": root_uri,
            "rootPath": root,
            "workspaceFolders": [{"uri": root_uri, "name": name}],
            "capabilities": {
                "general": {"positionEncodings": ["utf-16"]},
                "workspace": {
                    "workspaceFolders": true, "configuration": true, "symbol": {},
                    "applyEdit": false,
                    "workspaceEdit": {"documentChanges": true, "resourceOperations": ["rename"],
                        "failureHandling": "transactional", "changeAnnotationSupport": {"groupsOnLabel": false}},
                    "didChangeConfiguration": {"dynamicRegistration": false},
                    "didChangeWatchedFiles": {"dynamicRegistration": true, "relativePatternSupport": false},
                    "diagnostics": {"refreshSupport": true},
                    "fileOperations": {"dynamicRegistration": true, "willRename": true, "didRename": true},
                },
                "window": {"workDoneProgress": true},
                "textDocument": {
                    "synchronization": {"didSave": true, "willSave": false, "dynamicRegistration": false},
                    "publishDiagnostics": {"relatedInformation": false, "versionSupport": true},
                    "diagnostic": {"dynamicRegistration": true, "relatedDocumentSupport": false},
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "definition": {"linkSupport": true},
                    "typeDefinition": {"linkSupport": true},
                    "implementation": {"linkSupport": true},
                    "references": {},
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                    "formatting": {"dynamicRegistration": true},
                    "rename": {"prepareSupport": false},
                    "codeAction": {
                        "dynamicRegistration": true,
                        "codeActionLiteralSupport": {"codeActionKind": {"valueSet": [
                            "", "quickfix", "refactor", "refactor.extract", "refactor.inline",
                            "refactor.rewrite", "source", "source.organizeImports", "source.fixAll"
                        ]}},
                        "isPreferredSupport": true, "disabledSupport": true,
                        "dataSupport": true, "resolveSupport": {"properties": ["edit", "command"]},
                    },
                },
            },
        });
        if !options.init_options.is_null() {
            params["initializationOptions"] = options.init_options.clone();
        }
        let initialized = self.request_within("initialize", params, self.timeouts.initialize)?;
        let capabilities = initialized
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if capabilities
            .get("positionEncoding")
            .and_then(Value::as_str)
            .is_some_and(|encoding| encoding != "utf-16")
        {
            bail!(
                "{} selected a position encoding other than UTF-16",
                self.name
            );
        }
        *lock(&self.capabilities) = capabilities;
        self.notify("initialized", json!({}))?;
        if !options.settings.is_null() {
            self.notify(
                "workspace/didChangeConfiguration",
                json!({"settings": options.settings}),
            )?;
        }
        Ok(())
    }

    pub fn is_alive(&self) -> bool {
        !self.shared.closed.load(Ordering::SeqCst)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The server reports work in progress (indexing, flycheck).
    pub fn is_busy(&self) -> bool {
        !lock(&self.shared.progress).is_empty()
    }

    pub fn capabilities(&self) -> Value {
        json!({
            "initialize": lock(&self.capabilities).clone(),
            "dynamic_registrations": lock(&self.shared.registrations).values().cloned().collect::<Vec<_>>(),
            "client_limits": {
                "position_encoding": "utf-16",
                "resource_operations": ["rename (rename_file action and validated rename edits)"],
                "unsolicited_apply_edit": false,
                "execute_command": "separate approval; external effects are not transactional",
                "open_documents": MAX_OPEN_DOCUMENTS,
            },
        })
    }

    /// Static capability at `pointer` (present and not `false`), or a dynamic registration of `method`.
    pub fn supports(&self, pointer: &str, method: &str) -> bool {
        lock(&self.capabilities)
            .pointer(pointer)
            .is_some_and(|value| !value.is_null() && *value != json!(false))
            || lock(&self.shared.registrations)
                .values()
                .any(|entry| entry["method"] == method)
    }

    pub fn status(&self) -> Value {
        json!({
            "name": self.name(), "root": self.root, "alive": self.is_alive(), "busy": self.is_busy(),
            "process_id": lock(&self.child).as_ref().map(Child::id),
            "open_documents": lock(&self.docs).iter().map(|(path, doc)| {
                json!({"path": path, "version": doc.version})
            }).collect::<Vec<_>>(),
        })
    }

    pub fn can_resolve_code_action(&self) -> bool {
        lock(&self.capabilities)["codeActionProvider"]["resolveProvider"].as_bool() == Some(true)
            || lock(&self.shared.registrations).values().any(|entry| {
                entry["method"] == "textDocument/codeAction"
                    && entry["registerOptions"]["resolveProvider"].as_bool() == Some(true)
            })
    }

    /// Versioned edits are valid only for documents whose exact text we synchronized.
    /// Even an unversioned edit must not target an out-of-date open document.
    pub fn validate_document(&self, path: &Path, text: &str, version: Option<i64>) -> Result<()> {
        let docs = lock(&self.docs);
        match docs.get(path) {
            Some(doc) if doc.hash != hash_text(text) => {
                bail!("{} changed since synchronization", path.display())
            }
            Some(doc) if version.is_some_and(|version| version != i64::from(doc.version)) => bail!(
                "{} has a stale document version (server edit {:?}, current {})",
                path.display(),
                version,
                doc.version
            ),
            None if version.is_some() => bail!(
                "{} has a versioned edit but was never synchronized",
                path.display()
            ),
            _ => Ok(()),
        }
    }

    /// Explicit lifecycle shutdown, also used before an approved reload.
    pub fn stop(&self) {
        if self.is_alive() {
            if self.ready {
                let _ = self.request_within("shutdown", Value::Null, self.timeouts.shutdown);
            }
            let _ = self.notify("exit", Value::Null);
        }
        self.shared.close();
        let Some(mut child) = lock(&self.child).take() else {
            return;
        };
        let deadline = Instant::now() + self.timeouts.shutdown;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        kill_group(&mut child);
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.shared
            .send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .map_err(|e| anyhow!("{} is not accepting messages: {e}", self.name))
    }

    /// Cancel only this session's currently pending requests, without taking the tool's
    /// operation/workspace lock or tearing down a parent/sibling language server.
    pub fn cancel_pending_requests(&self) {
        self.cancellations.fetch_add(1, Ordering::AcqRel);
        let pending = std::mem::take(&mut *lock(&self.shared.pending));
        for (id, reply) in pending {
            let _ = reply.send(Err(RpcError {
                code: -32800,
                message: "LSP operation cancelled".into(),
            }));
            self.shared.send_nowait(
                &json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": id}}),
            );
        }
    }

    /// A request, retried when the server answers `ContentModified`/`ServerCancelled`.
    pub fn request(&self, method: &str, params: Value) -> Result<Value> {
        let mut attempt = 0;
        let generation = self.cancellations.load(Ordering::Acquire);
        loop {
            match self.request_within(method, params.clone(), self.timeouts.request) {
                Err(error) if attempt < RETRIES && is_retryable(&error) && self.is_alive() => {
                    attempt += 1;
                    thread::sleep(Duration::from_millis(100 * u64::from(attempt)));
                    if self.cancellations.load(Ordering::Acquire) != generation {
                        bail!("{} {method} cancelled", self.name);
                    }
                }
                other => return other,
            }
        }
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
                self.shared.send_nowait(
                    &json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": id}}),
                );
                bail!(
                    "{} did not answer {method} within {}s",
                    self.name,
                    limit.as_secs().max(1)
                )
            }
            Err(RecvTimeoutError::Disconnected) => {
                bail!("{} exited while handling {method}", self.name)
            }
        }
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed)
    }

    /// Make the server's view of `path` match `text`: `didOpen` the first time, a full-text
    /// `didChange` when the content differs from what it last saw. Returns whether anything was
    /// sent. Past [`MAX_OPEN_DOCUMENTS`] the least recently used other document is closed.
    pub fn sync_document(&self, path: &Path, language_id: &str, text: &str) -> Result<bool> {
        let hash = hash_text(text);
        let uri = uri_from_path(path);
        let used = self.tick();
        let mut docs = lock(&self.docs);
        match docs.get_mut(path) {
            None => {
                self.notify(
                    "textDocument/didOpen",
                    json!({"textDocument": {"uri": uri, "languageId": language_id, "version": 1, "text": text}}),
                )?;
                docs.insert(
                    path.to_owned(),
                    OpenDoc {
                        version: 1,
                        hash,
                        language_id: language_id.to_owned(),
                        used,
                    },
                );
                while docs.len() > MAX_OPEN_DOCUMENTS {
                    let Some(oldest) = docs
                        .iter()
                        .filter(|(open, _)| open.as_path() != path)
                        .min_by_key(|(_, doc)| doc.used)
                        .map(|(open, _)| open.clone())
                    else {
                        break;
                    };
                    docs.remove(&oldest);
                    self.notify(
                        "textDocument/didClose",
                        json!({"textDocument": {"uri": uri_from_path(&oldest)}}),
                    )?;
                }
            }
            Some(doc) if doc.hash == hash => {
                doc.used = used;
                return Ok(false);
            }
            Some(doc) => {
                let version = doc
                    .version
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("document version exhausted; reload the server"))?;
                self.notify(
                    "textDocument/didChange",
                    json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": text}]}),
                )?;
                doc.version = version;
                doc.hash = hash;
                doc.used = used;
            }
        }
        Ok(true)
    }

    /// Other live servers only need to refresh overlays they already own.
    pub fn sync_if_open(&self, path: &Path, text: &str) -> Result<()> {
        let language = lock(&self.docs)
            .get(path)
            .map(|doc| doc.language_id.clone());
        if let Some(language) = language {
            self.sync_document(path, &language, text)?;
        }
        Ok(())
    }

    /// The synchronized version of `path`, if it is open.
    pub fn version(&self, path: &Path) -> Option<i32> {
        lock(&self.docs).get(path).map(|doc| doc.version)
    }

    /// `textDocument/didSave` for an open document, with text when the server asks for it.
    pub fn did_save(&self, path: &Path, text: &str) -> Result<()> {
        if !lock(&self.docs).contains_key(path) {
            return Ok(());
        }
        let include = lock(&self.capabilities)["textDocumentSync"]["save"]["includeText"].as_bool()
            == Some(true);
        let mut params = json!({"textDocument": {"uri": uri_from_path(path)}});
        if include {
            params["text"] = json!(text);
        }
        self.notify("textDocument/didSave", params)
    }

    /// `workspace/didChangeWatchedFiles`; `kind` 1 created, 2 changed, 3 deleted.
    pub fn files_changed(&self, changes: &[(&Path, u8)]) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let changes: Vec<Value> = changes
            .iter()
            .map(|(path, kind)| json!({"uri": uri_from_path(path), "type": kind}))
            .collect();
        self.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes": changes}),
        )
    }

    /// Bring every other open document in line with the disk: changed files are re-synced,
    /// deleted or unreadable ones are closed. Returns one message per document that failed to sync;
    /// those are closed so the next sync reopens them.
    pub fn refresh_open_documents(&self, except: &Path) -> Vec<String> {
        let open: Vec<(PathBuf, String)> = lock(&self.docs)
            .iter()
            .filter(|(path, _)| path.as_path() != except)
            .map(|(path, doc)| (path.clone(), doc.language_id.clone()))
            .collect();
        let mut failures = Vec::new();
        for (path, language_id) in open {
            match read_source(&path) {
                Ok(text) => {
                    if let Err(error) = self.sync_document(&path, &language_id, &text) {
                        self.close_document(&path);
                        failures.push(format!("{}: {error:#}", path.display()));
                    }
                }
                Err(_) => self.close_document(&path),
            }
        }
        failures
    }

    pub fn close_document(&self, path: &Path) {
        if lock(&self.docs).remove(path).is_some() {
            let _ = self.notify(
                "textDocument/didClose",
                json!({"textDocument": {"uri": uri_from_path(path)}}),
            );
        }
    }

    /// Whether a `fileOperations` registration for `operation` (`willRename`/`didRename`) covers
    /// `path`: either the static capability's filters or a dynamic registration of `method`.
    fn file_operation_applies(&self, operation: &str, method: &str, path: &Path) -> bool {
        let mut lists: Vec<Value> = Vec::new();
        if let Some(options) = lock(&self.capabilities)
            .pointer(&format!("/workspace/fileOperations/{operation}"))
            .cloned()
        {
            lists.push(options["filters"].clone());
        }
        lists.extend(
            lock(&self.shared.registrations)
                .values()
                .filter(|entry| entry["method"] == method)
                .map(|entry| entry["registerOptions"]["filters"].clone()),
        );
        let absolute = path.to_string_lossy();
        let relative = path
            .strip_prefix(&self.root)
            .ok()
            .map(|rel| rel.to_string_lossy().into_owned());
        lists
            .iter()
            .filter_map(Value::as_array)
            .flatten()
            .any(|filter| Self::filter_matches(filter, &absolute, relative.as_deref()))
    }

    /// One LSP `FileOperationFilter` against a file. Renames are sent per file, so folder-only
    /// filters never apply; globs use LSP semantics (`*` and `**` respect `/`).
    fn filter_matches(filter: &Value, absolute: &str, relative: Option<&str>) -> bool {
        if filter
            .get("scheme")
            .and_then(Value::as_str)
            .unwrap_or("file")
            != "file"
        {
            return false;
        }
        let pattern = &filter["pattern"];
        if pattern["matches"].as_str() == Some("folder") {
            return false;
        }
        let Some(glob) = pattern["glob"].as_str() else {
            return false;
        };
        let ignore_case = pattern["options"]["ignoreCase"].as_bool().unwrap_or(false);
        let Ok(glob) = GlobBuilder::new(glob)
            .literal_separator(true)
            .case_insensitive(ignore_case)
            .build()
        else {
            return false;
        };
        let matcher = glob.compile_matcher();
        matcher.is_match(absolute) || relative.is_some_and(|rel| matcher.is_match(rel))
    }

    /// `workspace/willRenameFiles`: text edits (e.g. imports) the server wants with the rename.
    pub fn will_rename(&self, from: &Path, to: &Path) -> Result<Value> {
        if !self.file_operation_applies("willRename", "workspace/willRenameFiles", from) {
            return Ok(Value::Null);
        }
        self.request(
            "workspace/willRenameFiles",
            json!({"files": [{"oldUri": uri_from_path(from), "newUri": uri_from_path(to)}]}),
        )
    }

    /// After a committed rename: close the old document and tell the server.
    pub fn did_rename(&self, from: &Path, to: &Path) -> Result<()> {
        self.close_document(from);
        if self.file_operation_applies("didRename", "workspace/didRenameFiles", from) {
            self.notify(
                "workspace/didRenameFiles",
                json!({"files": [{"oldUri": uri_from_path(from), "newUri": uri_from_path(to)}]}),
            )?;
        }
        self.files_changed(&[(from, 3), (to, 1)])
    }

    /// Whole-document formatting edits, or `None` when the server cannot format.
    pub fn format(&self, path: &Path, text: &str) -> Result<Option<Vec<Value>>> {
        if !self.supports("/documentFormattingProvider", "textDocument/formatting") {
            return Ok(None);
        }
        let (tab_size, insert_spaces) = indentation(text);
        let result = self.request("textDocument/formatting", json!({
            "textDocument": {"uri": uri_from_path(path)},
            "options": {"tabSize": tab_size, "insertSpaces": insert_spaces,
                "trimTrailingWhitespace": true, "insertFinalNewline": true, "trimFinalNewlines": true},
        }))?;
        Ok(Some(result.as_array().cloned().unwrap_or_default()))
    }

    /// Whether the server offers pull diagnostics (`textDocument/diagnostic`).
    pub fn supports_pull(&self) -> bool {
        self.supports("/diagnosticProvider", "textDocument/diagnostic")
    }

    /// Pull diagnostics for the current version of `path`, stored like a publish. With `limit`,
    /// one request bounded by it and never retried; otherwise the usual retried request.
    fn pull_diagnostics(&self, path: &Path, limit: Option<Duration>) -> Result<Vec<Value>> {
        let previous = lock(&self.shared.diagnostics).result_ids.get(path).cloned();
        let version = self.version(path).map(i64::from);
        let mut params = json!({"textDocument": {"uri": uri_from_path(path)}});
        let previous = previous.filter(|_| !self.shared.refresh.swap(false, Ordering::SeqCst));
        if let Some((id, _)) = &previous {
            params["previousResultId"] = json!(id);
        }
        let result = match limit {
            Some(limit) => self.request_within("textDocument/diagnostic", params, limit)?,
            None => self.request("textDocument/diagnostic", params)?,
        };
        let unchanged = result.get("kind").and_then(Value::as_str) == Some("unchanged");
        // "unchanged" means the items of the result id we sent, not whatever was published last.
        let items = match (unchanged, previous) {
            (true, Some((_, items))) => items,
            _ => result
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        };
        let mut diagnostics = lock(&self.shared.diagnostics);
        match result.get("resultId").and_then(Value::as_str) {
            Some(id) => {
                diagnostics
                    .result_ids
                    .insert(path.to_owned(), (id.to_owned(), items.clone()));
            }
            None => {
                diagnostics.result_ids.remove(path);
            }
        }
        drop(diagnostics);
        self.shared.store(path.to_owned(), version, items.clone());
        Ok(items)
    }

    /// Counter value of the last publish for `path` (0 if none yet).
    pub fn diagnostics_mark(&self, path: &Path) -> u64 {
        lock(&self.shared.diagnostics)
            .by_file
            .get(path)
            .map_or(0, |(seq, _, _)| *seq)
    }

    /// The diagnostics the server last reported for `path`, whatever version they describe.
    pub fn last_diagnostics(&self, path: &Path) -> Option<Vec<Value>> {
        lock(&self.shared.diagnostics)
            .by_file
            .get(path)
            .map(|(_, _, items)| items.clone())
    }

    /// Diagnostics for `path` within the default diagnostics timeout; see [`Self::diagnostics_within`].
    pub fn diagnostics(&self, path: &Path, mark: u64, fresh: bool) -> Option<Vec<Value>> {
        self.diagnostics_within(path, mark, fresh, self.timeouts.diagnostics)
    }

    /// Like [`Self::diagnostics_within`], but `budget` bounds the whole call, a pull request
    /// included (which is then sent once, without retries), so an edit never waits longer.
    pub fn diagnostics_bounded(
        &self,
        path: &Path,
        mark: u64,
        fresh: bool,
        budget: Duration,
    ) -> Option<Vec<Value>> {
        self.collect_diagnostics(path, mark, fresh, budget, true)
    }

    /// Diagnostics for the current document version of `path`. When `fresh` (the document was
    /// just opened or changed) wait for a publish newer than `mark` and let follow-up publishes
    /// settle; otherwise return what the server last said, waiting only if it has said nothing
    /// yet. Pull-capable servers are asked directly. `None`: nothing arrived within `budget`.
    pub fn diagnostics_within(
        &self,
        path: &Path,
        mark: u64,
        fresh: bool,
        budget: Duration,
    ) -> Option<Vec<Value>> {
        self.collect_diagnostics(path, mark, fresh, budget, false)
    }

    fn collect_diagnostics(
        &self,
        path: &Path,
        mark: u64,
        fresh: bool,
        budget: Duration,
        bounded: bool,
    ) -> Option<Vec<Value>> {
        let started = Instant::now();
        if self.supports_pull() {
            let pulled = self.pull_diagnostics(path, bounded.then_some(budget));
            if let Ok(items) = pulled {
                return Some(items);
            }
        }
        let expected = self.version(path).map(i64::from);
        let seq_of = |d: &Diagnostics| {
            d.by_file
                .get(path)
                .filter(|(_, version, _)| version.is_none() || *version == expected)
                .map_or(0, |(seq, _, _)| *seq)
        };
        let deadline = if bounded { started } else { Instant::now() } + budget;
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
        let (_, _, items) = guard.by_file.get(path)?;
        let seq = seq_of(&guard);
        (seq > 0 && (!fresh || seq > mark)).then(|| items.clone())
    }

    /// Block until `done` holds, the server exits, a new user message interrupts, or `until` passes.
    fn wait_diagnostics<'a>(
        &self,
        mut guard: MutexGuard<'a, Diagnostics>,
        until: Instant,
        done: impl Fn(&Diagnostics) -> bool,
    ) -> MutexGuard<'a, Diagnostics> {
        while !done(&guard) && self.is_alive() && !wait_interrupted() {
            let Some(left) = until.checked_duration_since(Instant::now()) else {
                break;
            };
            // Wake periodically so an interrupting user message is noticed promptly.
            guard = self
                .shared
                .published
                .wait_timeout(guard, left.min(Duration::from_millis(200)))
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        guard
    }
}

/// Indentation options for formatting: tabs if any line starts with one, else the more common of
/// the 2- and 4-space steps between consecutive non-blank lines (default 4).
fn indentation(text: &str) -> (u32, bool) {
    if text.lines().any(|line| line.starts_with('\t')) {
        return (4, false);
    }
    let mut steps = [0usize; 9];
    let mut previous: Option<usize> = None;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let width = line.len() - line.trim_start_matches(' ').len();
        if let Some(before) = previous {
            let step = width.abs_diff(before);
            if (1..=8).contains(&step) {
                steps[step] += 1;
            }
        }
        previous = Some(width);
    }
    (if steps[2] > steps[4] { 2 } else { 4 }, true)
}

fn kill_group(child: &mut Child) {
    // The group leader remains unreaped until after its Job Object has been terminated.
    let _ = crate::platform::process::kill_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
