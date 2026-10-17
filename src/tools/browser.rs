//! Real Chromium automation through an operator-installed Playwright adapter.
//! Chromium has no OS network permission: all HTTP requests cross the guarded Rust bridge.
use super::background::procinfo::{ProcId, SIGKILL};
use super::ext::{Tool, ToolContext};
use super::kernel::{deadline, remaining_timeout, resolve_operator_executable};
#[cfg(target_os = "macos")]
use super::sandbox::{Sandbox, profile as sandbox_profile};
use super::web::{fetch::parse_url, guard::blocked_reason};
use super::{ToolOutput, tool_schema};
use crate::config::types::{Config, SandboxMode};
use crate::runtime::json_transport::{JsonProcess, execution_deadline};
use base64::{Engine, engine::general_purpose::STANDARD};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI32, Ordering},
};
use std::time::{Duration, Instant};
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::NextTimeout;

mod attach;
#[cfg(target_os = "linux")]
mod bwrap;
mod cmux;
mod discover;
mod external;
mod orphans;
mod profile;
mod run;
mod runtime;
mod stealth;
mod tern;
mod ws;

const BROWSER_DRIVER: &str = include_str!("browser_driver.cjs");
const BROWSER_RUN: &str = include_str!("browser_driver/run.cjs");
/// Driver program for `node -e`. The run/call module is inlined as an IIFE because `-e` has no `require` base.
fn driver_source() -> String {
    format!(
        "'use strict';const runModule=(function(){{const module={{exports:{{}}}};{BROWSER_RUN}\n;return module.exports}})();\n{BROWSER_DRIVER}"
    )
}
const MAX_BODY: usize = 4 * 1024 * 1024;
const PREFLIGHT: &str = r#"const readline=require('node:readline');readline.createInterface({input:process.stdin}).on('line',line=>{const r=JSON.parse(line);try{const entry=r.entry || require.resolve(process.env.DIAL_PLAYWRIGHT);const result={entry,node:process.execPath};if(r.entry)result.executable=require(entry).chromium.executablePath();process.stdout.write(JSON.stringify({id:r.id,result})+'\n')}catch(e){process.stdout.write(JSON.stringify({id:r.id,error:'Playwright failed to load (built-in runtime, or browser.playwright_module override which must be Playwright-core >=1.48): '+e.message})+'\n')}});"#;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserConfig {
    pub allowed_hosts: Vec<String>,
    /// Approved top-level localhost navigation grants only that exact host/port for this session.
    pub auto_localhost: bool,
    pub node: String,
    /// Empty (default) uses the built-in pinned runtime. A non-empty value is an operator-owned Playwright entry override.
    pub playwright_module: String,
    pub chromium_executable: Option<String>,
    pub timeout_secs: u64,
    /// Launch Chromium headless (default). Headed mode keeps the same sandbox and egress bridge.
    pub headless: bool,
    /// Operator-started Chromium DevTools endpoint to attach to (loopback only). Requires allow_attach.
    pub cdp_url: Option<String>,
    /// Opt-in gate for attaching to an existing browser. Attached sessions do not bridge page egress.
    pub allow_attach: bool,
    /// Named storage-state profile (cookies, localStorage) saved under the project state directory.
    pub profile: Option<String>,
    /// Record Playwright video into the session artifact directory.
    pub record_video: bool,
    /// Apply the stealth init scripts and launch flags (headless). `DIAL_BROWSER_STEALTH` overrides the setting.
    pub stealth: bool,
    /// Drive browser surfaces through a cmux socket when `CMUX_SOCKET_PATH` is set. `DIAL_BROWSER_CMUX` overrides.
    pub cmux: bool,
    /// Drive browser panes through Tern when `TERN_PANE_SOCKET` and `TERN_PANE` are set. `DIAL_BROWSER_TERN` overrides.
    pub tern: bool,
    /// Drive the Chrome tabs you already use through the Dial browser extension and relay. Requires allow_attach. `DIAL_BROWSER_RELAY` overrides.
    pub relay: bool,
    /// Loopback relay address the browser extension connects to. Empty uses http://127.0.0.1:9224.
    pub relay_url: String,
}
impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            node: "node".into(),
            playwright_module: String::new(),
            chromium_executable: None,
            timeout_secs: 0,
            headless: true,
            cdp_url: None,
            allow_attach: false,
            profile: None,
            record_video: false,
            allowed_hosts: Vec::new(),
            auto_localhost: true,
            stealth: true,
            cmux: true,
            tern: true,
            relay: false,
            relay_url: String::new(),
        }
    }
}
impl BrowserConfig {
    pub fn validate_allowed_hosts(&self) -> Result<(), String> {
        if self.allowed_hosts.len() > 128 {
            return Err("browser.allowed_hosts accepts at most 128 entries".into());
        }
        for entry in &self.allowed_hosts {
            let uri = parse_url(&format!("http://{entry}/"))?;
            let host = uri.host().ok_or("browser allowed host is missing")?;
            if uri.authority().map(|a| a.as_str()) != Some(entry.as_str())
                || uri.port_u16().is_none_or(|p| p == 0)
                || host.is_empty()
                || !host
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
            {
                return Err(format!(
                    "browser.allowed_hosts entry {entry:?} must be an exact lowercase hostname:port; no schemes, paths, credentials or wildcards"
                ));
            }
        }
        Ok(())
    }
}
pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(BrowserTool {
        config: config.browser.clone(),
        session: Mutex::new(None),
        external: Mutex::new(None),
        tern_fell_back: AtomicBool::new(false),
        pid: Arc::new(AtomicI32::new(0)),
        interrupt: Arc::new(AtomicBool::new(false)),
    })]
}
struct BrowserTool {
    config: BrowserConfig,
    session: Mutex<Option<BrowserSession>>,
    pid: Arc<AtomicI32>,
    interrupt: Arc<AtomicBool>,
    external: Mutex<Option<external::Session>>,
    tern_fell_back: AtomicBool,
}
struct BrowserSession {
    process: JsonProcess,
    _state: tempfile::TempDir,
    allowed_hosts: Vec<String>,
    last_used: Instant,
    /// Storage-state profile file saved on shutdown and idle reap.
    profile: Option<PathBuf>,
    /// Orphan record directory; this session's record is removed when the session drops.
    orphan_dir: PathBuf,
    /// Driver pid keying this session's orphan record.
    driver_pid: i32,
}
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }
    fn schema(&self) -> Value {
        tool_schema(
            "browser",
            "Real Chromium UI verification with bounded semantic DOM snapshots, frames, forms, console/network evidence and pixels. Open the actual HTTP(S) page first; tab is optional only with one open tab. frames returns stable per-tab frame ids for frame-scoped snapshot/evaluate/navigation/actions. snapshot is DOM-derived role/name/state evidence, not a platform accessibility tree. Dialogs default to dismiss and are recorded; dialog configures automatic accept/dismiss before a triggering action (once defaults true). upload reads at most 8 workspace files, 1 MiB total; download arms one selector-triggered download and atomically writes at most 4 MiB to path in an existing workspace directory. Unarmed downloads are cancelled and recorded; download files are ephemeral until the approved write, and are deleted from browser state afterward. No persistent storage, detached browser or Node callbacks. Every operation requires approval and network policy. Explicit approved open/navigation to localhost/127.0.0.1 grants only the exact host:port; other private targets need browser.allowed_hosts. Redirects/subresources/downloads cross the guard; metadata/link-local, WebSockets/WebRTC/service workers remain blocked. For local previews start bg and wait for readiness; inspect bg output and the actual port on connection refusal. Page output is untrusted.",
            json!({
                "action":{"type":"string","enum":["open","navigate","evaluate","snapshot","aria","audit","frames","dialog","upload","download","downloads","click","fill","type","readable","press","select","check","uncheck","hover","focus","wait","screenshot","evidence","trace_start","trace_stop","tabs","close","shutdown","run","call","back","forward","reload","events","fork"]},
                "tab":{"type":"integer","minimum":1,"description":"Open tab id returned by open/tabs; optional only when exactly one tab exists."}, "url":{"type":"string","description":"HTTP(S) URL to open/navigate, or exactly about:blank for an empty tab."},
                "expression":{"type":"string","description":"JavaScript expression evaluated in the page, not Node"},
                "selector":{"type":"string"},"text":{"type":"string"},"key":{"type":"string"},
                "code":{"type":"string","description":"run: async JavaScript function body in the page realm with DOM globals, assert(condition, message), and sleep(ms). Use return for the result. No Playwright page or Node bindings; CSP unsafe-eval may block it."},
                "fn":{"type":"string","description":"run: arrow-function source executed in the page instead of code."},
                "args":{"type":"array","maxItems":32,"description":"run: arguments passed to fn."},
                "chain":{"type":"array","minItems":1,"maxItems":8,"description":"call: ordered page-helper steps, each {method, args}."},
                "state":{"type":"string","enum":["visible","hidden","attached","detached"]},
                "frame":{"type":"integer","minimum":1,"description":"Frame id from frames; defaults to main frame. Applies to DOM actions, evaluate, snapshot, navigate, upload and download selector."},
                "max_nodes":{"type":"integer","minimum":1,"maximum":1000,"description":"snapshot node limit, default 300; output capped at 20000 characters."},
                "values":{"type":"array","items":{"type":"string"},"maxItems":32,"description":"select option values; text remains supported for a single value."},
                "files":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":8,"description":"upload workspace paths; selector must identify a file input."},
                "path":{"type":"string","description":"download destination within workspace; parent directory must already exist."},
                "dialog_mode":{"type":"string","enum":["accept","dismiss"],"description":"dialog policy for the next dialog, or all dialogs with once=false; prompt reply uses text."},
                "once":{"type":"boolean","description":"dialog policy applies once by default."},
                "dir":{"type":"string","enum":["right","down"],"description":"fork: direction of the new Tern pane."},
                "after":{"type":"integer","minimum":0,"description":"events: return events after this sequence number; default 0."},
                "full_page":{"type":"boolean"}, "timeout":{"type":"integer","minimum":0,"maximum":3600}
            }),
            &["action"],
        )
    }
    fn needs_approval(&self, _: &Value) -> bool {
        true
    }
    fn may_modify_workspace(&self, _: &Value) -> bool {
        false
    }
    fn manages_workspace_serialization(&self) -> bool {
        true
    }
    fn fork_session(&self) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self {
            config: self.config.clone(),
            session: Mutex::new(None),
            external: Mutex::new(None),
            tern_fell_back: AtomicBool::new(false),
            pid: Arc::new(AtomicI32::new(0)),
            interrupt: Arc::new(AtomicBool::new(false)),
        }))
    }
    fn cancel_operation(&self) {
        self.interrupt.store(true, Ordering::Release);
        let pid = self.pid.load(Ordering::Acquire);
        if pid > 0 {
            let _ = crate::platform::process::kill_group(pid as u32);
        }
    }
    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        match self.run(ctx, args) {
            Ok(v) => ToolOutput::structured(v),
            Err(e) => ToolOutput::err(e),
        }
    }
}
impl BrowserTool {
    fn run(&self, ctx: &ToolContext, args: &Value) -> Result<Value, String> {
        let mut notes = Vec::new();
        let mut value = self.run_action(ctx, args, &mut notes)?;
        if !notes.is_empty()
            && let Some(object) = value.as_object_mut()
        {
            object.insert("notes".into(), json!(notes));
        }
        Ok(value)
    }

