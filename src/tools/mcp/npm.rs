//! npm as an MCP listing: packages tagged `mcp` whose manifest declares a command, run with `npx`
//! pinned to the listed version. Reads listings only.
//!
//! npm metadata does not say which environment values a server needs, so npm rows have no inputs;
//! a server that cannot start without them fails the install health check and nothing is saved.
//! Endpoints used: `GET /-/v1/search?text=<query> keywords:mcp&size&from` and
//! `GET /<package>/latest` (the manifest, for `bin` and `repository`).
use super::catalog::{Candidate, Install, Source, cache_env, normalize_repository};
use super::fetch::{self, pick_command, plain_name, plain_version};
use super::smithery::safe_metadata;
use crate::config::types::McpServerConfig;
use anyhow::{Context, Result, anyhow, ensure};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const URL: &str = "https://registry.npmjs.org";
/// Listing names are `npm:<package>`, so they never collide with curated ids or registry names.
pub const PREFIX: &str = "npm:";
const PAGE_SIZE: usize = 20;
const KEYWORD: &str = "keywords:mcp";

/// Candidates on page `page` (1-based) of a search, and whether another page follows. Packages
/// whose manifest declares no command are libraries, not servers, and are left out.
pub fn search_page_at(
    base: &str,
    query: &str,
    page: usize,
    user_dir: &Path,
) -> Result<(Vec<Candidate>, bool)> {
    let from = (page.max(1) - 1) * PAGE_SIZE;
    let text = match query.trim() {
        "" => KEYWORD.to_owned(),
        query => format!("{query} {KEYWORD}"),
    };
    let mut url = url::Url::parse(&format!("{}/-/v1/search", base.trim_end_matches('/')))
        .context("npm registry URL is invalid")?;
    url.query_pairs_mut()
        .append_pair("text", &text)
        .append_pair("size", &PAGE_SIZE.to_string())
        .append_pair("from", &from.to_string());
    let body = fetch::get_json(url.as_str(), &[], "npm search")?
        .ok_or_else(|| anyhow!("npm search returned status 404"))?;
    let hits = body
        .get("objects")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("npm search reply has no objects array"))?;
    let total = body.get("total").and_then(Value::as_u64).unwrap_or(0);
    let more = !hits.is_empty() && ((from + hits.len()) as u64) < total;
    // Search hits carry no `bin`, so each manifest is read, concurrently, to keep only servers.
    let candidates = std::thread::scope(|scope| {
        let handles: Vec<_> = hits
            .iter()
            .filter_map(|hit| hit.get("package"))
            .map(|package| scope.spawn(move || listing(base, package, user_dir)))
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok().flatten())
            .collect()
    });
    Ok((candidates, more))
}

/// The candidate for one search hit. A manifest that cannot be read right now still lists the
/// package; installing reads it again.
fn listing(base: &str, package: &Value, user_dir: &Path) -> Option<Candidate> {
    let name = safe_metadata(package.get("name")).filter(|name| valid_package(name))?;
    match manifest(base, &name) {
        Ok(Some(manifest)) => from_manifest(&manifest, user_dir),
        Ok(None) => None,
        Err(_) => Some(Candidate {
            name: format!("{PREFIX}{name}"),
            title: name,
            description: safe_metadata(package.get("description"))
                .unwrap_or_else(|| "No description".to_owned()),
            sources: vec![Source::Npm],
            badges: Vec::new(),
            install: Install::Deferred("reads the package's command from npm".to_owned()),
            inputs: Vec::new(),
            repository: package
                .pointer("/links/repository")
                .and_then(Value::as_str)
                .and_then(normalize_repository),
        }),
    }
}

/// The latest published manifest of `name`, `None` when the package does not exist.
pub fn manifest(base: &str, name: &str) -> Result<Option<Value>> {
    ensure!(
        valid_package(name),
        "`{name}` is not a valid npm package name"
    );
    fetch::get_json(
        &format!("{}/{name}/latest", base.trim_end_matches('/')),
        &[],
        "npm registry",
    )
}

/// Maps a manifest. `None` when it declares no command, so it is not a server `npx` can start.
pub fn from_manifest(manifest: &Value, user_dir: &Path) -> Option<Candidate> {
    let name = safe_metadata(manifest.get("name")).filter(|name| valid_package(name))?;
    let version = safe_metadata(manifest.get("version")).filter(|v| plain_version(v))?;
    let unscoped = name.rsplit('/').next().unwrap_or(&name).to_owned();
    let commands = commands(manifest.get("bin"), &unscoped);
    if commands.is_empty() {
        return None;
    }
    // Pinned so a later publish never changes what runs.
    let spec = format!("{name}@{version}");
    let install = match commands.as_slice() {
        [_] => Install::Server(Box::new(npx(vec!["-y".into(), spec], user_dir))),
        _ => match pick_command(&commands, &[&unscoped]) {
            Ok(command) => Install::Server(Box::new(npx(
                vec!["-y".into(), format!("--package={spec}"), command.to_owned()],
                user_dir,
            ))),
            Err(listed) => Install::Blocked(format!(
                "the package has several commands ({listed}) and none is clearly its MCP server; add the right one by hand with `npx -y --package={spec} <command>`"
            )),
        },
    };
    let badges = match manifest.get("deprecated") {
        Some(Value::String(_)) | Some(Value::Bool(true)) => vec!["deprecated".to_owned()],
        _ => Vec::new(),
    };
    Some(Candidate {
        name: format!("{PREFIX}{name}"),
        title: name,
        description: safe_metadata(manifest.get("description"))
            .unwrap_or_else(|| "No description".to_owned()),
        sources: vec![Source::Npm],
        badges,
        install,
        inputs: Vec::new(),
        repository: repository(manifest),
    })
}

