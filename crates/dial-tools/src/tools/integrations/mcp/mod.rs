//! Model Context Protocol client (stdio, streamable HTTP, and legacy HTTP+SSE transports).
//!
//! Each `[mcp.<name>]` server in the config is started as a trusted operator program (unsandboxed,
//! like `[tools.*]`, with this process's environment minus likely credentials plus the entry's
//! `env`) or reached over HTTP, handshaken, and its tools offered to the model as
//! `mcp__<name>__<tool>`. The model can reach a server only through those tools; calls need
//! approval unless the entry trusts the tool's `annotations.readOnlyHint` (`trust_read_only_hint`).
//! Servers are connected concurrently
//! and a server that fails or is slow to start only yields a notice.
//!
//! Each connected server is a [`Session`]. When its transport drops, the session reconnects along
//! [`RECONNECT_LADDER_MS`]; calls made while a server is down reconnect it on demand, bounded by a
//! burst breaker ([`RECONNECT_BURST_LIMIT`] attempts per [`RECONNECT_BURST_WINDOW`]). Servers that
//! declare
//! `resources` or `prompts` also get `mcp__<name>__read_resource` and `mcp__<name>__get_prompt`,
//! which list when called without arguments. Resource and prompt listings are cached until the
//! server sends the matching list-changed notice.
//!
//! Remote servers that answer 401 can be signed in with OAuth (`/mcp login <name>`, see
//! [`login`]); the grant is stored per server and refreshed on rejection.
//!
//! Limits: the tool list is fixed at startup (a `tools/list_changed` notice does not change the
//! registry), `sampling` and `roots` are not offered, and servers are killed when the returned
//! tools are dropped.

pub mod catalog;
mod client;
mod fetch;
pub mod foreign;
mod github;
mod http;
mod inventory;
mod names;
mod npm;
mod oauth;
pub mod registry;
pub mod smithery;
pub use oauth::{login, logout};
mod tool;
use crate::config::types::{Config, McpServerConfig};
use crate::tools::integrations::ext::Tool;
use anyhow::{Context, Result, bail, ensure};
use client::{Client, Shared};
use names::Namer;
use parking_lot::{Mutex, MutexGuard};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tool::{McpSurface, McpTool, RemoteTool, SurfaceKind};

#[derive(Clone, Debug)]
pub enum Signal {
    Closed,
    ToolsChanged,
    PromptsChanged,
    ResourcesChanged,
    ResourceUpdated,
}

pub fn connect_extra(
    specs: &std::collections::BTreeMap<String, McpServerConfig>,
    root: &std::path::Path,
) -> McpLoad {
    let config = Config {
        project_root: root.to_path_buf(),
        mcp: specs.clone(),
        ..Config::default()
    };
    load(&config)
}

/// Budget for one server's whole startup: spawn, `initialize` and every `tools/list` page.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Guards against a server that paginates forever.
const MAX_PAGES: usize = 50;
const MAX_TOOLS_PER_SERVER: usize = 256;
/// Delays before each automatic reconnect after the transport dropped.
const RECONNECT_LADDER_MS: [u64; 4] = [500, 1000, 2000, 4000];
/// Automatic reconnect attempts allowed per [`RECONNECT_BURST_WINDOW`] before calls fail fast.
const RECONNECT_BURST_LIMIT: usize = 5;
const RECONNECT_BURST_WINDOW: Duration = Duration::from_secs(30);

/// The tools of every server that connected, plus one line per problem for the user.
#[derive(Default)]
pub struct McpLoad {
    pub tools: Vec<Arc<dyn Tool>>,
    pub notices: Vec<String>,
    /// `(server, instructions)` from each server's `initialize` result, for the system prompt.
    pub instructions: Vec<(String, String)>,
    /// Sessions of connected servers, for `/mcp resources|prompts|notifications`.
    pub sessions: Vec<Arc<Session>>,
}

impl McpLoad {
    /// A local, read-only view of configured servers and the actual runtime catalog.
    pub fn inventory_tool(&self, config: &Config) -> Arc<dyn Tool> {
        Arc::new(inventory::Inventory::new(config, self))
    }
}

