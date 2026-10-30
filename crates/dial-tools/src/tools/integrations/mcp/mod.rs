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
//! A `tools/list_changed` notice refreshes that server's tool list: notices arriving together share
//! one listing, and the registry takes the new list at the next model request. A failed refresh keeps
//! the previous list, marks the catalog stale (calls are refused until a refresh succeeds) and
//! yields a notice.
//! Limits: `sampling` and `roots` are not offered, and servers are killed when the returned tools are
//! dropped.

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
mod live;
mod tool;
use crate::config::types::{Config, McpServerConfig};
use crate::runtime::integration::{
    CatalogFreshness, ConnectionHealth, IntegrationHealth, NotificationHealth,
};
use crate::tools::integrations::ext::Tool;
use anyhow::{Context, Result, bail, ensure};
use client::{Client, Shared};
use live::Catalog;
pub use live::Live;
use parking_lot::{Mutex, MutexGuard};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tool::RemoteTool;

#[derive(Clone, Debug)]
pub enum Signal {
    Closed,
    ToolsChanged,
    PromptsChanged,
    ResourcesChanged,
    ResourceUpdated,
    NotificationHealthy,
    NotificationUnsupported,
    NotificationDegraded,
}

/// Connect temporary server specifications using the caller's project and OAuth identity.
pub fn connect_extra(
    specs: &std::collections::BTreeMap<String, McpServerConfig>,
    config: &Config,
) -> McpLoad {
    let mut config = config.clone();
    config.mcp = specs.clone();
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
    /// Mirror of every connected server's tools; the registry adopts it and keeps it in sync.
    pub live: Live,
    pub notices: Vec<String>,
    /// `(server, instructions)` from each server's `initialize` result, for the system prompt.
    pub instructions: Vec<(String, String)>,
    /// Sessions of connected servers, for `/mcp resources|prompts|notifications`.
    pub sessions: Vec<Arc<Session>>,
    /// Servers that failed only because they answered HTTP 401 with an OAuth (Bearer) challenge:
    /// `/mcp login <name>` can sign them in. Each also has a line in `notices`.
    pub sign_in: Vec<String>,
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
    health: Mutex<IntegrationHealth>,
    /// The server's current tool list, versioned so the registry knows when to re-mirror it.
    catalog: Mutex<Catalog>,
    /// User-visible notices about catalog changes and failed refreshes, drained by the registry.
    notices: Mutex<Vec<String>>,
    reported_transitions: AtomicU64,
}

/// Cached `resources/list` + templates and `prompts/list` results; `None` until fetched or
/// after the matching list-changed notice.
#[derive(Default)]
struct Listing {
    resources: Option<Value>,
    prompts: Option<Value>,
    /// Bumped whenever a notice or reconnect invalidates the cache, so a list fetched before the
    /// bump is not stored over the newer state.
    generation: u64,
}

impl Listing {
    /// Runs `set` only if nothing invalidated the cache since `generation` was read.
    fn store(&mut self, generation: u64, set: impl FnOnce(&mut Self)) {
        if self.generation == generation {
            set(self);
        }
    }

    /// Drops both cached listings (a reconnect) and invalidates in-flight fetches.
    fn reset(&mut self) {
        self.resources = None;
        self.prompts = None;
        self.generation += 1;
    }
}

impl Session {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Independent runtime states. Reading this snapshot never connects to a server.
    pub fn health(&self) -> IntegrationHealth {
        if self.current().is_closed() {
            self.update_health(|health| {
                health.connection = ConnectionHealth::Disconnected;
                health.catalog = CatalogFreshness::Stale;
            });
        }
        self.health.lock().clone()
    }

    fn update_health(&self, update: impl FnOnce(&mut IntegrationHealth)) {
        let mut health = self.health.lock();
        let before = health.clone();
        update(&mut health);
        if *health != before {
            health.transitions = before.transitions.saturating_add(1);
        }
    }

    pub(super) fn catalog_current(&self) -> bool {
        self.health().catalog == CatalogFreshness::Fresh
    }