    fn run_action(
        &self,
        ctx: &ToolContext,
        args: &Value,
        notes: &mut Vec<String>,
    ) -> Result<Value, String> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or("action is required")?;
        let timeout = deadline(args, self.config.timeout_secs)?;
        let end = execution_deadline(timeout);
        // Never erase cancellation between active-tool registration and call entry.
        if self.interrupt.swap(false, Ordering::AcqRel) {
            return Err("browser operation cancelled before startup".into());
        }
        let mut slot = loop {
            if self.interrupt.load(Ordering::Acquire) {
                return Err("browser operation cancelled while waiting for its session".into());
            }
            remaining_timeout(end)?;
            if let Some(slot) = self.session.try_lock() {
                break slot;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        if action == "shutdown" {
            retire(&mut slot)?;
            *self.external.lock() = None;
            self.tern_fell_back.store(false, Ordering::Release);
            return Ok(json!({"closed":true}));
        }
        if slot.as_ref().is_some_and(|s| s.last_used.elapsed() > IDLE)
            && let Err(error) = retire(&mut slot)
        {
            notes.push(format!(
                "browser profile was not saved on idle close: {error}"
            ));
        }
        if ctx.config.sandbox.mode == SandboxMode::Strict
            || (ctx.config.sandbox.mode == SandboxMode::Dev && !ctx.config.sandbox.allow_network)
        {
            return Err(
                "browser is unavailable: network access is disabled by sandbox policy".into(),
            );
        }
        if !matches!(
            action,
            "open"
                | "navigate"
                | "evaluate"
                | "snapshot"
                | "aria"
                | "audit"
                | "trace_start"
                | "trace_stop"
                | "frames"
                | "dialog"
                | "upload"
                | "download"
                | "downloads"
                | "check"
                | "uncheck"
                | "hover"
                | "focus"
                | "click"
                | "fill"
                | "type"
                | "readable"
                | "press"
                | "select"
                | "wait"
                | "screenshot"
                | "evidence"
                | "tabs"
                | "close"
                | "run"
                | "call"
                | "back"
                | "forward"
                | "reload"
                | "events"
                | "fork"
        ) {
            return Err(format!("unsupported browser action {action:?}"));
        }
        if matches!(action, "open" | "navigate") {
            let url = args
                .get("url")
                .and_then(Value::as_str)
                .ok_or("open/navigate requires URL")?;
            if url != "about:blank" {
                parse_url(url)?;
            }
        }
        let external_result = {
            let mut external = self.external.lock();
            if external.is_none() && !self.tern_fell_back.load(Ordering::Acquire) {
                *external = external::select(&self.config, |key| std::env::var(key).ok());
            }
            external.as_mut().map(|backend| backend.run(action, args))
        };
        match external_result {
            None => {}
            Some(Ok(value)) => return Ok(value),
            // Auto mode: an auto-selected Tern pane that cannot host `open` falls back to
            // Chromium once and the result says so. Tabs that fell back keep Chromium.
            Some(Err(external::ExternalError::Unavailable(message))) if action == "open" => {
                *self.external.lock() = None;
                self.tern_fell_back.store(true, Ordering::Release);
                notes.push(format!(
                    "Tern cannot show a browser here ({message}); opened Chromium instead. Set browser.tern = false to skip Tern."
                ));
            }
            Some(Err(error)) => return Err(error.into_message()),
        }
        if matches!(action, "back" | "forward" | "reload" | "events" | "fork") {
            return Err(format!(
                "browser action {action:?} requires a Tern browser pane"
            ));
        }
        let driver_request = match action {
            "run" => Some(run::parse_run(args)?.to_driver_json()),
            "call" => Some(run::parse_call(args)?.to_driver_json()),
            _ => None,
        };
        if args.to_string().len() > 64 * 1024 {
            return Err("browser arguments exceed 64 KiB".into());
        }
        // Transfers use Workspace confinement and observers, never model-supplied Node paths.
        let _workspace_operation = if matches!(action, "upload" | "download") {
            Some(loop {
                if self.interrupt.load(Ordering::Acquire) {
                    slot.take();
                    return Err(
                        "browser file transfer cancelled while waiting for workspace".into(),
                    );
                }
                remaining_timeout(end)?;
                if let Some(operation) = ctx.workspace.try_serial_operation() {
                    break operation;
                }
                std::thread::sleep(Duration::from_millis(10));
            })
        } else {
            None
        };
        let download_path = if action == "download" {
            let input = args["path"].as_str().ok_or("download requires path")?;
            let path = ctx.workspace.resolve(input)?;
            if path.is_dir() || path == ctx.workspace.root() {
                return Err("download path must be a file".into());
            }
            if !path.parent().is_some_and(Path::is_dir) {
                return Err("download parent directory must already exist".into());
            }
            ctx.authorize_mutation("browser", std::slice::from_ref(&path), false)?;
            ctx.workspace.observe_write(&path)?;
            Some(path)
        } else {
            None
        };
        let upload_files = if action == "upload" {
            Some(read_uploads(ctx, args)?)
        } else {
            None
        };
        self.config.validate_allowed_hosts()?;
        if slot.as_mut().is_some_and(|s| !s.process.alive()) {
            slot.take();
        }
        if slot.is_none() {
            *slot = Some(start(
                ctx,
                &self.config,
                remaining_timeout(end)?,
                self.pid.clone(),
                &self.interrupt,
            )?);
        }
        let remaining = remaining_timeout(end)?;
        if !timeout.is_zero() && remaining.is_zero() {
            slot.take();
            return Err("browser startup exceeded deadline".into());
        }
        if self.interrupt.load(Ordering::Acquire) {
            slot.take();
            return Err("browser operation cancelled during startup".into());
        }
        let mut request = args.clone();
        if let Some(Value::Object(fields)) = driver_request {
            request
                .as_object_mut()
                .ok_or("browser request must be an object")?
                .extend(fields);
        }
        request["timeout_ms"] = json!(remaining.as_millis() as u64);
        if let Some(files) = upload_files {
            request["upload_files"] = files;
        }
        let session = slot.as_mut().unwrap();
        // Tool::needs_approval gates this top-level request. Page callbacks/redirects cannot
        // authorize themselves: they only use the session's already-approved exact hosts.
        permit_requested_localhost(&mut session.allowed_hosts, &self.config, args)?;
        session.last_used = Instant::now();
        request["websocket_endpoints"] = json!(ws::websocket_endpoints(&session.allowed_hosts));
        let result = session.process.request(request, remaining, |event| {
            bridge(
                event,
                remaining_timeout(end)?,
                &session.allowed_hosts,
                self.interrupt.clone(),
            )
        });
        let mut result = match result {
            Ok(result) => result,
            Err(error) => {
                slot.take();
                return Err(error);
            }
        };
        // An ordinary Playwright/action error preserves the tab and its evidence. Only
        // transport failure, cancellation or deadline failure discards the process.
        if let Some(error) = result["browser_error"].as_str() {
            return Err(error.to_owned());
        }
        if matches!(action, "run" | "call") {
            result = run::shape_output(&result, run::RUN_OUTPUT_MAX_BYTES);
        }
        if let Some(path) = download_path {
            let encoded = result
                .as_object_mut()
                .and_then(|object| object.remove("download_body"))
                .ok_or("browser download returned no body")?;
            let bytes = STANDARD
                .decode(encoded.as_str().ok_or("invalid browser download body")?)
                .map_err(|e| format!("invalid browser download encoding: {e}"))?;
            if bytes.len() > MAX_BODY {
                return Err("browser download exceeds 4 MiB".into());
            }
            // Re-check after the interaction before installing a staged file.
            if ctx.workspace.resolve(args["path"].as_str().unwrap())? != path {
                return Err(
                    "download destination changed during interaction; nothing written".into(),
                );
            }
            let mut staged = tempfile::NamedTempFile::new_in(path.parent().unwrap())
                .map_err(|e| format!("staging browser download: {e}"))?;
            staged
                .write_all(&bytes)
                .and_then(|()| staged.as_file().sync_all())
                .map_err(|e| format!("staging browser download: {e}"))?;
            staged
                .persist(&path)
                .map_err(|e| format!("installing browser download: {e}"))?;
            ctx.workspace.store().forget(&path);
            result["path"] = json!(ctx.workspace.rel(&path));
            result["bytes"] = json!(bytes.len());
        }
        Ok(result)
    }
}

fn read_uploads(ctx: &ToolContext, args: &Value) -> Result<Value, String> {
    let files = args["files"].as_array().ok_or("upload requires files")?;
    if files.is_empty() || files.len() > 8 {
        return Err("upload requires 1 to 8 workspace files".into());
    }
    let mut total = 0usize;
    let mut payloads = Vec::with_capacity(files.len());
    for input in files {
        let input = input.as_str().ok_or("upload file paths must be strings")?;
        let path = ctx.workspace.resolve(input)?;
        let file = std::fs::File::open(&path).map_err(|e| format!("upload {input:?}: {e}"))?;
        if !file.metadata().map_err(|e| e.to_string())?.is_file() {
            return Err(format!("upload {input:?} is not a regular file"));
        }
        let budget = (1024 * 1024usize).saturating_sub(total);
        let mut bytes = Vec::new();
        file.take(budget as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > budget {
            return Err("upload files exceed the total 1 MiB limit".into());
        }
        total += bytes.len();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("upload filename is not UTF-8")?;
        payloads.push(json!({"name":name,"mimeType":"application/octet-stream","data":STANDARD.encode(bytes)}));
    }
    Ok(json!(payloads))
}

/// Only explicit, approved navigation can grant loopback access. Never called for page callbacks.
fn permit_requested_localhost(
    hosts: &mut Vec<String>,
    config: &BrowserConfig,
    args: &Value,
) -> Result<(), String> {
    if !config.auto_localhost || !matches!(args["action"].as_str(), Some("open" | "navigate")) {
        return Ok(());
    }
    let url = args["url"].as_str().ok_or("open/navigate requires URL")?;
    if url == "about:blank" {
        return Ok(());
    }
    let uri = parse_url(url)?;
    let host = uri.host().unwrap_or("");
    if !matches!(host, "localhost" | "127.0.0.1") {
        return Ok(());
    }
    // Uri::port() returns None for an out-of-range explicit port; do not mistake it for port 80.
    let suffix = uri
        .authority()
        .unwrap()
        .as_str()
        .strip_prefix(host)
        .unwrap_or("");
    let port = if suffix.is_empty() {
        if uri.scheme_str() == Some("https") {
            443
        } else {
            80
        }
    } else {
        let text = suffix
            .strip_prefix(':')
            .ok_or("invalid localhost authority")?;
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err("invalid localhost port".into());
        }
        text.parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or("invalid localhost port")?
    };
    let authority = format!("{host}:{port}");
    if !hosts.contains(&authority) {
        // Bound configured hosts plus session grants; no unbounded navigation history.
        if hosts.len() >= 256 {
            return Err("browser session host limit reached; shut down the browser before opening more local endpoints".into());
        }
        hosts.push(authority);
    }
    Ok(())
}

// Seatbelt profile helpers: only the macOS driver sandbox (and its tests) builds profiles.
#[cfg(any(target_os = "macos", test))]
fn profile_path(path: &Path) -> Result<String, String> {
    let path = path.to_str().ok_or("dependency path is not UTF-8")?;
    if !Path::new(path).is_absolute()
        || path
            .chars()
            .any(|c| c == '"' || c == '\\' || c.is_control())
    {
        return Err("invalid browser dependency path for sandbox".into());
    }
    Ok(format!("\"{path}\""))
}
fn operator_dependency(workspace: &Path, entry: &Path) -> Result<PathBuf, String> {
    let entry = entry
        .canonicalize()
        .map_err(|e| format!("Playwright module: {e}"))?;
    if entry.starts_with(workspace) {
        return Err("browser refuses workspace-owned Playwright code before requiring it; install Playwright outside the editable workspace and configure browser.playwright_module to that external absolute path".into());
    }
    Ok(entry)
}
#[cfg(any(target_os = "macos", test))]
fn dependency_metadata(paths: &[&Path]) -> Result<String, String> {
    let ancestors = paths
        .iter()
        .flat_map(|path| path.ancestors().skip(1))
        .collect::<std::collections::BTreeSet<_>>();
    let mut rules = String::from("(allow file-read-metadata");
    for ancestor in ancestors {
        rules.push_str(&format!(" (literal {})", profile_path(ancestor)?));
    }
    rules.push_str(")\n");
    Ok(rules)
}
#[cfg(any(target_os = "macos", test))]
fn state_unix_ipc(state: &Path) -> Result<String, String> {
    profile_path(state)?;
    let prefix = regex::escape(state.to_str().ok_or("browser state path is not UTF-8")?);
    Ok(format!(
        "(allow network-bind network-inbound (local unix-socket (path-regex #\"^{prefix}/\")))\n(allow network-outbound (remote unix-socket (path-regex #\"^{prefix}/\")))\n"
    ))
}
fn chromium_bundle_root(executable: &Path) -> Result<&Path, String> {
    executable
        .ancestors()
        .find(|ancestor| {
            ancestor
                .extension()
                .is_some_and(|extension| extension == "app")
        })
        .or_else(|| executable.parent())
        .ok_or_else(|| "Chromium executable has no parent".into())
}

const RELAY_REQUIRES_ATTACH: &str = "browser relay requires browser.allow_attach = true. Enable \"Allow attach\" in Settings → Browser (or set browser.allow_attach = true), then retry.";

/// `DIAL_BROWSER_RELAY` overrides `browser.relay` when it holds a recognised value.
fn relay_enabled(config: &BrowserConfig) -> bool {
    cmux::parse_flag(
        std::env::var("DIAL_BROWSER_RELAY").ok().as_deref(),
        config.relay,
    )
}

const IDLE: Duration = Duration::from_secs(600);
const SAVE_TIMEOUT: Duration = Duration::from_secs(10);

fn start(
    ctx: &ToolContext,
    config: &BrowserConfig,
    timeout: Duration,
    pid: Arc<AtomicI32>,
    interrupt: &AtomicBool,
) -> Result<BrowserSession, String> {
    #[cfg(target_os = "macos")]
    Sandbox::detect()
        .available()
        .map_err(|e| format!("browser requires an OS sandbox: {e}"))?;
    #[cfg(windows)]
    if ctx.config.sandbox.mode != SandboxMode::Off {
        return Err("browser on Windows requires an OS sandbox; strict/dev isolation is unavailable. Set sandbox.mode = \"off\" only if you explicitly accept an unsandboxed browser.".into());
    }
    let end = execution_deadline(timeout);
    let cdp = match config.cdp_url.as_deref() {
        Some(raw) => {
            if !config.allow_attach {
                return Err("browser.cdp_url requires browser.allow_attach = true".into());
            }
            Some(attach::validate_cdp_url(raw)?)
        }
        None if relay_enabled(config) => {
            if !config.allow_attach {
                return Err(RELAY_REQUIRES_ATTACH.into());
            }
            let cancelled = || interrupt.load(Ordering::Acquire);
            let url = crate::browser_relay::client::resolve(
                &config.relay_url,
                &ctx.config.user_dir,
                &cancelled,
            )
            .map_err(|error| format!("browser relay: {error}"))?;
            Some(attach::validate_cdp_url(&url)?)
        }
        None => None,
    };
    let profile_file = match config.profile.as_deref() {
        Some(name) => {
            profile::validate_profile_name(name)?;
            Some(profile::profile_path(&ctx.config.state_dir, name)?)
        }
        None => None,
    };
    let storage_state = match &profile_file {
        Some(path) => profile::load(path)?,
        None => None,
    };
    let trusted = resolve_operator_executable(ctx.workspace.root(), &config.node, "browser.node")?;
    let runtime = runtime::provision(interrupt)?;
    if interrupt.load(Ordering::Acquire) {
        return Err("browser startup cancelled".into());
    }
    let builtin = config.playwright_module.trim().is_empty();
    let browsers = builtin.then_some(runtime.browsers.as_path());
    let (entry, mut meta) = probe(ctx, &trusted, config, &runtime, &pid, interrupt, end)?;
    let node = PathBuf::from(meta["node"].as_str().ok_or("Node executable missing")?)
        .canonicalize()
        .map_err(|e| format!("Node executable: {e}"))?;
    if builtin
        && config.chromium_executable.is_none()
        && cdp.is_none()
        && !chromium_path(config, &meta)?.is_file()
    {
        match runtime::install_chromium(&node, &runtime, interrupt) {
            Ok(()) => (_, meta) = probe(ctx, &trusted, config, &runtime, &pid, interrupt, end)?,
            // Offline or a failed download still works when another Chromium is installed.
            Err(error)
                if discover::installed_chromium(&discover::playwright_roots(&runtime.browsers))
                    .is_none() =>
            {
                return Err(error);
            }
            Err(_) => {}
        }
    }
    if interrupt.load(Ordering::Acquire) {
        return Err("browser startup cancelled".into());
    }
    let executable_path = chromium_path(config, &meta)?;
    // Attaching to an operator browser never launches Chromium, so a missing bundled binary must not block it.
    let executable = match executable_path.canonicalize() {
        Ok(path) => path,
        Err(_) if cdp.is_some() => executable_path,
        Err(e) if config.chromium_executable.is_some() => {
            return Err(format!(
                "browser.chromium_executable {executable_path:?} is not usable: {e}; point it at an installed Chrome/Chromium or remove it"
            ));
        }
        // The build this Playwright expects is missing (e.g. a playwright_module whose Chromium
        // revision was never downloaded): use another installed Chromium before giving up.
        Err(e) => discover::installed_chromium(&discover::playwright_roots(&runtime.browsers))
            .and_then(|path| path.canonicalize().ok())
            .ok_or_else(|| {
                let cli = entry.with_file_name("cli.js");
                format!(
                    "Chromium is not installed ({executable_path:?}: {e}) and no other Chromium, Chrome or Edge was found. Fix: run `{} {} install chromium`, or set browser.chromium_executable to an installed Chrome/Chromium{}",
                    node.display(),
                    cli.display(),
                    if builtin { "" } else { ", or remove browser.playwright_module to use the built-in runtime" }
                )
            })?,
    };
    // Stealth applies to launched headless Chromium only, never to an attached user browser.
    let stealth_request =
        if stealth::enabled_from_config(config.stealth) && config.headless && cdp.is_none() {
            let executable_text = executable
                .to_str()
                .ok_or("Chromium executable path is not valid UTF-8")?;
            json!({
                "script": stealth::injection_script(),
                "args": stealth::launch_args(),
                "ignoreDefaultArgs": stealth::ignored_default_args(Some(executable_text)),
            })
        } else {
            Value::Null
        };
    let readability = runtime
        .readability
        .canonicalize()
        .map_err(|e| format!("browser runtime readability: {e}"))?;
    let runtime_root = runtime
        .root
        .canonicalize()
        .map_err(|e| format!("browser runtime: {e}"))?;
    let package_root = entry
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "node_modules"))
        .or_else(|| entry.parent())
        .ok_or("Playwright entry has no parent")?
        .to_path_buf();
    let reads = vec![
        package_root,
        runtime_root,
        chromium_bundle_root(&executable)?.to_path_buf(),
        node.parent()
            .ok_or("Node executable has no parent")?
            .to_path_buf(),
    ];
    let orphan_dir = ctx.config.state_dir.join("browser").join("orphans");
    orphans::sweep(&orphan_dir, |driver| {
        let _ = driver.signal_group(SIGKILL);
    });
    let state = tempfile::Builder::new()
        .prefix("dial-browser-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let state_path = state.path().canonicalize().map_err(|e| e.to_string())?;
    let artifacts = state_path.join("artifacts");
    std::fs::create_dir(&artifacts)
        .map_err(|e| format!("creating browser artifacts directory: {e}"))?;
    let state_os = state_path.clone().into_os_string();
    let mut env: Vec<(&str, OsString)> = vec![
        ("PATH", std::env::var_os("PATH").unwrap_or_default()),
        ("HOME", state_os.clone()),
        ("TMPDIR", state_os.clone()),
        ("TEMP", state_os.clone()),
        ("TMP", state_os.clone()),
        ("MAC_CHROMIUM_TMPDIR", state_os),
        ("LANG", "en_US.UTF-8".into()),
        ("DIAL_PLAYWRIGHT", entry.into_os_string()),
        ("DIAL_CHROMIUM", executable.into_os_string()),
        ("DIAL_READABILITY", readability.into_os_string()),
        ("DIAL_BROWSER_ARTIFACTS", artifacts.into_os_string()),
    ];
    if let Some(browsers) = browsers {
        env.push(("PLAYWRIGHT_BROWSERS_PATH", browsers.as_os_str().to_owned()));
    }
    if let Some(cdp) = &cdp {
        env.push(("DIAL_CDP_URL", OsString::from(cdp)));
    }
    let mut command = driver_command(ctx, &node, &reads, &state_path, cdp.is_some())?;
    command
        .env_clear()
        .envs(env)
        .current_dir(ctx.workspace.root());
    let process = JsonProcess::start(command, pid.clone())?;
    let driver_pid = pid.load(Ordering::Acquire);
    let owner =
        ProcId::current().ok_or("cannot record browser process ownership for orphan cleanup")?;
    let driver =
        ProcId::of(driver_pid).ok_or("browser driver exited before it could be recorded")?;
    orphans::write(&orphan_dir, owner, driver)?;
    let mut session = BrowserSession {
        process,
        _state: state,
        allowed_hosts: config.allowed_hosts.clone(),
        last_used: Instant::now(),
        profile: profile_file,
        orphan_dir,
        driver_pid,
    };
    if interrupt.load(Ordering::Acquire) {
        return Err("browser startup cancelled".into());
    }
    let remaining = remaining_timeout(end)?;
    if !timeout.is_zero() && remaining.is_zero() {
        return Err("browser startup exceeded deadline".into());
    }
    let initialized = session.process.request(
        json!({
            "action":"initialize",
            "timeout_ms":remaining.as_millis() as u64,
            "headless":config.headless,
            "record_video":config.record_video,
            "storage_state":storage_state,
            "websocket_endpoints":ws::websocket_endpoints(&config.allowed_hosts),
            "stealth":stealth_request
        }),
        remaining,
        |_| Err("unexpected startup network".into()),
    )?;
    if let Some(error) = initialized["browser_error"].as_str() {
        return Err(error.to_owned());
    }
    Ok(session)
}