/// One configured server: its current client plus what is needed to bring a new one up.
pub struct Session {
    name: String,
    server: McpServerConfig,
    root: PathBuf,
    user_dir: PathBuf,
    connect_timeout: Duration,
    client: Mutex<Arc<Client>>,
    /// Held while a connect runs, so calls made meanwhile wait for it instead of racing it.
    turn: Mutex<()>,
    /// Start times of automatic reconnect attempts inside the burst window.
    attempts: Mutex<Vec<Instant>>,
    listing: Mutex<Listing>,
}

/// Cached `resources/list` + templates and `prompts/list` results; `None` until fetched or
/// after the matching list-changed notice.
#[derive(Default)]
struct Listing {
    resources: Option<Value>,
    prompts: Option<Value>,
}

impl Session {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The server's `initialize` capabilities as last negotiated, without reconnecting.
    /// `Null` when the current connection never completed initialization.
    pub fn capabilities(&self) -> Value {
        self.current().capabilities()
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.server.timeout_secs)
    }

    /// The client as it is now, without reconnecting.
    pub(super) fn current(&self) -> Arc<Client> {
        self.client.lock().clone()
    }

    /// The live client. Waits for an in-flight connect, and reconnects on demand when the
    /// transport dropped and the burst breaker allows it.
    pub(super) fn client(self: &Arc<Self>) -> Result<Arc<Client>> {
        self.client_until(&|| false)
    }

    /// [`Session::client`] that gives up when `cancel` turns true, both while waiting for another
    /// connect and while reconnecting: a server that never answers `initialize` must not hold a
    /// tool call forever when no connect timeout is configured.
    pub(super) fn client_until(self: &Arc<Self>, cancel: &dyn Fn() -> bool) -> Result<Arc<Client>> {
        let _turn = lock_turn(&self.turn, cancel)?;
        let current = self.current();
        if !current.is_closed() {
            return Ok(current);
        }
        self.open_locked(cancel)
    }

    /// Automatic recovery after the transport dropped, along the reconnect ladder.
    fn recover(self: &Arc<Self>) {
        for delay in RECONNECT_LADDER_MS {
            thread::sleep(Duration::from_millis(delay));
            let _turn = self.turn.lock();
            if !self.current().is_closed() || self.open_locked(&|| false).is_ok() {
                return;
            }
        }
    }

    /// Opens a fresh client and starts listening to it. The caller holds `turn`.
    fn open_locked(self: &Arc<Self>, cancel: &dyn Fn() -> bool) -> Result<Arc<Client>> {
        let now = Instant::now();
        let mut attempts = self.attempts.lock();
        attempts.retain(|at| now.duration_since(*at) < RECONNECT_BURST_WINDOW);
        ensure!(
            attempts.len() < RECONNECT_BURST_LIMIT,
            "MCP server `{}` keeps disconnecting; retry later",
            self.name
        );
        attempts.push(now);
        let deadline = crate::tools::exec::kernel::execution_deadline(self.connect_timeout);
        let (client, signals) = open(
            &self.name,
            &self.server,
            &self.root,
            &self.user_dir,
            deadline,
            cancel,
        )?;
        let client = Arc::new(client);
        listen(self, signals)?;
        let old = std::mem::replace(&mut *self.client.lock(), client.clone());
        drop(old);
        *self.listing.lock() = Listing::default();
        Ok(client)
    }

    /// Resources and resource templates, from cache when the server has not announced a change.
    pub fn resources(self: &Arc<Self>) -> Result<Value> {
        let cached = self.listing.lock().resources.clone();
        if let Some(cached) = cached {
            return Ok(cached);
        }
        let client = self.client()?;
        let timeout = self.timeout();
        let listing = json!({
            "resources": list_all(&client, "resources/list", "resources", timeout)?,
            "templates": list_all(&client, "resources/templates/list", "resourceTemplates", timeout)?,
        });
        self.listing.lock().resources = Some(listing.clone());
        Ok(listing)
    }

    /// Prompts, from cache when the server has not announced a change.
    pub fn prompts(self: &Arc<Self>) -> Result<Value> {
        let cached = self.listing.lock().prompts.clone();
        if let Some(cached) = cached {
            return Ok(cached);
        }
        let client = self.client()?;
        let listing =
            json!({"prompts": list_all(&client, "prompts/list", "prompts", self.timeout())?});
        self.listing.lock().prompts = Some(listing.clone());
        Ok(listing)
    }

    /// Reads a resource. When the server can subscribe, the read also subscribes so later
    /// `notifications/resources/updated` reach this session; a failed subscription is returned
    /// as a note rather than failing the read.
    pub fn read_resource(
        self: &Arc<Self>,
        uri: &str,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(Value, Option<String>)> {
        let client = self.client_until(cancel)?;
        let timeout = self.timeout();
        let result =
            client.request_with("resources/read", json!({"uri": uri}), timeout, cancel, None)?;
        let note = if client.can_subscribe() {
            client
                .request_with(
                    "resources/subscribe",
                    json!({"uri": uri}),
                    timeout,
                    cancel,
                    None,
                )
                .err()
                .map(|error| format!("change notifications not enabled for {uri}: {error:#}"))
        } else {
            None
        };
        Ok((result, note))
    }

    pub fn get_prompt(
        self: &Arc<Self>,
        name: &str,
        arguments: Value,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Value> {
        let client = self.client_until(cancel)?;
        let mut params = json!({"name": name});
        if arguments.as_object().is_some_and(|args| !args.is_empty()) {
            params["arguments"] = arguments;
        }
        client.request_with("prompts/get", params, self.timeout(), cancel, None)
    }

    /// Drops the cached listing a notice invalidates. The tool list is not refreshed: the registry
    /// is built once at startup.
    fn on_signal(&self, signal: Signal) {
        let mut listing = self.listing.lock();
        match signal {
            Signal::ResourcesChanged | Signal::ResourceUpdated => listing.resources = None,
            Signal::PromptsChanged => listing.prompts = None,
            Signal::ToolsChanged | Signal::Closed => {}
        }
    }
}

/// Follows `nextCursor` pages of a list method and collects the `key` arrays.
fn list_all(client: &Client, method: &str, key: &str, timeout: Duration) -> Result<Vec<Value>> {
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let params = cursor
            .as_ref()
            .map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
        let page = client.request(method, params, timeout)?;
        let Some(entries) = page.get(key).and_then(Value::as_array) else {
            bail!("`{method}` returned no `{key}` array");
        };
        items.extend(entries.iter().cloned());
        cursor = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    Ok(items)
}

/// Routes a session's signals: list-changed and resource-updated notices invalidate its cache,
/// and a closed transport triggers recovery. The thread holds only a weak reference, so dropping
/// the session (its tools) stops the server.
fn listen(session: &Arc<Session>, signals: mpsc::Receiver<(u64, Signal)>) -> Result<()> {
    let weak: Weak<Session> = Arc::downgrade(session);
    thread::Builder::new()
        .name(format!("mcp-{}-signals", session.name))
        .spawn(move || {
            for (_, signal) in signals {
                let Some(session) = weak.upgrade() else {
                    return;
                };
                if matches!(signal, Signal::Closed) {
                    session.recover();
                    return;
                }
                session.on_signal(signal);
            }
        })
        .context("starting MCP signal listener")?;
    Ok(())
}

/// Connects every configured server. Spawns nothing when `config.mcp` is empty. Takes over a
/// [`prefetch`] started for the same servers, so startup only waits for what is still connecting.
pub fn load(config: &Config) -> McpLoad {
    let started = PREFETCH.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some((key, handle)) = started
        && Some(key) == prefetch_key(config)
        && let Ok(loaded) = handle.join()
    {
        return loaded;
    }
    load_with(config, CONNECT_TIMEOUT)
}

/// A connection pass started before the first agent exists, keyed by the servers it connects.
type Prefetch = (String, thread::JoinHandle<McpLoad>);
static PREFETCH: std::sync::Mutex<Option<Prefetch>> = std::sync::Mutex::new(None);

/// Everything `load_with` reads from the config; a prefetch is reused only for an equal key.
fn prefetch_key(config: &Config) -> Option<String> {
    serde_json::to_string(&(&config.mcp, &config.project_root, &config.user_dir)).ok()
}

/// Start connecting the configured servers in the background, so the interface can draw while
/// slow servers start; the next [`load`] for the same servers adopts the result. A config that
/// changes first discards the prefetch (dropping its sessions stops those servers).
pub fn prefetch(config: &Config) {
    if !config.mcp.values().any(|server| server.enabled) {
        return;
    }
    let Some(key) = prefetch_key(config) else {
        return;
    };
    let config = config.clone();
    let Ok(handle) = thread::Builder::new()
        .name("mcp-prefetch".into())
        .spawn(move || load_with(&config, CONNECT_TIMEOUT))
    else {
        return;
    };
    *PREFETCH.lock().unwrap_or_else(|e| e.into_inner()) = Some((key, handle));
}

/// Whether a [`prefetch`] is still connecting, so the interface can keep accepting input
/// instead of blocking in [`load`].
pub fn prefetch_pending() -> bool {
    PREFETCH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|(_, handle)| !handle.is_finished())
}