/// The candidate to install for `package`, read from its current manifest.
pub fn resolve(base: &str, package: &str, user_dir: &Path) -> Result<Candidate> {
    let manifest =
        manifest(base, package)?.ok_or_else(|| anyhow!("`{package}` is not published on npm"))?;
    from_manifest(&manifest, user_dir).ok_or_else(|| {
        anyhow!("`{package}` declares no command to run, so it is not an MCP server Dial can start")
    })
}

/// The manifest's source repository as a plain URL (`repository` is a string or `{url}`).
pub fn repository(manifest: &Value) -> Option<String> {
    let raw = match manifest.get("repository")? {
        Value::String(url) => url.as_str(),
        other => other.get("url")?.as_str()?,
    };
    normalize_repository(raw)
}

/// Command names from `bin`: a string runs as the unscoped package name, an object names each.
fn commands(bin: Option<&Value>, unscoped: &str) -> Vec<String> {
    let mut found: Vec<String> = match bin {
        Some(Value::String(path)) if !path.trim().is_empty() => vec![unscoped.to_owned()],
        Some(Value::Object(map)) => map.keys().cloned().collect(),
        _ => Vec::new(),
    };
    found.retain(|command| plain_name(command));
    found
}

fn npx(args: Vec<String>, user_dir: &Path) -> McpServerConfig {
    McpServerConfig {
        program: PathBuf::from("npx"),
        args,
        env: cache_env(user_dir),
        enabled: true,
        ..Default::default()
    }
}

/// `name` or `@scope/name`, each part a plain registry name, so it is safe in a URL path and as
/// one `npx` argument.
pub fn valid_package(name: &str) -> bool {
    let part = |part: &str| {
        !part.is_empty()
            && !part.starts_with(['.', '_', '-'])
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
    };
    name.len() <= 214
        && match name.strip_prefix('@') {
            Some(scoped) => scoped
                .split_once('/')
                .is_some_and(|(scope, rest)| part(scope) && part(rest)),
            None => part(name),
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user() -> &'static Path {
        Path::new("/home/u/.dial")
    }

    #[test]
    fn a_single_command_runs_pinned_through_npx() {
        let manifest = json!({"name": "context-mode", "version": "1.0.169",
            "description": "MCP plugin", "bin": {"context-mode": "cli.bundle.mjs"},
            "repository": {"type": "git", "url": "git+https://github.com/mksglu/context-mode.git"}});
        let found = from_manifest(&manifest, user()).expect("has a command");
        assert_eq!(found.name, "npm:context-mode");
        assert_eq!(found.sources, vec![Source::Npm]);
        assert_eq!(
            found.repository.as_deref(),
            Some("https://github.com/mksglu/context-mode")
        );
        let Install::Server(server) = found.install else {
            panic!("one command is installable");
        };
        assert_eq!(server.program, PathBuf::from("npx"));
        assert_eq!(server.args, ["-y", "context-mode@1.0.169"]);
        assert!(server.env.contains_key("npm_config_cache"));
    }

    #[test]
    fn several_commands_name_the_server_one_or_block_with_the_list() {
        let named = json!({"name": "@acme/tool", "version": "2.0.0",
            "bin": {"tool-cli": "a.js", "tool-mcp": "b.js"}});
        let Install::Server(server) = from_manifest(&named, user()).unwrap().install else {
            panic!("the mcp command is picked");
        };
        assert_eq!(
            server.args,
            ["-y", "--package=@acme/tool@2.0.0", "tool-mcp"]
        );

        let unclear = json!({"name": "multi", "version": "1.0.0",
            "bin": {"one": "a.js", "two": "b.js"}, "deprecated": "use other"});
        let found = from_manifest(&unclear, user()).unwrap();
        assert!(!found.installable());
        assert!(found.install_summary().contains("one, two"));
        assert_eq!(found.badges, vec!["deprecated".to_owned()]);
    }

    #[test]
    fn libraries_and_unsafe_metadata_are_not_listed() {
        let library = json!({"name": "@modelcontextprotocol/sdk", "version": "1.0.0"});
        assert!(from_manifest(&library, user()).is_none());
        let bad_version = json!({"name": "x", "version": "1 || 2", "bin": "x.js"});
        assert!(from_manifest(&bad_version, user()).is_none());
        let bad_command = json!({"name": "x", "version": "1.0.0", "bin": {"--eval": "x.js"}});
        assert!(from_manifest(&bad_command, user()).is_none());
    }

    #[test]
    fn package_names_are_plain_or_scoped() {
        assert!(valid_package("context-mode"));
        assert!(valid_package("@upstash/context7-mcp"));
        assert!(!valid_package("@scope"));
        assert!(!valid_package("../etc"));
        assert!(!valid_package("a/b"));
        assert!(!valid_package("-y"));
    }
}
