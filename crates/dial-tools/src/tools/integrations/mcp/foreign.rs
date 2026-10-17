//! MCP servers that other coding agents already have configured.
//!
//! Dial reads these files read-only and never starts what it finds on its own: the extensions
//! browser lists them and the user imports one into Dial's own config after confirming. The
//! locations mirror `storage::foreign` (which does the same for skills).
//!
//! Formats: JSON with an `mcpServers` object (Claude `.mcp.json` and `~/.claude.json`, Cursor,
//! Windsurf, Gemini) and Codex's TOML `[mcp_servers.<name>]` tables.

use crate::config::types::{McpServerConfig, McpTransport};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Config files larger than this are ignored; real ones are a few KiB.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// One server found in another tool's config.
#[derive(Debug, Clone)]
pub struct Found {
    /// Name in Dial's config: the other tool's name, reduced to `[a-z0-9_-]`.
    pub name: String,
    /// Tool the entry came from, e.g. `Claude`.
    pub provider: &'static str,
    /// File the entry was read from.
    pub source: PathBuf,
    /// Other tools that define the same name; the first provider wins.
    pub also_in: Vec<&'static str>,
    pub server: McpServerConfig,
}

#[derive(Clone, Copy)]
enum Format {
    /// JSON `{"mcpServers": {...}}`.
    Json,
    /// `~/.claude.json`: top-level `mcpServers` plus `projects.<root>.mcpServers`.
    ClaudeUser,
    /// TOML `[mcp_servers.<name>]`.
    Codex,
}

struct Source {
    provider: &'static str,
    /// Relative to the workspace root (`project`) or to the home directory.
    rel: &'static str,
    project: bool,
    format: Format,
}

/// Highest precedence first, project files before user files.
const SOURCES: &[Source] = &[
    Source {
        provider: "Claude",
        rel: ".mcp.json",
        project: true,
        format: Format::Json,
    },
    Source {
        provider: "Cursor",
        rel: ".cursor/mcp.json",
        project: true,
        format: Format::Json,
    },
    Source {
        provider: "Claude",
        rel: ".claude.json",
        project: false,
        format: Format::ClaudeUser,
    },
    Source {
        provider: "Codex",
        rel: ".codex/config.toml",
        project: false,
        format: Format::Codex,
    },
    Source {
        provider: "Cursor",
        rel: ".cursor/mcp.json",
        project: false,
        format: Format::Json,
    },
    Source {
        provider: "Gemini",
        rel: ".gemini/settings.json",
        project: false,
        format: Format::Json,
    },
    Source {
        provider: "Windsurf",
        rel: ".codeium/windsurf/mcp_config.json",
        project: false,
        format: Format::Json,
    },
];

/// The user's home directory, if `HOME` names one.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// Servers defined by other tools for this workspace and user. Unreadable, oversized or
/// malformed files and entries Dial cannot represent are skipped; the first definition of a
/// name wins. Entries are not validated against Dial's config rules until they are imported.
pub fn discover(home: Option<&Path>, project_root: &Path) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    for source in SOURCES {
        let path = if source.project {
            project_root.join(source.rel)
        } else if let Some(home) = home {
            home.join(source.rel)
        } else {
            continue;
        };
        let Some(text) = read_small(&path) else {
            continue;
        };
        for (name, server) in parse(source.format, &text, project_root) {
            let name = super::catalog::config_name(&name);
            match found.iter_mut().find(|known| known.name == name) {
                Some(known) => {
                    if known.provider != source.provider
                        && !known.also_in.contains(&source.provider)
                    {
                        known.also_in.push(source.provider);
                    }
                }
                None => found.push(Found {
                    name,
                    provider: source.provider,
                    source: path.clone(),
                    also_in: Vec::new(),
                    server,
                }),
            }
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// The entry named `name` among [`discover`]'s results.
pub fn find(home: Option<&Path>, project_root: &Path, name: &str) -> Option<Found> {
    discover(home, project_root)
        .into_iter()
        .find(|found| found.name == name)
}

/// Names that Dial already configures are not offered again.
pub fn not_configured(found: Vec<Found>, configured: &BTreeSet<&str>) -> Vec<Found> {
    found
        .into_iter()
        .filter(|entry| !configured.contains(entry.name.as_str()))
        .collect()
}

fn read_small(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn parse(format: Format, text: &str, project_root: &Path) -> Vec<(String, McpServerConfig)> {
    match format {
        Format::Json | Format::ClaudeUser => {
            let Ok(root) = serde_json::from_str::<Value>(text) else {
                return Vec::new();
            };
            let mut out = json_servers(root.get("mcpServers"));
            if matches!(format, Format::ClaudeUser) {
                // Servers a user scoped to this project inside `~/.claude.json`.
                let key = project_root.to_string_lossy();
                out.extend(json_servers(root.pointer(&format!(
                    "/projects/{}/mcpServers",
                    escape_pointer(&key)
                ))));
            }
            out
        }
        Format::Codex => {
            let Ok(root) = toml::from_str::<toml::Table>(text) else {
                return Vec::new();
            };
            let Some(servers) = root.get("mcp_servers").and_then(toml::Value::as_table) else {
                return Vec::new();
            };
            servers
                .iter()
                .filter_map(|(name, value)| {
                    let json = serde_json::to_value(value).ok()?;
                    codex_server(&json).map(|server| (name.clone(), server))
                })
                .collect()
        }
    }
}

fn escape_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn json_servers(table: Option<&Value>) -> Vec<(String, McpServerConfig)> {
    let Some(table) = table.and_then(Value::as_object) else {
        return Vec::new();
    };
    table
        .iter()
        .filter_map(|(name, entry)| json_server(entry).map(|server| (name.clone(), server)))
        .collect()
}

/// `command`/`args`/`env`/`cwd` for stdio, `url`/`serverUrl` and `headers` for remote servers.
fn json_server(entry: &Value) -> Option<McpServerConfig> {
    let mut server = McpServerConfig::default();
    let url = text(entry, "url").or_else(|| text(entry, "serverUrl"));
    if let Some(url) = url {
        server.url = Some(url);
        server.transport = match text(entry, "type").as_deref() {
            Some("sse") => Some(McpTransport::Sse),
            _ => None,
        };
        server.headers = string_map(entry.get("headers"));
    } else {
        server.program = PathBuf::from(text(entry, "command")?);
        server.args = string_list(entry.get("args"));
        server.env = string_map(entry.get("env"));
        server.cwd = text(entry, "cwd").map(PathBuf::from);
    }
    if entry.get("disabled").and_then(Value::as_bool) == Some(true) {
        server.enabled = false;
    }
    Some(server)
}

fn codex_server(entry: &Value) -> Option<McpServerConfig> {
    let mut server = json_server(entry)?;
    if server.url.is_some() {
        server.bearer_env = text(entry, "bearer_token_env_var");
        server.headers = string_map(entry.get("http_headers"));
    }
    if let Some(seconds) = entry.get("startup_timeout_sec").and_then(Value::as_u64) {
        server.connect_timeout_secs = seconds;
    }
    if let Some(seconds) = entry.get("tool_timeout_sec").and_then(Value::as_u64) {
        server.timeout_secs = seconds;
    }
    if entry.get("enabled").and_then(Value::as_bool) == Some(false) {
        server.enabled = false;
    }
    Some(server)
}

fn text(entry: &Value, key: &str) -> Option<String> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn string_map(value: Option<&Value>) -> std::collections::BTreeMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
