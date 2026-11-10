//! Keeps the registry's MCP tools in step with each server's current tool list.
//!
//! A [`Session`] owns a versioned [`Catalog`]: its revision moves only when the list changes.
//! [`Live`] mirrors that catalog per server. The registry calls [`Live::sync`] at a request
//! boundary, and only servers whose revision moved are reconciled, so unchanged tools keep their
//! identity.

use super::client::Client;
use super::names::Namer;
use super::tool::{McpSurface, McpTool, RemoteTool, SurfaceKind};
use super::{CATALOG_RETRY_INTERVAL, MAX_PAGES, MAX_TOOLS_PER_SERVER, Session, remaining};
use crate::runtime::integration::CatalogFreshness;
use crate::tools::integrations::ext::Tool;
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

/// Most notices a session keeps while no one drains them; the oldest are dropped first.
const NOTICE_LIMIT: usize = 16;

/// A server's tool list, versioned: `revision` moves only when the list changes.
#[derive(Default)]
pub(super) struct Catalog {
    /// Sorted by remote name.
    tools: Vec<RemoteTool>,
    revision: u64,
}

/// How one tool list differs from the previous one, by remote tool name.
#[derive(Debug, Default, PartialEq)]
pub(super) struct CatalogDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

impl CatalogDiff {
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// Compares two tool lists sorted by name.
fn diff(old: &[RemoteTool], new: &[RemoteTool]) -> CatalogDiff {
    let mut out = CatalogDiff::default();
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        match (old.get(i), new.get(j)) {
            (Some(a), Some(b)) if a.name == b.name => {
                if a != b {
                    out.changed.push(a.name.clone());
                }
                i += 1;
                j += 1;
            }
            (Some(a), Some(b)) if a.name < b.name => {
                out.removed.push(a.name.clone());
                i += 1;
            }
            (Some(_), Some(b)) => {
                out.added.push(b.name.clone());
                j += 1;
            }
            (Some(a), None) => {
                out.removed.push(a.name.clone());
                i += 1;
            }
            (None, Some(b)) => {
                out.added.push(b.name.clone());
                j += 1;
            }
            (None, None) => break,
        }
    }
    out
}