fn load_with(config: &Config, connect_timeout: Duration) -> McpLoad {
    let mut loaded = McpLoad::default();
    let attempts: Vec<_> = config
        .mcp
        .iter()
        .filter(|(_, server)| server.enabled)
        .map(|(name, server)| {
            let (thread_name, server_name, server, root, user_dir) = (
                format!("mcp-{name}-connect"),
                name.clone(),
                server.clone(),
                config.project_root.clone(),
                config.user_dir.clone(),
            );
            let handle = thread::Builder::new()
                .name(thread_name)
                .spawn(move || connect(&server_name, &server, &root, &user_dir, connect_timeout));
            (name, handle)
        })
        .collect();

    let mut namer = Namer::default();
    for (server, handle) in attempts {
        let outcome = match handle {
            Ok(handle) => handle
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("connection thread panicked"))),
            Err(error) => Err(error.into()),
        };
        match outcome {
            Ok(connected) => add_tools(&mut loaded, &mut namer, server, connected),
            Err(error) => loaded.notices.push(format!("mcp `{server}`: {error:#}")),
        }
    }
    loaded
}

/// Everything one connected server contributes to the registry.
struct Connected {
    session: Arc<Session>,
    tools: Vec<RemoteTool>,
    lifecycle_hooks: Vec<String>,
    notice: Option<String>,
    instructions: Option<String>,
    resources: bool,
    prompts: bool,
}

