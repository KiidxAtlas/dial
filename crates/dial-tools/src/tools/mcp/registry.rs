//! Official MCP Registry client (`registry.modelcontextprotocol.io/v0`). Reads listings only: no
//! package is fetched or started here. Entries map to [`Candidate`]s; see `catalog` for install.
//!
//! Schema used (checked against the live API): `{"servers":[{"server":{name, title?, description,
//! version, packages[], remotes[]}, "_meta":{"io.modelcontextprotocol.registry/official":{status,
//! isLatest}}}], "metadata":{"nextCursor"}}`. Each package has `registryType` (`npm`, `pypi`, `oci`),
//! `identifier`, `version`, `transport.type` and `environmentVariables[]`; each remote has `type`
//! (`streamable-http` or `sse`), `url` and `headers[]`.
use super::catalog::{
    Candidate, Input, Install, Source, cache_env, looks_secret, normalize_repository,
};
use super::smithery::safe_metadata;
use crate::config::types::{McpServerConfig, McpTransport};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const OFFICIAL_URL: &str = "https://registry.modelcontextprotocol.io/v0";
const PAGE_SIZE: usize = 20;
/// Cursor pages walked at most for exact-name lookups (interactive browsing is not capped).
const MAX_PAGES: usize = 10;
const TIMEOUT: Duration = Duration::from_secs(10);
const LATEST_KEY: &str = "io.modelcontextprotocol.registry/official";

/// One page of raw entries and the cursor for the next page.
fn get_page(base: &str, query: &str, cursor: Option<&str>) -> Result<(Vec<Value>, Option<String>)> {
    let mut url = url::Url::parse(&format!("{}/servers", base.trim_end_matches('/')))
        .context("MCP registry URL is invalid")?;
    {
        let mut pairs = url.query_pairs_mut();
        if !query.trim().is_empty() {
            pairs.append_pair("search", query.trim());
        }
        pairs.append_pair("limit", &PAGE_SIZE.to_string());
        pairs.append_pair("version", "latest");
        if let Some(cursor) = cursor {
            pairs.append_pair("cursor", cursor);
        }
    }
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut response = agent
        .get(url.as_str())
        .call()
        .map_err(|e| anyhow!("MCP registry request failed: {e}"))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| anyhow!("MCP registry reply could not be read: {e}"))?;
    if !(200..300).contains(&status) {
        bail!("MCP registry returned status {status}");
    }
    let value: Value = serde_json::from_str(&body).context("MCP registry returned invalid JSON")?;
    let entries = value
        .get("servers")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| anyhow!("MCP registry response has no servers array"))?;
    let next = value
        .pointer("/metadata/nextCursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty())
        .map(str::to_owned);
    Ok((entries, next))
}

/// Candidates on page `page` (1-based) of a search, and whether another page follows.
pub fn search_page_at(
    base: &str,
    query: &str,
    page: usize,
    user_dir: &Path,
) -> Result<(Vec<Candidate>, bool)> {
    let page = page.max(1);
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    for _ in 1..page {
        let (_, next) = get_page(base, query, cursor.as_deref())?;
        match next {
            Some(next) => {
                if !seen.insert(next.clone()) {
                    bail!("MCP registry repeated a pagination cursor");
                }
                cursor = Some(next);
            }
            None => return Ok((Vec::new(), false)),
        }
    }
    let (entries, next) = get_page(base, query, cursor.as_deref())?;
    if next.as_ref().is_some_and(|next| seen.contains(next)) {
        bail!("MCP registry repeated a pagination cursor");
    }
    let candidates = entries
        .iter()
        .filter_map(|entry| from_entry(entry, user_dir))
        .collect();
    Ok((candidates, next.is_some()))
}

/// The latest listing with exactly this name, searching cursor pages up to a fixed bound.
pub fn find_at(base: &str, name: &str, user_dir: &Path) -> Result<Option<Candidate>> {
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let (entries, next) = get_page(base, name, cursor.as_deref())?;
        if let Some(found) = entries
            .iter()
            .filter_map(|entry| from_entry(entry, user_dir))
            .find(|candidate| candidate.name == name)
        {
            return Ok(Some(found));
        }
        match next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(None)
}