    /// Claim each transition once even when several calls settle concurrently.
    pub(super) fn observe_health(&self, through: u64) -> u64 {
        let before = self
            .reported_transitions
            .fetch_max(through, Ordering::AcqRel);
        through.saturating_sub(before)
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

    /// Wait for a live client or reconnect, with cancellation during both lock acquisition and
    /// connection setup. A server that never answers must not hold a cancelled tool call.
    pub(super) fn client_until(self: &Arc<Self>, cancel: &dyn Fn() -> bool) -> Result<Arc<Client>> {
        let _turn = lock_turn(&self.turn, cancel)?;
        let current = self.current();
        if !current.is_closed() {
            let snapshot = self.health();
            if snapshot.catalog == CatalogFreshness::Unknown {
                let deadline = crate::tools::exec::kernel::execution_deadline(self.timeout());
                self.sync_catalog(&current, deadline, cancel)?;
                self.update_health(|health| {
                    // Never erase a concurrent invalidation while the listing was in flight.
                    if health.transitions == snapshot.transitions {
                        health.catalog = CatalogFreshness::Fresh;
                    }
                });
            }
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
        self.update_health(|health| health.connection = ConnectionHealth::Connecting);
        let opened = open(
            &self.name,
            &self.server,
            &self.root,
            &self.user_dir,
            deadline,
            cancel,
        );
        let (client, signals) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                self.update_health(|health| health.connection = ConnectionHealth::Disconnected);
                return Err(error);
            }
        };
        let fresh = match self.sync_catalog(&client, deadline, cancel) {
            Ok(_) => true,
            Err(error) => {
                self.push_notice(format!(
                    "MCP server `{}`: tools/list failed after reconnect; calls stay refused until a refresh succeeds: {error:#}",
                    self.name
                ));
                false
            }
        };
        let client = Arc::new(client);
        let old = std::mem::replace(&mut *self.client.lock(), client.clone());
        self.listing.lock().reset();
        self.update_health(|health| {
            health.connection = ConnectionHealth::Connected;
            health.notifications = if self.server.url.is_none() {
                NotificationHealth::Healthy
            } else {
                NotificationHealth::Unknown
            };
            health.catalog = if fresh {
                CatalogFreshness::Fresh
            } else {
                CatalogFreshness::Stale
            };
            health.generation = health.generation.saturating_add(1);
        });
        drop(old);
        listen(self, signals)?;
        Ok(client)
    }

    /// Resources and resource templates, from cache when the server has not announced a change.
    pub fn resources(self: &Arc<Self>) -> Result<Value> {
        self.resources_until(&|| false)
    }

    pub(super) fn resources_until(self: &Arc<Self>, cancel: &dyn Fn() -> bool) -> Result<Value> {
        ensure!(!cancel(), "MCP resource listing cancelled before sending");
        if let Some(cached) = self.cached_listing(|listing| listing.resources.clone()) {
            return Ok(cached);
        }
        self.list_resources(cancel)
    }

    /// Fetches resources and resource templates from the server and caches them.
    fn list_resources(self: &Arc<Self>, cancel: &dyn Fn() -> bool) -> Result<Value> {
        let generation = self.listing.lock().generation;
        let deadline = crate::tools::exec::kernel::execution_deadline(self.timeout());
        let client = self.client_until(cancel)?;
        let listing = json!({
            "resources": list_all(&client, "resources/list", "resources", deadline, cancel)?,
            "templates": list_all(&client, "resources/templates/list", "resourceTemplates", deadline, cancel)?,
        });
        self.listing
            .lock()
            .store(generation, |cache| cache.resources = Some(listing.clone()));
        Ok(listing)
    }

    /// Prompts, from cache when the server has not announced a change.
    pub fn prompts(self: &Arc<Self>) -> Result<Value> {
        self.prompts_until(&|| false)
    }

    pub(super) fn prompts_until(self: &Arc<Self>, cancel: &dyn Fn() -> bool) -> Result<Value> {
        ensure!(!cancel(), "MCP prompt listing cancelled before sending");
        if let Some(cached) = self.cached_listing(|listing| listing.prompts.clone()) {
            return Ok(cached);
        }
        self.list_prompts(cancel)
    }

    /// Fetches prompts from the server and caches them.
    fn list_prompts(self: &Arc<Self>, cancel: &dyn Fn() -> bool) -> Result<Value> {
        let generation = self.listing.lock().generation;
        let deadline = crate::tools::exec::kernel::execution_deadline(self.timeout());
        let client = self.client_until(cancel)?;
        let listing =
            json!({"prompts": list_all(&client, "prompts/list", "prompts", deadline, cancel)?});
        self.listing
            .lock()
            .store(generation, |cache| cache.prompts = Some(listing.clone()));
        Ok(listing)
    }