/// Start-time dependency probe. Resolve without evaluating, validate the entry's trust boundary,
/// then require it to discover Chromium (honoring PLAYWRIGHT_BROWSERS_PATH when set).
fn probe(
    ctx: &ToolContext,
    trusted: &Path,
    config: &BrowserConfig,
    runtime: &runtime::Runtime,
    pid: &Arc<AtomicI32>,
    interrupt: &AtomicBool,
    end: Option<Instant>,
) -> Result<(PathBuf, Value), String> {
    let builtin = config.playwright_module.trim().is_empty();
    let module = if builtin {
        runtime.entry.as_os_str()
    } else {
        std::ffi::OsStr::new(&config.playwright_module)
    };
    let mut command = Command::new(trusted);
    command
        .args(["-e", PREFLIGHT])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("DIAL_PLAYWRIGHT", module)
        .current_dir(ctx.workspace.root());
    if builtin {
        command.env("PLAYWRIGHT_BROWSERS_PATH", &runtime.browsers);
    }
    let mut process = JsonProcess::start(command, pid.clone()).map_err(|e| {
        format!(
            "cannot start browser.node {:?}; install Node.js: {e}",
            trusted
        )
    })?;
    if interrupt.load(Ordering::Acquire) {
        return Err("browser dependency discovery cancelled".into());
    }
    let info = process.request(json!({}), remaining_timeout(end)?, |_| {
        Err("unexpected dependency callback".into())
    })?;
    let entry = PathBuf::from(info["entry"].as_str().ok_or("Playwright entry missing")?);
    let entry = if builtin {
        entry
            .canonicalize()
            .map_err(|e| format!("browser runtime entry: {e}"))?
    } else {
        operator_dependency(ctx.workspace.root(), &entry)?
    };
    let meta = process.request(json!({"entry": entry}), remaining_timeout(end)?, |_| {
        Err("unexpected dependency callback".into())
    })?;
    Ok((entry, meta))
}

