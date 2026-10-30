//! Keeps the registry's MCP tools in step with each server's current tool list.
//!
//! A [`Session`] owns a versioned [`Catalog`]: its revision moves only when the list changes.
//! [`Live`] mirrors that catalog per server. The registry calls [`Live::sync`] at a request
//! boundary, and only servers whose revision moved are reconciled, so unchanged tools keep their
//! identity.

use super::client::Client;
use super::names::Namer;
use super::tool::{McpSurface, McpTool, RemoteTool, SurfaceKind};
use super::{MAX_TOOLS_PER_SERVER, Session, list_all};
use crate::runtime::integration::CatalogFreshness;
use crate::tools::integrations::ext::Tool;
use anyhow::Result;
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
    fn fetch_tools(
        &self,
        client: &Client,
        deadline: Option<Instant>,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Vec<RemoteTool>> {
        let serves_tools = client.has_capability("tools")
            || !(client.has_capability("resources") || client.has_capability("prompts"));
        let entries = if serves_tools {
            list_all(client, "tools/list", "tools", deadline, cancel)?
        } else {
            Vec::new()
        };
        let mut tools: Vec<RemoteTool> = entries.iter().filter_map(RemoteTool::parse).collect();
        tools.truncate(MAX_TOOLS_PER_SERVER);
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
        let tools = self.fetch_tools(client, deadline, cancel)?;
        let diff = self.apply_catalog(tools);
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
        let deadline = crate::tools::exec::kernel::execution_deadline(self.timeout());
        match self.sync_catalog(&client, deadline, &|| false) {
            Ok(_) => self.update_health(|health| health.catalog = CatalogFreshness::Fresh),
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

    /// Whether the live catalog still offers exactly this definition.
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
    /// `read_resource` and `get_prompt` surfaces, registered once when mounted.
    surfaces: Vec<Arc<dyn Tool>>,
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
        let hooks = session.server.lifecycle_hooks.clone();
        let mut surfaces: Vec<Arc<dyn Tool>> = Vec::new();
        if resources {
            let name = self.namer.name(&server, "read_resource");
            surfaces.push(Arc::new(McpSurface::new(
                name,
                &server,
                SurfaceKind::Resource,
                session.clone(),
                hooks.clone(),
            )));
        }
        if prompts {
            let name = self.namer.name(&server, "get_prompt");
            surfaces.push(Arc::new(McpSurface::new(
                name,
                &server,
                SurfaceKind::Prompt,
                session.clone(),
                hooks,
            )));
        }
        self.servers.push(Mounted {
            session,
            applied: None,
            names: BTreeMap::new(),
            offered: BTreeMap::new(),
            surfaces,
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

    fn reconcile(&mut self, change: &mut Change) {
        let Live {
            namer,
            servers,
            retired,
        } = self;
        for mounted in servers.iter_mut() {
            let revision = mounted.session.catalog_revision();
            if mounted.applied == Some(revision) {
                continue;
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