impl Session {
    /// Lists the server's tools as the registry offers them: parsed, truncated, trust-filtered,
    /// sorted by name. A resource- or prompt-only server does not declare `tools`, and probing
    /// `tools/list` would fail it.
    pub(super) fn fetch_tools(
        &self,
        client: &Client,
        deadline: Option<Instant>,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Vec<RemoteTool>> {
        let serves_tools = client.has_capability("tools")
            || !(client.has_capability("resources") || client.has_capability("prompts"));
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        if serves_tools {
            for page_index in 0..MAX_PAGES {
                ensure!(!cancel(), "MCP tools/list cancelled");
                ensure!(
                    deadline.is_none_or(|end| Instant::now() < end),
                    "MCP tools/list exceeded its operation deadline"
                );
                let params = cursor
                    .as_ref()
                    .map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
                let page =
                    client.request_with("tools/list", params, remaining(deadline), cancel, None)?;
                let Some(entries) = page.get("tools").and_then(Value::as_array) else {
                    bail!("`tools/list` returned no `tools` array");
                };
                let mut parsed = entries.iter().filter_map(RemoteTool::parse);
                tools.extend(parsed.by_ref().take(MAX_TOOLS_PER_SERVER - tools.len()));
                let extra = parsed.next().is_some();
                cursor = page
                    .get("nextCursor")
                    .and_then(Value::as_str)
                    .filter(|c| !c.is_empty())
                    .map(str::to_owned);
                if extra
                    || (cursor.is_some()
                        && (tools.len() == MAX_TOOLS_PER_SERVER || page_index + 1 == MAX_PAGES))
                {
                    self.push_notice(format!(
                        "mcp `{}`: tool list cut off after {} tools (limit: {} tools / {} pages)",
                        self.name,
                        tools.len(),
                        MAX_TOOLS_PER_SERVER,
                        MAX_PAGES
                    ));
                    break;
                }
                if cursor.is_none() {
                    break;
                }
            }
        }
        for tool in &mut tools {
            tool.read_only_hint &= self.server.trust_read_only_hint;
        }
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(tools)
    }

    /// Replaces the catalog with `tools` when they differ, and returns what changed.
    pub(super) fn apply_catalog(&self, mut tools: Vec<RemoteTool>) -> CatalogDiff {
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        let mut catalog = self.catalog.lock();
        let diff = diff(&catalog.tools, &tools);
        if !diff.is_empty() {
            catalog.revision += 1;
            catalog.tools = tools;
        }
        diff
    }

    /// Fetches, applies, and reports the server's tool list. Reports a notice only when the list
    /// changed. Errors propagate so the caller decides the freshness.
    pub(super) fn sync_catalog(
        &self,
        client: &Client,
        deadline: Option<Instant>,
        cancel: &dyn Fn() -> bool,
    ) -> Result<CatalogDiff> {
        let revision = client.signal_revision();
        let transitions = self.health.lock().transitions;
        let tools = self.fetch_tools(client, deadline, cancel)?;
        let diff = self.apply_catalog(tools);
        self.catalog_signal_revision
            .store(revision, std::sync::atomic::Ordering::Release);
        self.update_health(|health| {
            health.catalog =
                if revision == client.signal_revision() && health.transitions == transitions {
                    CatalogFreshness::Fresh
                } else {
                    CatalogFreshness::Stale
                };
        });
        if !diff.is_empty() {
            self.push_notice(format!(
                "mcp `{}`: tools updated (+{} -{} ~{})",
                self.name,
                diff.added.len(),
                diff.removed.len(),
                diff.changed.len()
            ));
        }
        Ok(diff)
    }

    /// Handles a tool-list notice: re-lists the server and applies the result. The caller holds
    /// the signal thread, so a burst of notices is serialized here; the turn lock keeps a request
    /// from reading the catalog mid-refresh. A failure keeps the previous list and marks it stale.
    pub(super) fn refresh_tools(&self) {
        let _turn = self.turn.lock();
        let client = self.current();
        if client.is_closed() {
            // Recovery re-syncs the catalog when the transport returns.
            return;
        }
        let deadline = Some(Instant::now() + self.catalog_timeout());
        *self.catalog_retry.lock() = Some(Instant::now() + CATALOG_RETRY_INTERVAL);
        match self.sync_catalog(&client, deadline, &|| false) {
            Ok(_) => {}
            Err(error) => {
                let error = format!("{error:#}");
                self.update_health(|health| health.catalog = CatalogFreshness::Stale);
                self.push_notice(format!(
                    "mcp `{}`: tools/list refresh failed; keeping previous tools: {error}",
                    self.name
                ));
            }
        }
    }

    pub(super) fn catalog_revision(&self) -> u64 {
        self.catalog.lock().revision
    }

    /// The current revision and tool list, cloned for reconciliation.
    pub(super) fn catalog_snapshot(&self) -> (u64, Vec<RemoteTool>) {
        let catalog = self.catalog.lock();
        (catalog.revision, catalog.tools.clone())
    }

    /// Linearizes definition and freshness admission against catalog publication. The guard is
    /// released before request I/O or callbacks, so refresh and cancellation cannot deadlock it.
    pub(super) fn admit_tool(
        &self,
        client: &Client,
        tool: &RemoteTool,
        cancel: &dyn Fn() -> bool,
    ) -> Result<()> {
        let catalog = loop {
            ensure!(!cancel(), "MCP tool admission cancelled");
            if let Some(catalog) = self.catalog.try_lock_for(super::CANCEL_POLL) {
                break catalog;
            }
        };
        ensure!(
            catalog.tools.iter().any(|live| live == tool),
            "MCP tool was removed or its definition changed on the server; the next request offers the current definition"
        );
        ensure!(
            std::ptr::eq(self.current().as_ref(), client) && self.catalog_current(),
            "MCP tool catalog is stale or unverified; retry after catalog revalidation before using this registered definition"
        );
        Ok(())
    }

    /// Whether the live catalog still offers exactly this definition.
    #[cfg(test)]
    pub(super) fn offers(&self, tool: &RemoteTool) -> bool {
        self.catalog.lock().tools.iter().any(|live| live == tool)
    }

    pub(super) fn push_notice(&self, notice: String) {
        let mut notices = self.notices.lock();
        if notices.len() == NOTICE_LIMIT {
            notices.remove(0);
        }
        notices.push(notice);
    }

    fn drain_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock())
    }
}