fn chromium_path(config: &BrowserConfig, meta: &Value) -> Result<PathBuf, String> {
    config
        .chromium_executable
        .as_deref()
        .or(meta["executable"].as_str())
        .map(PathBuf::from)
        .ok_or_else(|| "Chromium executable missing".to_owned())
}

impl BrowserSession {
    /// Save the storage-state profile when one is configured. Errors are returned, never ignored by shutdown.
    fn save_profile(&mut self) -> Result<(), String> {
        let Some(path) = self.profile.clone() else {
            return Ok(());
        };
        let state =
            self.process
                .request(json!({"action":"storage_state"}), SAVE_TIMEOUT, |_| {
                    Err("unexpected storage callback".into())
                })?;
        if let Some(error) = state["browser_error"].as_str() {
            return Err(error.to_owned());
        }
        profile::save(&path, &state["storage_state"])
    }
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        orphans::forget(&self.orphan_dir, std::process::id() as i32, self.driver_pid);
    }
}

/// Close the session: save its profile first, then drop the process group.
fn retire(slot: &mut Option<BrowserSession>) -> Result<(), String> {
    let Some(mut session) = slot.take() else {
        return Ok(());
    };
    session.save_profile()
}

#[cfg(target_os = "macos")]
fn driver_command(
    ctx: &ToolContext,
    node: &Path,
    reads: &[PathBuf],
    state: &Path,
    network: bool,
) -> Result<Command, String> {
    let mut policy = sandbox_profile(
        ctx.workspace.root(),
        ctx.workspace.additional_roots(),
        network,
    )?;
    let mut subpaths = String::new();
    for path in reads {
        subpaths.push_str(&format!(" (subpath {})", profile_path(path)?));
    }
    policy.push_str(&format!(
        "(allow file-read*{subpaths})\n(allow file-read* file-write* (subpath {}))\n",
        profile_path(state)?
    ));
    let mut metadata: Vec<&Path> = reads.iter().map(PathBuf::as_path).collect();
    metadata.push(node);
    metadata.push(state);
    policy.push_str(&dependency_metadata(&metadata)?);
    policy.push_str(&state_unix_ipc(state)?);
    policy.push_str("(allow iokit-open (iokit-user-client-class \"RootDomainUserClient\") (iokit-user-client-class \"IOSurfaceRootUserClient\") (iokit-user-client-class \"IOSurfaceSendRight\"))\n");
    policy.push_str("(allow mach-register (global-name-regex #\"(^|\\.)MachPortRendezvousServer\\.[0-9]+$\"))\n");
    policy.push_str("(allow mach-register (global-name-regex #\"(^|\\.)apps\\.[A-F0-9]+$\"))\n");
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command
        .args(["-p", &policy])
        .arg(node)
        .args(["-e"])
        .arg(driver_source());
    Ok(command)
}