    /// A cached listing, used only while the server's list notices are trusted.
    fn cached_listing(&self, pick: impl Fn(&Listing) -> Option<Value>) -> Option<Value> {
        let cached = pick(&self.listing.lock());
        cached.filter(|_| self.health().notifications == NotificationHealth::Healthy)
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

    /// Applies one server notice. A tool-list change refreshes the catalog (see
    /// [`Session::refresh_tools`]); a resource or prompt list change re-lists that kind, keeping the
    /// previous listing with a notice if the re-list fails; a closed transport marks the catalog
    /// stale until recovery succeeds.
    fn on_signal(self: &Arc<Self>, signal: Signal) {
        match signal {
            Signal::ResourcesChanged => self.listing.lock().generation += 1,
            Signal::ResourceUpdated => {
                let mut listing = self.listing.lock();
                listing.resources = None;
                listing.generation += 1;
            }
            Signal::PromptsChanged => self.listing.lock().generation += 1,
            Signal::ToolsChanged => {
                self.update_health(|health| health.catalog = CatalogFreshness::Stale)
            }
            Signal::Closed => self.update_health(|health| {
                health.connection = ConnectionHealth::Disconnected;
                health.catalog = CatalogFreshness::Stale;
            }),
            Signal::NotificationHealthy => {
                self.update_health(|health| health.notifications = NotificationHealth::Healthy)
            }
            Signal::NotificationUnsupported => {
                self.update_health(|health| health.notifications = NotificationHealth::Unsupported)
            }
            Signal::NotificationDegraded => {
                self.listing.lock().reset();
                self.update_health(|health| {
                    health.notifications = NotificationHealth::Degraded;
                    if health.catalog == CatalogFreshness::Fresh {
                        health.catalog = CatalogFreshness::Unknown;
                    }
                });
            }
        }
    }

    /// Re-lists what a notice announced, after [`Session::on_signal`] invalidated it. A failed list
    /// keeps the previous listing and reports a notice. Other signals have nothing to re-list.
    fn relist(self: &Arc<Self>, signal: &Signal) {
        match signal {
            Signal::ToolsChanged => self.refresh_tools(),
            Signal::ResourcesChanged => {
                if let Err(error) = self.list_resources(&|| false) {
                    self.push_notice(format!(
                        "mcp `{}`: resources/list refresh failed; keeping previous resources: {error:#}",
                        self.name
                    ));
                }
            }
            Signal::PromptsChanged => {
                if let Err(error) = self.list_prompts(&|| false) {
                    self.push_notice(format!(
                        "mcp `{}`: prompts/list refresh failed; keeping previous prompts: {error:#}",
                        self.name
                    ));
                }
            }
            _ => {}
        }
    }
}

/// Follows `nextCursor` pages of a list method and collects the `key` arrays.
fn list_all(
    client: &Client,
    method: &str,
    key: &str,
    deadline: Option<Instant>,
    cancel: &dyn Fn() -> bool,
) -> Result<Vec<Value>> {
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        ensure!(!cancel(), "MCP listing `{method}` cancelled");
        ensure!(
            deadline.is_none_or(|end| Instant::now() < end),
            "MCP listing `{method}` exceeded its operation deadline"
        );
        let params = cursor
            .as_ref()
            .map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
        let page = client.request_with(method, params, remaining(deadline), cancel, None)?;
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
    ensure!(
        cursor.is_none(),
        "MCP listing `{method}` incomplete: pagination limit reached"
    );
    Ok(items)
}

/// Quiet period after a tool-list notice: notices inside it share one refresh.
const TOOLS_CHANGED_DEBOUNCE: Duration = Duration::from_millis(100);

/// Routes a session's signals: a tool-list notice refreshes the catalog once per burst, other list
/// notices invalidate their cache, and a closed transport triggers recovery. The thread holds only a
/// weak reference, so dropping the session (its tools) stops the server.
fn listen(session: &Arc<Session>, signals: mpsc::Receiver<Signal>) -> Result<()> {
    let weak: Weak<Session> = Arc::downgrade(session);
    let generation = session.health.lock().generation;
    thread::Builder::new()
        .name(format!("mcp-{}-signals", session.name))
        .spawn(move || {
            let mut queued = VecDeque::new();
            loop {
                let signal = match queued.pop_front() {
                    Some(signal) => signal,
                    None => match signals.recv() {
                        Ok(signal) => signal,
                        Err(_) => return,
                    },
                };
                let debounced = matches!(
                    signal,
                    Signal::ToolsChanged | Signal::PromptsChanged | Signal::ResourcesChanged
                );
                if debounced {
                    // Notices of one kind sent together share one re-list: wait out the burst, drop
                    // the extra notices of that kind it holds and keep the rest in order.
                    let kind = std::mem::discriminant(&signal);
                    thread::sleep(TOOLS_CHANGED_DEBOUNCE);
                    queued.extend(
                        signals
                            .try_iter()
                            .filter(|next| std::mem::discriminant(next) != kind),
                    );
                }
                let Some(session) = weak.upgrade() else {
                    return;
                };
                if session.health.lock().generation != generation {
                    return;
                }
                let closed = matches!(signal, Signal::Closed);
                let listed = signal.clone();
                session.on_signal(signal);
                session.relist(&listed);
                if closed {
                    session.recover();
                    return;
                }
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

    for (server, handle) in attempts {
        let outcome = match handle {
            Ok(handle) => handle
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("connection thread panicked"))),
            Err(error) => Err(error.into()),
        };
        match outcome {
            Ok(connected) => add_tools(&mut loaded, server, connected),
            Err(error) => {
                if error
                    .downcast_ref::<http::AuthRequired>()
                    .is_some_and(http::AuthRequired::oauth_sign_in)
                {
                    loaded.sign_in.push(server.clone());
                }
                loaded.notices.push(format!("mcp `{server}`: {error:#}"));
            }
        }
    }
    loaded
}

/// Everything one connected server contributes to the registry.
struct Connected {
    session: Arc<Session>,
    /// Whether `tools/list` returned any tool at connect time.
    offers_tools: bool,
    notice: Option<String>,
    instructions: Option<String>,
    resources: bool,
    prompts: bool,
}

fn add_tools(loaded: &mut McpLoad, server: &str, connected: Connected) {
    let Connected {
        session,
        offers_tools,
        notice,
        instructions,
        resources,
        prompts,
    } = connected;
    loaded.notices.extend(notice);
    if let Some(instructions) = instructions {
        loaded.instructions.push((server.to_owned(), instructions));
    }
    if !offers_tools && !resources && !prompts {
        loaded
            .notices
            .push(format!("mcp `{server}`: connected but offers no tools"));
        // Keep initialized sessions in the inventory even when they expose no callable surface.
    }
    loaded.sessions.push(session.clone());
    loaded.live.mount(session, resources, prompts);
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
        reported_transitions: AtomicU64::new(0),
        health: Mutex::new(IntegrationHealth {
            connection: ConnectionHealth::Connected,
            notifications: if server.url.is_none() {
                NotificationHealth::Healthy
            } else {
                NotificationHealth::Unknown
            },
            catalog: CatalogFreshness::Fresh,
            generation: 1,
            transitions: 1,
        }),
        catalog: Mutex::new(Catalog::default()),
        notices: Mutex::new(Vec::new()),
    });

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
    let offers_tools = !tools.is_empty();
    session.apply_catalog(tools);
    listen(&session, signals)?;
    Ok(Connected {
        session,
        offers_tools,
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

/// The stdio config with a relative `cwd` resolved against the project root, not against Dial's
/// own working directory (which differs under `-C`). A portable entry such as `cwd = "."` then
/// runs every project's server in that project, whichever config layer holds it.
fn in_project<'a>(server: &'a McpServerConfig, root: &Path) -> Cow<'a, McpServerConfig> {
    match &server.cwd {
        Some(cwd) if cwd.is_relative() => {
            let mut resolved = server.clone();
            resolved.cwd = Some(root.join(cwd));
            Cow::Owned(resolved)
        }
        _ => Cow::Borrowed(server),
    }
}

/// Spawns or dials the server and performs `initialize`, returning the client and its signals.
fn open(
    name: &str,
    server: &McpServerConfig,
    root: &Path,
    user_dir: &Path,
    deadline: Option<Instant>,
    cancel: &dyn Fn() -> bool,
) -> Result<(Client, mpsc::Receiver<Signal>)> {
    let (tx, rx) = mpsc::channel();
    let shared = Shared::new(vec![root.to_path_buf()], Some(tx));
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
        Client::spawn(name, &in_project(server, root), shared)?
    };
    match client.initialize(remaining(deadline), cancel) {
        Ok(()) => Ok((client, rx)),
        Err(error) => {
            // The request thread reports refusals as text, so the typed error is rebuilt from the
            // transport: installers and `/mcp` need to tell "sign in" apart from "broken".
            if client.sign_in_offered() {
                return Err(anyhow::Error::new(http::AuthRequired(
                    name.to_owned(),
                    401,
                    true,
                )));
            }
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