/// Maps one listing entry. Superseded versions and deleted listings give `None`.
pub fn from_entry(entry: &Value, user_dir: &Path) -> Option<Candidate> {
    let server = entry.get("server").unwrap_or(entry);
    let meta = entry.get("_meta").and_then(|meta| meta.get(LATEST_KEY));
    if meta
        .and_then(|m| m.get("isLatest"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        return None;
    }
    if meta.and_then(|m| m.get("status")).and_then(Value::as_str) == Some("deleted") {
        return None;
    }
    let name = safe_metadata(server.get("name"))?;
    let title = safe_metadata(server.get("title"))
        .unwrap_or_else(|| name.rsplit('/').next().unwrap_or(name.as_str()).to_owned());
    let description =
        safe_metadata(server.get("description")).unwrap_or_else(|| "No description".to_owned());
    let version = safe_metadata(server.get("version"));
    let (install, inputs) = install_for(server, version.as_deref(), user_dir);
    Some(Candidate {
        name,
        title,
        description,
        sources: vec![Source::Official],
        badges: Vec::new(),
        install,
        inputs,
        repository: server
            .pointer("/repository/url")
            .and_then(Value::as_str)
            .and_then(normalize_repository),
    })
}

/// A usable install: the server config and the inputs the user must fill in. `Err` is the reason
/// this option cannot be used as-is.
type Found = Result<(McpServerConfig, Vec<Input>), String>;

/// Packages first (they run locally), then remotes. The first usable option wins; when none is
/// usable, the first reason is shown.
fn install_for(server: &Value, version: Option<&str>, user_dir: &Path) -> (Install, Vec<Input>) {
    let packages = list(server.get("packages"));
    let remotes = list(server.get("remotes"));
    let mut reasons = Vec::new();
    let options = packages
        .iter()
        .filter_map(|package| package_install(package, version, user_dir))
        .chain(remotes.iter().filter_map(remote_install));
    for found in options {
        match found {
            Ok((server, inputs)) => return (Install::Server(Box::new(server)), inputs),
            Err(reason) => reasons.push(reason),
        }
    }
    let reason = reasons
        .into_iter()
        .next()
        .unwrap_or_else(|| "the listing has no package or remote that Dial can run".to_owned());
    (Install::Blocked(reason), Vec::new())
}

fn list(value: Option<&Value>) -> &[Value] {
    value
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// `None` when the package is not a stdio package this module can run.
fn package_install(package: &Value, version: Option<&str>, user_dir: &Path) -> Option<Found> {
    if package
        .pointer("/transport/type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "stdio")
    {
        return None;
    }
    let identifier = safe_metadata(package.get("identifier"))?;
    let pinned = safe_metadata(package.get("version")).or_else(|| version.map(str::to_owned));
    let kind = package.get("registryType").and_then(Value::as_str)?;
    let (program, mut args): (&str, Vec<String>) = match kind {
        // Pinned to the version the registry lists, so a later publish never changes what runs.
        "npm" => (
            "npx",
            vec![
                "-y".to_owned(),
                match &pinned {
                    Some(v) => format!("{identifier}@{v}"),
                    None => identifier.clone(),
                },
            ],
        ),
        "pypi" => (
            "uvx",
            match &pinned {
                Some(v) => vec![
                    "--from".to_owned(),
                    format!("{identifier}=={v}"),
                    identifier.clone(),
                ],
                None => vec![identifier.clone()],
            },
        ),
        "oci" => (
            "docker",
            vec!["run".to_owned(), "-i".to_owned(), "--rm".to_owned()],
        ),
        _ => return None,
    };
    let inputs = env_inputs(package);
    let required_argument = list(package.get("packageArguments"))
        .iter()
        .find(|argument| argument.get("isRequired").and_then(Value::as_bool) == Some(true));
    if let Some(argument) = required_argument {
        let label = safe_metadata(argument.get("name"))
            .or_else(|| safe_metadata(argument.get("valueHint")))
            .unwrap_or_else(|| "an argument".to_owned());
        return Some(Err(format!(
            "the package needs {label}; add it by hand in your config"
        )));
    }
    if kind == "oci" {
        // Docker forwards only the variables it is named, so each input is passed through by name.
        for input in &inputs {
            args.push("-e".to_owned());
            args.push(input.key.clone());
        }
        args.push(identifier);
    }
    Some(Ok((
        McpServerConfig {
            program: PathBuf::from(program),
            args,
            env: cache_env(user_dir),
            enabled: true,
            ..Default::default()
        },
        inputs,
    )))
}

fn env_inputs(package: &Value) -> Vec<Input> {
    list(package.get("environmentVariables"))
        .iter()
        .filter_map(|item| {
            let key = safe_metadata(item.get("name"))?;
            let secret =
                item.get("isSecret").and_then(Value::as_bool) == Some(true) || looks_secret(&key);
            Some(Input {
                description: safe_metadata(item.get("description")).unwrap_or_default(),
                required: item.get("isRequired").and_then(Value::as_bool) == Some(true),
                secret,
                key,
            })
        })
        .collect()
}

/// `None` for remote types other than `streamable-http` and `sse`.
fn remote_install(remote: &Value) -> Option<Found> {
    let transport = match remote.get("type").and_then(Value::as_str)? {
        "streamable-http" => None,
        "sse" => Some(McpTransport::Sse),
        _ => return None,
    };
    let url = safe_metadata(remote.get("url"))?;
    if url.contains('{') {
        return Some(Err(
            "the remote URL has variables to fill in; set it by hand in your config".to_owned(),
        ));
    }
    let required_header = list(remote.get("headers"))
        .iter()
        .find(|header| header.get("isRequired").and_then(Value::as_bool) == Some(true));
    if let Some(header) = required_header {
        let name = safe_metadata(header.get("name")).unwrap_or_else(|| "a".to_owned());
        return Some(Err(format!(
            "the remote needs a {name} header; add it by hand under headers in your config"
        )));
    }
    Some(Ok((
        McpServerConfig {
            url: Some(url),
            transport,
            enabled: true,
            ..Default::default()
        },
        Vec::new(),
    )))
}

#[cfg(test)]
mod tests;