#[cfg(target_os = "linux")]
fn driver_command(
    _ctx: &ToolContext,
    node: &Path,
    reads: &[PathBuf],
    state: &Path,
    network: bool,
) -> Result<Command, String> {
    let writable = [state.to_path_buf()];
    let mut read_only = bwrap::system_read_roots();
    for path in reads {
        if !read_only.contains(path) {
            read_only.push(path.clone());
        }
    }
    let plan = bwrap::Plan {
        read_only: &read_only,
        writable: &writable,
        network,
        cwd: state,
    };
    bwrap::command(
        &plan,
        node,
        &[OsString::from("-e"), OsString::from(driver_source())],
    )
}

#[cfg(windows)]
fn driver_command(
    _ctx: &ToolContext,
    node: &Path,
    _reads: &[PathBuf],
    _state: &Path,
    _network: bool,
) -> Result<Command, String> {
    let mut command = Command::new(node);
    command.arg("-e").arg(driver_source());
    Ok(command)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn driver_command(
    _ctx: &ToolContext,
    _node: &Path,
    _reads: &[PathBuf],
    _state: &Path,
    _network: bool,
) -> Result<Command, String> {
    Err("browser requires macOS, Linux, or Windows with sandbox.mode = off".into())
}

#[derive(Debug)]
struct BrowserResolver {
    allowed_hosts: Vec<String>,
    interrupt: Arc<AtomicBool>,
}
impl BrowserResolver {
    fn check(&self, uri: &ureq::http::Uri, ip: std::net::IpAddr) -> Option<&'static str> {
        let reason = blocked_reason(ip)?;
        let host = uri.host().unwrap_or("");
        let port = uri
            .port_u16()
            .unwrap_or(if uri.scheme_str() == Some("https") {
                443
            } else {
                80
            });
        let permitted = self.allowed_hosts.iter().any(|entry| {
            entry
                .rsplit_once(':')
                .is_some_and(|(allowed_host, allowed_port)| {
                    allowed_host == host && allowed_port.parse::<u16>().ok() == Some(port)
                })
        });
        if permitted && matches!(reason, "private" | "unique local (private)") {
            return None;
        }
        if permitted
            && reason == "loopback"
            && matches!(host, "localhost" | "127.0.0.1")
            && ip.is_loopback()
        {
            return None;
        }
        Some(reason)
    }
}
impl Resolver for BrowserResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        if self.interrupt.load(Ordering::Acquire) {
            return Err(ureq::Error::Other(Box::new(crate::llm::Interrupted)));
        }
        let addresses = DefaultResolver::default().resolve(uri, config, timeout)?;
        if self.interrupt.load(Ordering::Acquire) {
            return Err(ureq::Error::Other(Box::new(crate::llm::Interrupted)));
        }
        for address in addresses.iter() {
            if let Some(reason) = self.check(uri, address.ip()) {
                return Err(ureq::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing browser request to {}: {reason} address ({})",
                        uri.host().unwrap_or("?"),
                        address.ip()
                    ),
                )));
            }
        }
        Ok(addresses)
    }
}