/// Registered tools that changed since the last [`Live::sync`].
#[derive(Default)]
pub struct Change {
    /// Tools to register: new, redefined, or a server's surfaces on first sync. Keyed by name.
    pub upserted: Vec<(String, Arc<dyn Tool>)>,
    /// Registered names that the servers no longer offer.
    pub removed: Vec<String>,
    /// Notices from the servers' refreshes and reconnects.
    pub notices: Vec<String>,
}

/// One remote tool as the registry currently offers it.
#[derive(Clone)]
struct Offered {
    remote: RemoteTool,
    name: String,
}

/// One connected server as the registry mirrors it.
#[derive(Clone)]
struct Mounted {
    session: Arc<Session>,
    /// Catalog revision last reconciled; `None` before the first sync.
    applied: Option<u64>,
    /// Registered name per remote tool name. Kept after removal, so a returning tool keeps it.
    names: BTreeMap<String, String>,
    /// Tools currently offered, by remote name.
    offered: BTreeMap<String, Offered>,
    /// One resource/prompt context surface, registered once when mounted.
    surfaces: Vec<Arc<dyn Tool>>,
    /// Exact names reserved by the old wrappers, before any remote tool names are allocated.
    legacy_surfaces: Vec<(crate::tools::surface::contracts::Identity, SurfaceKind)>,
}

/// The registry's mirror of every connected server's tools, keyed by registered name.
#[derive(Clone, Default)]
pub struct Live {
    namer: Namer,
    servers: Vec<Mounted>,
    /// Registered names a server withdrew, so calls can say why they fail.
    retired: BTreeSet<String>,
}

impl Live {
    /// Adds a connected server. Its tools are registered by the next [`Live::sync`].
    pub(super) fn mount(&mut self, session: Arc<Session>, resources: bool, prompts: bool) {
        let server = session.name.clone();
        let mut legacy_surfaces = Vec::new();
        for (enabled, remote, kind) in [
            (resources, "read_resource", SurfaceKind::Resource),
            (prompts, "get_prompt", SurfaceKind::Prompt),
        ] {
            if enabled {
                let name = self.namer.name(&server, remote);
                legacy_surfaces.push((kind.historical_identity(name), kind));
            }
        }
        self.servers.push(Mounted {
            session,
            applied: None,
            names: BTreeMap::new(),
            offered: BTreeMap::new(),
            surfaces: Vec::new(),
            legacy_surfaces,
        });
    }

    /// Reconciles every server whose catalog moved since the last call, and drains their notices.
    /// Unchanged tools keep their existing `Arc`, so the registry sees only real differences.
    pub fn sync(&mut self) -> Change {
        let mut change = Change::default();
        for mounted in &self.servers {
            change.notices.extend(mounted.session.drain_notices());
        }
        self.reconcile(&mut change);
        change
    }