fn add_tools(loaded: &mut McpLoad, namer: &mut Namer, server: &str, connected: Connected) {
    let Connected {
        session,
        tools,
        lifecycle_hooks,
        notice,
        instructions,
        resources,
        prompts,
    } = connected;
    loaded.notices.extend(notice);
    if let Some(instructions) = instructions {
        loaded.instructions.push((server.to_owned(), instructions));
    }
    if tools.is_empty() && !resources && !prompts {
        loaded
            .notices
            .push(format!("mcp `{server}`: connected but offers no tools"));
        // Keep initialized sessions in the inventory even when they expose no callable surface.
    }
    loaded.sessions.push(session.clone());
    let timeout = session.timeout();
    for remote in tools {
        let name = namer.name(server, &remote.name);
        let tool = McpTool::with_lifecycle_hooks(
            name,
            server,
            remote,
            session.clone(),
            timeout,
            lifecycle_hooks.clone(),
        );
        loaded.tools.push(Arc::new(tool));
    }
    if resources {
        let name = namer.name(server, "read_resource");
        loaded.tools.push(Arc::new(McpSurface::new(
            name,
            server,
            SurfaceKind::Resource,
            session.clone(),
            lifecycle_hooks.clone(),
        )));
    }
    if prompts {
        let name = namer.name(server, "get_prompt");
        loaded.tools.push(Arc::new(McpSurface::new(
            name,
            server,
            SurfaceKind::Prompt,
            session,
            lifecycle_hooks,
        )));
    }
}