fn bridge(
    event: &Value,
    timeout: Duration,
    allowed_hosts: &[String],
    interrupt: Arc<AtomicBool>,
) -> Result<Value, String> {
    if interrupt.load(Ordering::Acquire) {
        return Err("browser request cancelled".into());
    }
    let timeout = match event.get("expires_at_ms").and_then(Value::as_u64) {
        Some(expiry) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_millis();
            let remaining = u128::from(expiry).saturating_sub(now);
            if remaining == 0 {
                return Err("browser request expired".into());
            }
            if timeout.is_zero() {
                Duration::from_millis(remaining.min(u128::from(u64::MAX)) as u64)
            } else {
                timeout.min(Duration::from_millis(
                    remaining.min(u128::from(u64::MAX)) as u64
                ))
            }
        }
        None => timeout,
    };
    if event.get("callback").and_then(Value::as_str) != Some("http") {
        return Err("unsupported browser callback".into());
    }
    let uri = parse_url(event["url"].as_str().ok_or("request URL missing")?)?;
    let method = event["method"].as_str().ok_or("HTTP method missing")?;
    let data = STANDARD
        .decode(event.get("body").and_then(Value::as_str).unwrap_or(""))
        .map_err(|e| e.to_string())?;
    if data.len() > 1024 * 1024 {
        return Err("browser request body exceeds 1 MiB".into());
    }
    // Resolve/connect/write have short explicit phase bounds; header/body/TLS waits poll cancellation below TLS.
    let phase = if timeout.is_zero() {
        Duration::from_millis(500)
    } else {
        timeout.min(Duration::from_millis(500))
    };
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .proxy(None)
        .timeout_global((!timeout.is_zero()).then_some(timeout))
        .timeout_resolve(Some(phase))
        .timeout_connect(Some(phase))
        .timeout_send_request(Some(phase))
        .build();
    let agent = crate::llm::cancellable_agent_with_resolver(
        config,
        interrupt.clone(),
        BrowserResolver {
            allowed_hosts: allowed_hosts.to_vec(),
            interrupt,
        },
    );
    let mut request = ureq::http::Request::builder().method(method).uri(uri);
    if let Some(headers) = event["headers"].as_object() {
        for (name, value) in headers {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "host"
                    | "connection"
                    | "content-length"
                    | "accept-encoding"
                    | "proxy-authorization"
                    | "proxy-connection"
            ) {
                continue;
            }
            if let Some(value) = value.as_str() {
                request = request.header(name, value);
            }
        }
    }
    let request = request
        .body(data.as_slice())
        .map_err(|e| format!("browser HTTP request: {e}"))?;
    let mut response = agent
        .run(request)
        .map_err(|e| format!("browser guarded HTTP request: {e}"))?;
    let status = response.status().as_u16();
    let mut headers = serde_json::Map::new();
    for (name, value) in response.headers() {
        if matches!(
            name.as_str(),
            "content-length" | "content-encoding" | "transfer-encoding" | "connection"
        ) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            let key = name.as_str();
            if let Some(existing) = headers.get_mut(key) {
                if let serde_json::Value::Array(values) = existing {
                    values.push(json!(value));
                } else {
                    let first = std::mem::replace(existing, serde_json::Value::Null);
                    *existing = json!([first, value]);
                }
            } else {
                headers.insert(key.into(), json!(value));
            }
        }
    }
    let mut body = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|e| format!("browser response body: {e}"))?;
    if body.len() > MAX_BODY {
        return Err("browser response exceeds 4 MiB".into());
    }
    Ok(json!({"status":status,"headers":headers,"body":STANDARD.encode(body)}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    #[test]
    fn sandbox_dependency_paths_are_not_profile_injection() {
        assert!(profile_path(Path::new("/tmp/a\"(allow network*)")).is_err());
        assert!(profile_path(Path::new("relative")).is_err());
        assert_eq!(profile_path(Path::new("/tmp/ok")).unwrap(), "\"/tmp/ok\"");
    }
    #[test]
    fn dependency_ancestors_only_receive_literal_metadata_permission() {
        let rules = dependency_metadata(&[
            Path::new("/private/tmp/operator/node_modules"),
            Path::new("/Users/operator/chromium/bin/chrome"),
        ])
        .unwrap();
        assert!(rules.starts_with("(allow file-read-metadata"));
        assert!(rules.contains("(literal \"/private/tmp\")"));
        assert!(rules.contains("(literal \"/Users/operator\")"));
        assert!(
            !rules.contains("file-read-data")
                && !rules.contains("file-read*")
                && !rules.contains("subpath")
        );
    }
    #[test]
    fn browser_ipc_permission_is_restricted_to_private_state_unix_sockets() {
        let rules = state_unix_ipc(Path::new("/private/tmp/dial-browser-owned")).unwrap();
        assert!(rules.contains("local unix-socket") && rules.contains("remote unix-socket"));
        assert!(rules.contains("^/private/tmp/dial\\-browser\\-owned/"));
        assert!(
            !rules.contains("tcp") && !rules.contains("udp") && !rules.contains("(allow network*)")
        );
    }
    #[test]
    fn trusted_browser_read_scope_includes_app_bundle_without_parent_directory_contents() {
        assert_eq!(
            chromium_bundle_root(Path::new(
                "/Users/operator/cache/Chrome.app/Contents/MacOS/Chrome"
            ))
            .unwrap(),
            Path::new("/Users/operator/cache/Chrome.app")
        );
        assert_eq!(
            chromium_bundle_root(Path::new("/opt/chromium/chrome")).unwrap(),
            Path::new("/opt/chromium")
        );
    }
    #[test]
    fn bridge_refuses_private_addresses_and_non_http_schemes() {
        for url in [
            "http://127.0.0.1/",
            "http://169.254.169.254/",
            "file:///etc/passwd",
        ] {
            let result = bridge(
                &json!({"callback":"http","url":url,"method":"GET","headers":{},"body":""}),
                Duration::from_secs(2),
                &[],
                Arc::new(AtomicBool::new(false)),
            );
            assert!(result.is_err(), "{url}");
        }
    }
    #[test]
    fn bridge_rejects_oversize_upload_before_network() {
        let body = STANDARD.encode(vec![0u8; 1024 * 1024 + 1]);
        let error = bridge(
            &json!({"callback":"http","url":"https://example.com/","method":"POST","body":body}),
            Duration::from_secs(1),
            &[],
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
        assert!(error.contains("1 MiB"));
    }
    #[test]
    fn every_browser_action_requires_approval_and_policy_applies_before_startup() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = super::super::Workspace::new(dir.path()).unwrap();
        let mut config = Config::default();
        config.sandbox.mode = SandboxMode::Strict;
        let tool = tools(&config).pop().unwrap();
        for action in [
            "open",
            "evaluate",
            "evidence",
            "screenshot",
            "close",
            "shutdown",
        ] {
            assert!(tool.needs_approval(&json!({"action":action})));
        }
        let output = tool.call(
            &ToolContext {
                workspace: &workspace,
                config: &config,
                mutation_authorizer: None,
                confirm: None,
            },
            &json!({"action":"open","url":"https://example.com/"}),
        );
        assert!(output.is_error && output.text.contains("network access is disabled"));
    }
    #[test]
    fn approved_loopback_grants_are_exact_bounded_and_never_come_from_page_actions() {
        let config = BrowserConfig::default();
        let mut hosts = Vec::new();
        permit_requested_localhost(
            &mut hosts,
            &config,
            &json!({"action":"open", "url":"http://127.0.0.1:3000/studio/porting-windowing"}),
        )
        .unwrap();
        assert_eq!(hosts, ["127.0.0.1:3000"]);
        for (action, url) in [
            ("evaluate", "http://127.0.0.1:4000/"),
            ("click", "http://localhost:3000/"),
            ("open", "http://evil.example:3000/"),
            ("navigate", "http://169.254.169.254/"),
            ("open", "http://10.1.2.3:3000/"),
        ] {
            permit_requested_localhost(&mut hosts, &config, &json!({"action":action, "url":url}))
                .unwrap();
        }
        let resolver = BrowserResolver {
            allowed_hosts: hosts.clone(),
            interrupt: Arc::new(AtomicBool::new(false)),
        };
        let check =
            |url: &str, ip: &str| resolver.check(&parse_url(url).unwrap(), ip.parse().unwrap());
        assert_eq!(check("http://127.0.0.1:3000/", "127.0.0.1"), None);
        for (url, ip) in [
            ("http://127.0.0.1:4000/", "127.0.0.1"),
            ("http://localhost:3000/", "127.0.0.1"),
            ("http://evil.example:3000/", "127.0.0.1"),
            ("http://169.254.169.254/", "169.254.169.254"),
            ("http://127.0.0.1:3000/", "169.254.169.254"),
            ("http://10.1.2.3:3000/", "10.1.2.3"),
        ] {
            assert!(check(url, ip).is_some(), "{url} via {ip}");
        }
        assert_eq!(hosts, ["127.0.0.1:3000"]);
        permit_requested_localhost(
            &mut hosts,
            &config,
            &json!({"action":"navigate", "url":"http://localhost:4000/"}),
        )
        .unwrap();
        assert_eq!(hosts, ["127.0.0.1:3000", "localhost:4000"]);
        let strict = BrowserConfig {
            auto_localhost: false,
            ..Default::default()
        };
        let mut denied = Vec::new();
        permit_requested_localhost(
            &mut denied,
            &strict,
            &json!({"action":"open", "url":"http://localhost:3000/"}),
        )
        .unwrap();
        assert!(denied.is_empty());
        let mut full = vec!["already:80".into(); 256];
        assert!(
            permit_requested_localhost(
                &mut full,
                &config,
                &json!({"action":"open", "url":"http://localhost:3000/"})
            )
            .is_err()
        );
        assert!(
            permit_requested_localhost(
                &mut hosts,
                &config,
                &json!({"action":"open", "url":"http://localhost:0/"})
            )
            .is_err()
        );
        assert!(
            permit_requested_localhost(
                &mut hosts,
                &config,
                &json!({"action":"open", "url":"http://localhost:65536/"})
            )
            .is_err()
        );
    }

    #[test]
    fn operator_allowlist_is_exact_and_never_allows_metadata() {
        let resolver = BrowserResolver {
            allowed_hosts: vec![
                "127.0.0.1:8080".into(),
                "localhost:8080".into(),
                "169.254.169.254:80".into(),
                "10.1.2.3:8080".into(),
                "evil.example:8080".into(),
            ],
            interrupt: Arc::new(AtomicBool::new(false)),
        };
        let check = |url: &str, address: &str| {
            resolver.check(&parse_url(url).unwrap(), address.parse().unwrap())
        };
        assert_eq!(check("http://127.0.0.1:8080/", "127.0.0.1"), None);
        assert_eq!(check("http://localhost:8080/", "::1"), None);
        assert!(check("http://127.0.0.1:8081/", "127.0.0.1").is_some());
        assert!(check("http://evil.example:8080/", "127.0.0.1").is_some());
        assert!(check("http://169.254.169.254/", "169.254.169.254").is_some());
        assert_eq!(check("http://10.1.2.3:8080/", "10.1.2.3"), None);
        assert!(check("http://10.1.2.3:8080/", "169.254.169.254").is_some());
        let mut config = BrowserConfig::default();
        for invalid in [
            "localhost",
            "localhost:0",
            "*.example:80",
            "http://localhost:80",
            "localhost:80/path",
            "LOCALHOST:80",
        ] {
            config.allowed_hosts = vec![invalid.into()];
            assert!(config.validate_allowed_hosts().is_err(), "{invalid}");
        }
    }
    #[test]
    fn workspace_playwright_is_resolved_without_executing_it_then_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let escaped = outside.path().join("preflight-escaped");
        let module = dir.path().join("node_modules/playwright");
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(
            module.join("package.json"),
            r#"{"name":"playwright","main":"index.js"}"#,
        )
        .unwrap();
        std::fs::write(
            module.join("index.js"),
            format!(
                "require('node:fs').writeFileSync({}, 'escaped'); throw Error('executed');",
                serde_json::to_string(&escaped).unwrap()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                module.join("index.js"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        let workspace = super::super::Workspace::new(dir.path()).unwrap();
        let config = Config::default();
        let ctx = ToolContext {
            workspace: &workspace,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        };
        let root = workspace.root();
        let node = resolve_operator_executable(root, "node", "browser.node").unwrap();
        let trusted_module = outside.path().join("trusted.cjs");
        std::fs::write(
            &trusted_module,
            "exports.chromium = { executablePath: () => (process.env.PLAYWRIGHT_BROWSERS_PATH || 'external') + '/chromium' };",
        )
        .unwrap();
        let runtime = runtime::Runtime {
            root: outside.path().to_path_buf(),
            entry: trusted_module.clone(),
            cli: outside.path().join("cli.js"),
            readability: outside.path().join("Readability.js"),
            browsers: outside.path().join("browsers"),
        };
        let check = |module: &str| {
            let browser_config = BrowserConfig {
                playwright_module: module.into(),
                ..Default::default()
            };
            probe(
                &ctx,
                &node,
                &browser_config,
                &runtime,
                &Arc::new(AtomicI32::new(0)),
                &AtomicBool::new(false),
                Some(Instant::now() + Duration::from_secs(5)),
            )
        };
        assert!(
            check(module.to_str().unwrap())
                .unwrap_err()
                .contains("workspace-owned")
        );
        assert!(!escaped.exists());
        // An operator-owned module completes both requests, proving this uses the production probe.
        let (_, meta) = check(trusted_module.to_str().unwrap()).unwrap();
        assert_eq!(meta["executable"], "external/chromium");
        // The built-in cache still completes the second request with its private browser path.
        let (entry, meta) = check("").unwrap();
        assert_eq!(entry, trusted_module.canonicalize().unwrap());
        assert_eq!(
            meta["executable"],
            format!("{}/chromium", runtime.browsers.display())
        );
        #[cfg(unix)]
        {
            let symlink = outside.path().join("symlink.js");
            std::os::unix::fs::symlink(module.join("index.js"), &symlink).unwrap();
            assert!(
                check(symlink.to_str().unwrap())
                    .unwrap_err()
                    .contains("workspace-owned")
            );
            assert!(!escaped.exists());
            assert!(
                resolve_operator_executable(
                    root,
                    module.join("index.js").to_str().unwrap(),
                    "browser.node"
                )
                .unwrap_err()
                .contains("outside")
            );
            assert!(
                resolve_operator_executable(root, symlink.to_str().unwrap(), "browser.node")
                    .is_err()
            );
        }
    }
    #[test]
    fn guarded_http_cancellation_interrupts_stalled_headers_and_body_and_closes_socket() {
        use std::io::Write;
        for send_headers in [false, true] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (closed_tx, closed_rx) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut buffer = [0u8; 4096];
                let _ = socket.read(&mut buffer).unwrap();
                if send_headers {
                    socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                }
                ready_tx.send(()).unwrap();
                let closed = loop {
                    match socket.read(&mut buffer) {
                        Ok(0) => break true,
                        Ok(_) => {}
                        // Linux may reset a connection when cancellation drops
                        // unread response bytes. RST proves closure just as EOF does;
                        // a read timeout still fails this leak check.
                        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {
                            break true;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break false,
                    }
                };
                closed_tx.send(closed).unwrap();
            });
            let token = Arc::new(AtomicBool::new(false));
            let running = token.clone();
            let worker = std::thread::spawn(move || {
                bridge(
                    &json!({"callback":"http","url":format!("http://127.0.0.1:{port}/"),"method":"GET","headers":{}}),
                    Duration::from_secs(3600),
                    &[format!("127.0.0.1:{port}")],
                    running,
                )
            });
            ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let start = Instant::now();
            token.store(true, Ordering::Release);
            assert!(worker.join().unwrap().is_err());
            assert!(start.elapsed() < Duration::from_secs(1));
            assert!(closed_rx.recv_timeout(Duration::from_secs(2)).unwrap());
            server.join().unwrap();
        }
    }
    struct LocalApp {
        port: u16,
        stop: Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl LocalApp {
        fn start() -> Self {
            Self::start_with_header_poll(Duration::from_millis(25))
        }

        fn start_with_header_poll(header_poll: Duration) -> Self {
            use std::io::Write;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stopping = stop.clone();
            let thread = std::thread::spawn(move || {
                // Browsers may open idle preconnect sockets. Serve accepted connections independently
                // so one empty socket cannot prevent the guarded client from sending its real request.
                std::thread::scope(|scope| {
                    while !stopping.load(Ordering::Acquire) {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                let stopping = &stopping;
                                scope.spawn(move || {
                            stream
                                .set_read_timeout(Some(header_poll))
                                .unwrap();
                            // TCP can split even the request line. Wait for a complete bounded
                            // header before routing or replying; an early close also races the
                            // guarded client's remaining header writes.
                            let mut request = Vec::new();
                            let mut chunk = [0u8; 4096];
                            while request.len() < 16 * 1024
                                && !stopping.load(Ordering::Acquire)
                                && !request.windows(4).any(|bytes| bytes == b"\r\n\r\n")
                            {
                                match stream.read(&mut chunk) {
                                    Ok(0) => break,
                                    Ok(count) => request.extend_from_slice(&chunk[..count]),
                                    Err(error)
                                        if error.kind() == std::io::ErrorKind::Interrupted =>
                                    {
                                        continue;
                                    }
                                    Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                                        // A preconnect or partial request can stay idle legitimately. This
                                        // timeout only polls fixture shutdown; it is not a header deadline.
                                        continue;
                                    }
                                    Err(error) => {
                                        eprintln!(
                                            "local browser fixture header read failed after {} bytes: {error}",
                                            request.len()
                                        );
                                        break;
                                    }
                                }
                            }
                            if !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                                return;
                            }
                            let response = if String::from_utf8_lossy(&request)
                                .starts_with("GET /redirect ")
                            {
                                format!(
                                    "HTTP/1.1 302 Found\r\nLocation: http://localhost:{port}/blocked\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                )
                            } else {
                                let article =
                                    String::from_utf8_lossy(&request).starts_with("GET /article ");
                                let body = if article {
                                    "<!doctype html><title>Dial article</title><article><h1>Dial article</h1><p>Local browser readable extraction keeps the substantive paragraphs of this fixture page, which is long enough for Readability to score it as an article rather than navigation chrome.</p><p>A second paragraph repeats the claim with more ordinary prose so the extracted text is clearly article content from the local fixture server.</p><p>A third paragraph adds further descriptive sentences about the local test page so the readable extraction has a stable body of text to return to the caller.</p></article>"
                                } else {
                                    "<!doctype html><title>Dial local smoke</title><input id='name'><button id='go' onclick=\"document.querySelector('#result').textContent=document.querySelector('#name').value;console.log('verified-click')\">Go</button><p id='result'></p>"
                                };
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                )
                            };
                            if let Err(error) = stream.write_all(response.as_bytes()) {
                                eprintln!("local browser fixture response write failed: {error}");
                            }
                            });
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(5))
                            }
                            Err(e) => panic!("local browser fixture: {e}"),
                        }
                    }
                });
            });
            Self {
                port,
                stop,
                thread: Some(thread),
            }
        }
    }
    impl Drop for LocalApp {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
    #[test]
    fn local_http_fixture_serves_requests_while_a_preconnect_socket_is_idle() {
        use std::io::{Read, Write};
        let app = LocalApp::start();
        let idle = std::net::TcpStream::connect(("127.0.0.1", app.port)).unwrap();
        let mut client = std::net::TcpStream::connect(("127.0.0.1", app.port)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        client
            .write_all(b"GET /first HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("Dial local smoke"));
        drop(idle);
    }

    #[test]
    fn local_http_fixture_preserves_delayed_headers_and_closes_idle_connections_on_shutdown() {
        use std::io::{Read, Write};
        let app = LocalApp::start_with_header_poll(Duration::from_millis(5));
        let mut client = std::net::TcpStream::connect(("127.0.0.1", app.port)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        client.write_all(b"GET /fi").unwrap();
        let error = client
            .read(&mut [0u8; 1])
            .expect_err("partial headers cannot receive a response or be closed");
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ));
        client
            .write_all(b"rst HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        let idle = std::net::TcpStream::connect(("127.0.0.1", app.port)).unwrap();
        let (closed, shutdown) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || {
            drop(app);
            closed.send(()).unwrap();
        });
        shutdown
            .recv_timeout(Duration::from_secs(2))
            .expect("fixture shutdown must not wait for idle clients to send headers");
        drop(idle);
        join.join().unwrap();
    }
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires operator-installed Node/Playwright/Chromium and sandbox-exec; no public internet needed"]
    fn chromium_approved_localhost_without_manual_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        let app = LocalApp::start();
        let workspace = super::super::Workspace::new(dir.path()).unwrap();
        let mut config = Config::default();
        config.sandbox.mode = SandboxMode::Dev;
        config.sandbox.allow_network = true;
        config.browser.playwright_module = std::env::var("DIAL_PLAYWRIGHT").unwrap();
        assert!(config.browser.allowed_hosts.is_empty());
        let tool = tools(&config).pop().unwrap();
        let ctx = ToolContext {
            workspace: &workspace,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        };
        let blank = tool.call(&ctx, &json!({"action":"open", "url":"about:blank"}));
        assert!(!blank.is_error, "{}", blank.text);
        let blank: Value = serde_json::from_str(&blank.text).unwrap();
        assert_eq!(blank["url"], "about:blank");
        let evaluated = tool.call(
            &ctx,
            &json!({"action":"evaluate", "expression":"document.URL"}),
        );
        assert!(
            !evaluated.is_error && evaluated.text.contains("about:blank"),
            "{}",
            evaluated.text
        );
        let closed = tool.call(&ctx, &json!({"action":"close", "tab":blank["tab"]}));
        assert!(!closed.is_error, "{}", closed.text);
        // A managed Node preview must survive start/wait and other tool calls until explicitly
        // stopped. No Nuxt/package download or user project is involved in this lifecycle check.
        std::fs::write(dir.path().join("preview.cjs"), "const http=require('node:http'); const server=http.createServer((req,res)=>{res.writeHead(200,{'content-type':'text/html'});res.end('<title>Managed preview</title><p>Preview stays running between tools</p>')});server.listen(0,'127.0.0.1',()=>console.log('READY http://127.0.0.1:'+server.address().port+'/')); ").unwrap();
        let policy = super::super::sandbox::Policy::Dev {
            deny_read: Vec::new(),
            extra_write: Vec::new(),
            allow_network: true,
        };
        let sandbox = super::super::sandbox::Sandbox::probe_policy(dir.path(), &policy);
        sandbox.available().unwrap();
        let preview_workspace = workspace.clone().with_policy(policy).with_sandbox(sandbox);
        let preview_ctx = ToolContext {
            workspace: &preview_workspace,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        };
        let bg = super::super::background::tools(&config).pop().unwrap();
        let started = bg.call(
            &preview_ctx,
            &json!({"action":"start","name":"preview","command":"node preview.cjs"}),
        );
        assert!(!started.is_error, "{}", started.text);
        let ready = bg.call(&preview_ctx, &json!({"action":"wait","name":"preview","until":"READY http://127\\.0\\.0\\.1:","timeout":10}));
        assert!(
            !ready.is_error && ready.text.starts_with("ready:"),
            "{}",
            ready.text
        );
        let preview_url = ready
            .text
            .lines()
            .find_map(|line| line.strip_prefix("> READY "))
            .unwrap();
        let running = bg.call(&preview_ctx, &json!({"action":"list"}));
        assert!(running.text.contains("1 running"), "{}", running.text);
        let page = tool.call(&ctx, &json!({"action":"open","url":preview_url}));
        assert!(!page.is_error, "{}", page.text);
        let page: Value = serde_json::from_str(&page.text).unwrap();
        assert_eq!(page["status"], 200);
        assert_eq!(page["title"], "Managed preview");
        let pixels = tool.call(&ctx, &json!({"action":"screenshot"}));
        assert!(
            !pixels.is_error && pixels.text.contains("image/png"),
            "{}",
            pixels.text
        );
        let killed = bg.call(&preview_ctx, &json!({"action":"kill","name":"preview"}));
        assert!(!killed.is_error, "{}", killed.text);
        let stopped = tool.call(&ctx, &json!({"action":"navigate","url":preview_url}));
        assert!(
            stopped.is_error && stopped.text.contains("Connection refused"),
            "{}",
            stopped.text
        );
        // A failed request closes the session, so the next open must recover from scratch.
        assert!(!tool.call(&ctx, &json!({"action":"shutdown"})).is_error);
        let url = format!("http://127.0.0.1:{}/studio/porting-windowing", app.port);
        let opened = tool.call(&ctx, &json!({"action":"open", "url":url}));
        assert!(!opened.is_error, "{}", opened.text);
        let result: Value = serde_json::from_str(&opened.text).unwrap();
        assert_eq!(result["status"], 200);
        assert_eq!(result["title"], "Dial local smoke");
        let tab = result["tab"].as_u64().unwrap();
        let screenshot = tool.call(&ctx, &json!({"action":"screenshot", "tab":tab}));
        assert!(!screenshot.is_error, "{}", screenshot.text);
        let screenshot: Value = serde_json::from_str(&screenshot.text).unwrap();
        let png = STANDARD
            .decode(screenshot["artifact"]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        if let Ok(path) = std::env::var("DIAL_TEST_BROWSER_SCREENSHOT") {
            std::fs::write(path, &png).unwrap();
        }
        // An unapproved localhost alias in a redirect must still be blocked.
        let rejected = tool.call(&ctx, &json!({"action":"navigate", "tab":tab, "url":format!("http://127.0.0.1:{}/redirect", app.port)}));
        assert!(
            rejected.is_error && rejected.text.contains("loopback"),
            "{}",
            rejected.text
        );
        tool.call(&ctx, &json!({"action":"shutdown"}));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires operator-installed Node/Playwright/Chromium and sandbox-exec; no public internet needed"]
    fn chromium_end_to_end_sandboxed() {
        let dir = tempfile::tempdir().unwrap();
        let app = LocalApp::start();
        let workspace = super::super::Workspace::new(dir.path()).unwrap();
        let mut config = Config::default();
        config.sandbox.mode = SandboxMode::Dev;
        config.sandbox.allow_network = true;
        config.browser.auto_localhost = false;
        config.browser.allowed_hosts = vec![format!("127.0.0.1:{}", app.port)];
        if let Ok(module) = std::env::var("DIAL_PLAYWRIGHT") {
            config.browser.playwright_module = module;
        }
        let tool = tools(&config).pop().unwrap();
        let ctx = ToolContext {
            workspace: &workspace,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        };
        let call = |args: Value| {
            let output = tool.call(&ctx, &args);
            assert!(!output.is_error, "{}", output.text);
            serde_json::from_str::<Value>(&output.text).unwrap()
        };
        let opened =
            call(json!({"action":"open","url":format!("http://127.0.0.1:{}/first",app.port)}));
        let tab = opened["tab"].as_u64().unwrap();
        assert_eq!(opened["status"], 200);
        let navigated = call(
            json!({"action":"navigate","tab":tab,"url":format!("http://127.0.0.1:{}/actual",app.port)}),
        );
        assert_eq!(navigated["title"], "Dial local smoke");
        call(json!({"action":"fill","tab":tab,"selector":"#name","text":"verified"}));
        call(json!({"action":"click","tab":tab,"selector":"#go"}));
        assert_eq!(
            call(
                json!({"action":"evaluate","tab":tab,"expression":"document.querySelector('#result').textContent"})
            )["value"],
            "verified"
        );
        let evidence = call(json!({"action":"evidence","tab":tab}));
        assert!(
            evidence["console"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["text"] == "verified-click")
        );
        assert!(
            evidence["network"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["status"] == 200)
        );
        let screenshot = call(json!({"action":"screenshot","tab":tab}));
        let png = STANDARD
            .decode(screenshot["artifact"]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        call(json!({"action":"close","tab":tab}));
        assert_eq!(call(json!({"action":"tabs"}))["tabs"], json!([]));
        call(json!({"action":"shutdown"}));
        let rejected = tool.call(
            &ctx,
            &json!({"action":"open","url":format!("http://localhost:{}/blocked",app.port)}),
        );
        assert!(
            rejected.is_error && rejected.text.contains("loopback"),
            "{}",
            rejected.text
        );
        let redirect = tool.call(
            &ctx,
            &json!({"action":"open","url":format!("http://127.0.0.1:{}/redirect",app.port)}),
        );
        assert!(
            redirect.is_error && redirect.text.contains("loopback"),
            "{}",
            redirect.text
        );
        call(json!({"action":"shutdown"}));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn builtin_runtime_end_to_end_sandboxed() {
        // Reads PATH and HOME (bun, runtime cache); gh fakes mutate both under this lock.
        let _env = crate::tools::gh_common::test_support::PATH_LOCK.lock();
        if std::process::Command::new("bun")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping built-in browser test: bun is not installed");
            return;
        }
        if !super::runtime::is_cached() && !super::runtime::network_reachable() {
            eprintln!(
                "skipping built-in browser test: runtime is not cached and the npm registry or Playwright CDN is unreachable"
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let app = LocalApp::start();
        let workspace = super::super::Workspace::new(dir.path()).unwrap();
        let mut config = Config::default();
        config.sandbox.mode = SandboxMode::Dev;
        config.sandbox.allow_network = true;
        config.browser.auto_localhost = false;
        // Isolate from cmux/tern sessions on the host so this test always exercises Chromium.
        config.browser.cmux = false;
        config.browser.tern = false;
        config.browser.allowed_hosts = vec![format!("127.0.0.1:{}", app.port)];
        assert!(
            config.browser.playwright_module.trim().is_empty(),
            "the built-in runtime is the default"
        );
        let tool = tools(&config).pop().unwrap();
        let ctx = ToolContext {
            workspace: &workspace,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        };
        let call = |args: Value| {
            let output = tool.call(&ctx, &args);
            assert!(!output.is_error, "{}", output.text);
            serde_json::from_str::<Value>(&output.text).unwrap()
        };
        let opened =
            call(json!({"action":"open","url":format!("http://127.0.0.1:{}/first",app.port)}));
        let tab = opened["tab"].as_u64().unwrap();
        call(json!({"action":"type","tab":tab,"selector":"#name","text":"verified"}));
        call(json!({"action":"click","tab":tab,"selector":"#go"}));
        assert_eq!(
            call(
                json!({"action":"evaluate","tab":tab,"expression":"document.querySelector('#result').textContent"})
            )["value"],
            "verified"
        );
        call(
            json!({"action":"navigate","tab":tab,"url":format!("http://127.0.0.1:{}/article",app.port)}),
        );
        let article = call(json!({"action":"readable","tab":tab}));
        assert!(
            article["text"]
                .as_str()
                .unwrap()
                .contains("Local browser readable extraction"),
            "{article}"
        );
        let screenshot = call(json!({"action":"screenshot","tab":tab}));
        let png = STANDARD
            .decode(screenshot["artifact"]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        call(json!({"action":"close","tab":tab}));
        call(json!({"action":"shutdown"}));
    }
}
