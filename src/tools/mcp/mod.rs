//! Model Context Protocol client (stdio transport).
//!
//! Each `[mcp.<name>]` server in the config is started as a trusted operator program (unsandboxed,
//! like `[tools.*]`, with this process's environment minus likely credentials plus the entry's
//! `env`), handshaken, and its tools offered to the model as `mcp__<name>__<tool>`. The model can
//! reach a server only through those tools; calls need approval unless the tool declares
//! `annotations.readOnlyHint`. Servers are connected concurrently and a server that fails or is slow
//! to start only yields a notice; one that dies later makes its tools return errors.
//!
//! Limits: only tools are used (no resources, prompts, sampling or roots), `tools/list_changed`
//! notifications are ignored (the list is fixed at startup), and servers are killed when the
//! returned tools are dropped.

mod client;
mod http;
pub use http::validate_url;
mod names;
mod tool;

use crate::config::types::{Config, McpServerConfig};
use crate::tools::ext::Tool;
use anyhow::{Result, bail};
use client::Client;
use names::Namer;
use serde_json::{Value, json};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tool::{McpTool, RemoteTool};

/// Budget for one server's whole startup: spawn, `initialize` and every `tools/list` page.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Guards against a server that paginates forever.
const MAX_PAGES: usize = 50;
const MAX_TOOLS_PER_SERVER: usize = 256;

/// The tools of every server that connected, plus one line per problem for the user.
#[derive(Default)]
pub struct McpLoad {
    pub tools: Vec<Arc<dyn Tool>>,
    pub notices: Vec<String>,
}

struct Connected {
    client: Arc<Client>,
    tools: Vec<RemoteTool>,
    timeout: Duration,
    lifecycle_hooks: Vec<String>,
    notice: Option<String>,
}

/// Connects every configured server. Spawns nothing when `config.mcp` is empty.
pub fn load(config: &Config) -> McpLoad {
    load_with(config, CONNECT_TIMEOUT)
}

fn load_with(config: &Config, connect_timeout: Duration) -> McpLoad {
    let mut loaded = McpLoad::default();
    let attempts: Vec<_> = config
        .mcp
        .iter()
        .map(|(name, server)| {
            let (thread_name, server_name, server) =
                (format!("mcp-{name}-connect"), name.clone(), server.clone());
            let handle = thread::Builder::new()
                .name(thread_name)
                .spawn(move || connect(&server_name, &server, connect_timeout));
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
            Ok(connected) => {
                loaded.notices.extend(connected.notice.clone());
                add_tools(&mut loaded, &mut namer, server, connected);
            }
            Err(error) => loaded.notices.push(format!("mcp `{server}`: {error:#}")),
        }
    }
    loaded
}

fn add_tools(loaded: &mut McpLoad, namer: &mut Namer, server: &str, connected: Connected) {
    if connected.tools.is_empty() {
        loaded
            .notices
            .push(format!("mcp `{server}`: connected but offers no tools"));
        return; // dropping the only client reference stops the server
    }
    for remote in connected.tools {
        let name = namer.name(server, &remote.name);
        let tool = McpTool::with_lifecycle_hooks(
            name,
            server,
            remote,
            connected.client.clone(),
            connected.timeout,
            connected.lifecycle_hooks.clone(),
        );
        loaded.tools.push(Arc::new(tool));
    }
}

fn connect(name: &str, server: &McpServerConfig, connect_timeout: Duration) -> Result<Connected> {
    let deadline = Instant::now() + connect_timeout;
    let remaining = || {
        deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1))
    };
    let client = Arc::new(if server.url.is_some() {
        Client::connect_http(name, server)?
    } else {
        Client::spawn(name, server)?
    });
    client.initialize(remaining())?;

    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let params = cursor
            .as_ref()
            .map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
        let page = client.request("tools/list", params, remaining())?;
        let Some(entries) = page.get("tools").and_then(Value::as_array) else {
            bail!("`tools/list` returned no `tools` array");
        };
        tools.extend(entries.iter().filter_map(RemoteTool::parse));
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
    let notice = truncated.then(|| {
        format!(
            "mcp `{name}`: tool list cut off after {} tools",
            tools.len()
        )
    });
    Ok(Connected {
        client,
        tools,
        timeout: Duration::from_secs(server.timeout_secs.max(1)),
        lifecycle_hooks: server.lifecycle_hooks.clone(),
        notice,
    })
}

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;