    /// The tools every server offers now, as [`Live::sync`] would register them. Notices and
    /// events stay queued, so a read-only view can call this without consuming them.
    pub fn offered_tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut view = self.clone();
        for mounted in &mut view.servers {
            mounted.applied = None;
            mounted.offered.clear();
        }
        let mut change = Change::default();
        view.reconcile(&mut change);
        change.upserted.into_iter().map(|(_, tool)| tool).collect()
    }

    /// Pure projection of a settled old resource/prompt wrapper call. Exact identities and the
    /// names reserved at mount time distinguish wrappers from identically named remote tools.
    /// This never executes or admits a call, and leaves the durable original untouched.
    pub fn historical_surface_projection(
        &self,
        recorded: &crate::tools::surface::contracts::Identity,
        arguments: &Value,
    ) -> Option<(String, Value)> {
        let mut view = self.clone();
        view.reconcile(&mut Change::default());
        for mounted in &view.servers {
            if let Some((_, kind)) = mounted
                .legacy_surfaces
                .iter()
                .find(|(old, _)| old == recorded)
            {
                let mut arguments = arguments.as_object()?.clone();
                // Unknown fields were historically ignored. The new discriminator must come
                // from the recorded wrapper identity, never from old argument data.
                arguments.insert("surface".into(), json!(kind.selector()));
                return Some((
                    mounted.surfaces.first()?.name().to_owned(),
                    Value::Object(arguments),
                ));
            }
        }
        None
    }

    fn reconcile(&mut self, change: &mut Change) {
        let Live {
            namer,
            servers,
            retired,
        } = self;
        // Reserve all remote names before allocating any new context wrapper. This reproduces
        // the legacy order (all old wrappers, then remote catalogs) even for sanitized collisions.
        for mounted in servers.iter_mut() {
            for remote in mounted.session.catalog_snapshot().1 {
                mounted
                    .names
                    .entry(remote.name.clone())
                    .or_insert_with(|| namer.name(&mounted.session.name, &remote.name));
            }
        }
        for mounted in servers.iter_mut() {
            let revision = mounted.session.catalog_revision();
            if mounted.applied == Some(revision) {
                continue;
            }
            if mounted.surfaces.is_empty() && !mounted.legacy_surfaces.is_empty() {
                let server = mounted.session.name.clone();
                let name = namer.name(&server, "context");
                mounted.surfaces.push(Arc::new(McpSurface::new(
                    name,
                    &server,
                    mounted.session.clone(),
                    mounted.session.server.lifecycle_hooks.clone(),
                )));
            }
            if mounted.applied.is_none() {
                change.upserted.extend(
                    mounted
                        .surfaces
                        .iter()
                        .map(|surface| (surface.name().to_owned(), surface.clone())),
                );
            }
            let (revision, tools) = mounted.session.catalog_snapshot();
            let server = mounted.session.name.clone();
            let hooks = mounted.session.server.lifecycle_hooks.clone();
            let timeout = mounted.session.timeout();

            let live: BTreeSet<String> = tools.iter().map(|tool| tool.name.clone()).collect();
            let withdrawn: Vec<String> = mounted
                .offered
                .keys()
                .filter(|remote| !live.contains(*remote))
                .cloned()
                .collect();
            for remote in withdrawn {
                if let Some(gone) = mounted.offered.remove(&remote) {
                    retired.insert(gone.name.clone());
                    change.removed.push(gone.name);
                }
            }

            for remote in tools {
                if mounted
                    .offered
                    .get(&remote.name)
                    .is_some_and(|current| current.remote == remote)
                {
                    continue;
                }
                let name = match mounted.names.get(&remote.name) {
                    Some(name) => name.clone(),
                    None => {
                        let name = namer.name(&server, &remote.name);
                        mounted.names.insert(remote.name.clone(), name.clone());
                        name
                    }
                };
                let tool: Arc<dyn Tool> = Arc::new(McpTool::with_lifecycle_hooks(
                    name.clone(),
                    &server,
                    remote.clone(),
                    mounted.session.clone(),
                    timeout,
                    hooks.clone(),
                ));
                retired.remove(&name);
                change.upserted.push((name.clone(), tool.clone()));
                mounted
                    .offered
                    .insert(remote.name.clone(), Offered { remote, name });
            }
            mounted.applied = Some(revision);
        }
    }

    /// Whether `name` was a tool a server withdrew and has not returned.
    pub fn is_retired(&self, name: &str) -> bool {
        self.retired.contains(name)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod historical_tests;