fn connect(
    name: &str,
    server: &McpServerConfig,
    root: &std::path::Path,
    user_dir: &Path,
    connect_timeout: Duration,
) -> Result<Connected> {
    let deadline = crate::tools::exec::kernel::execution_deadline(connect_timeout);
    let (client, signals) = open(name, server, root, user_dir, deadline, &|| false)?;
    let (resources, prompts) = (
        client.has_capability("resources"),
        client.has_capability("prompts"),
    );
    let instructions = client.instructions();
    let session = Arc::new(Session {
        name: name.to_owned(),
        server: server.clone(),
        root: root.to_path_buf(),
        user_dir: user_dir.to_path_buf(),
        connect_timeout,
        client: Mutex::new(Arc::new(client)),
        turn: Mutex::new(()),
        attempts: Mutex::new(Vec::new()),
        listing: Mutex::new(Listing::default()),
    });
    listen(&session, signals)?;

    let mut tools = Vec::new();
    let mut notice = None;
    let client = session.current();
    let budget = || remaining(deadline);
    let mut cursor: Option<String> = None;
    // A resource- or prompt-only server does not declare `tools`; probing `tools/list` would fail it.
    let serves_tools = client.has_capability("tools") || !(resources || prompts);
    let page_limit = if serves_tools { MAX_PAGES } else { 0 };
    for _ in 0..page_limit {
        let params = cursor
            .as_ref()
            .map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
        let page = client.request("tools/list", params, budget())?;
        let Some(entries) = page.get("tools").and_then(Value::as_array) else {
            bail!("`tools/list` returned no `tools` array");
        };
        tools.extend(
            entries
                .iter()
                .filter_map(RemoteTool::parse)
                .map(|mut tool| {
                    tool.read_only_hint &= server.trust_read_only_hint;
                    tool
                }),
        );
        cursor = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(str::to_owned);
        if cursor.is_none() || tools.len() >= MAX_TOOLS_PER_SERVER {
            break;
        }
    }
    let truncated = cursor.is_some() || tools.len() > MAX_TOOLS_PER_SERVER;
    tools.truncate(MAX_TOOLS_PER_SERVER);
    if truncated {
        notice = Some(format!(
            "mcp `{name}`: tool list cut off after {} tools",
            tools.len()
        ));
    }
    Ok(Connected {
        session,
        tools,
        lifecycle_hooks: server.lifecycle_hooks.clone(),
        notice,
        instructions,
        resources,
        prompts,
    })
}

/// How often a request waiting on another connect checks for cancellation.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Takes `turn`, polling `cancel` while another connect or reconnect holds it.
fn lock_turn<'a>(turn: &'a Mutex<()>, cancel: &dyn Fn() -> bool) -> Result<MutexGuard<'a, ()>> {
    loop {
        if let Some(guard) = turn.try_lock_for(CANCEL_POLL) {
            return Ok(guard);
        }
        ensure!(
            !cancel(),
            "waiting for the MCP server connection was cancelled"
        );
    }
}

/// Time left before `deadline`, at least a millisecond; zero (no deadline) waits indefinitely.
fn remaining(deadline: Option<Instant>) -> Duration {
    deadline.map_or(Duration::ZERO, |end| {
        end.saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1))
    })
}

/// Spawns or dials the server and performs `initialize`, returning the client and its signals.
fn open(
    name: &str,
    server: &McpServerConfig,
    root: &Path,
    user_dir: &Path,
    deadline: Option<Instant>,
    cancel: &dyn Fn() -> bool,
) -> Result<(Client, mpsc::Receiver<(u64, Signal)>)> {
    let (tx, rx) = mpsc::channel();
    let shared = Shared::new(vec![root.to_path_buf()], Some((0, tx)));
    let client = if let Some(url) = &server.url {
        let oauth = Arc::new(oauth::TokenSource::load(user_dir, name)?);
        let remote = Arc::new(http::Http::new(name, url, server, Some(oauth))?);
        let shared = Arc::new(shared);
        if server.transport == Some(crate::config::types::McpTransport::Sse) {
            remote
                .clone()
                .connect_sse(shared.clone(), remaining(deadline), cancel)?;
        }
        Client::remote(name, remote, shared)
    } else {
        Client::spawn(name, server, shared)?
    };
    match client.initialize(remaining(deadline), cancel) {
        Ok(()) => Ok((client, rx)),
        Err(error) => {
            if error.downcast_ref::<http::AuthRequired>().is_some()
                && client.auth_challenge().is_none()
            {
                bail!(
                    "MCP server `{name}` requires authorization (HTTP 401) but sent no OAuth challenge; configure bearer_env"
                );
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;
