//! One install shape for every MCP listing: the built-in curated list, the official MCP Registry
//! and Smithery. Discovery only reads listings. Nothing here starts a server or writes config;
//! callers (the `/extensions` browser and the `mcp_servers` agent tool) run the health check and
//! save the returned [`Change`](crate::storage::settings_store::Change) themselves, after approval.
//!
//! Secrets are never invented: a server's required environment variables come back as [`Input`]s
//! that the user must fill in, and a remote that needs a header this module cannot supply is
//! [`Install::Blocked`] with the reason.
use super::{registry, smithery};
use crate::config::types::{Config, McpServerConfig};
use anyhow::{Result, anyhow, bail};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// A built-in MCP server. It runs through `runtime`, which must be on PATH.
pub struct Curated {
    pub id: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    /// Runtime that must be on PATH; also the program that runs the server.
    pub runtime: &'static str,
    pub args: &'static [&'static str],
    /// Whether the project root is appended as the last argument (filesystem and git servers).
    pub takes_root: bool,
    /// Tools the server exposes, for the details pane.
    pub tools: &'static str,
    /// Environment variables the server reads: `(name, description, required)`.
    pub env: &'static [(&'static str, &'static str, bool)],
}

/// Curated servers. Package names were checked against the npm and PyPI registries; packages marked
/// archived upstream still install but are no longer maintained.
pub const CURATED: &[Curated] = &[
    // Official stdio setup: https://nuxt.com/docs/4.x/guide/ai/mcp
    Curated {
        id: "nuxt",
        title: "Nuxt",
        summary: "official Nuxt documentation, modules, blog posts and deployment guides",
        runtime: "npx",
        args: &["-y", "mcp-remote", "https://nuxt.com/mcp"],
        takes_root: false,
        tools: "list-documentation-pages, get-documentation-page, list-modules, get-module and more",
        env: &[],
    },
    Curated {
        id: "memory",
        title: "Memory",
        summary: "persistent knowledge graph across sessions",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-memory"],
        takes_root: false,
        tools: "create_entities, create_relations, search_nodes, read_graph",
        env: &[],
    },
    Curated {
        id: "filesystem",
        title: "Filesystem",
        summary: "read and write files under the project root",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-filesystem"],
        takes_root: true,
        tools: "read_file, write_file, list_directory, search_files",
        env: &[],
    },
    Curated {
        id: "sequential-thinking",
        title: "Sequential thinking",
        summary: "structured step-by-step reasoning tool",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-sequential-thinking"],
        takes_root: false,
        tools: "sequentialthinking",
        env: &[],
    },
    Curated {
        id: "fetch",
        title: "Fetch",
        summary: "download web pages as markdown",
        runtime: "uvx",
        args: &["mcp-server-fetch"],
        takes_root: false,
        tools: "fetch",
        env: &[],
    },
    Curated {
        id: "time",
        title: "Time",
        summary: "current time and timezone conversion",
        runtime: "uvx",
        args: &["mcp-server-time"],
        takes_root: false,
        tools: "get_current_time, convert_time",
        env: &[],
    },
    Curated {
        id: "everything",
        title: "Everything",
        summary: "reference server that exercises every MCP feature (for testing Dial)",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-everything"],
        takes_root: false,
        tools: "echo, add, longRunningOperation, getTinyImage and more",
        env: &[],
    },
    Curated {
        id: "git",
        title: "Git",
        summary: "read history, diffs and status of the project's git repository",
        runtime: "uvx",
        args: &["mcp-server-git", "--repository"],
        takes_root: true,
        tools: "git_status, git_diff, git_log, git_add, git_commit and more",
        env: &[],
    },
    Curated {
        id: "playwright",
        title: "Playwright",
        summary: "drive a real browser: navigate, click, type and screenshot",
        runtime: "npx",
        args: &["-y", "@playwright/mcp@latest"],
        takes_root: false,
        tools: "browser_navigate, browser_click, browser_snapshot, browser_take_screenshot",
        env: &[],
    },
    Curated {
        id: "context7",
        title: "Context7",
        summary: "current library documentation and code examples",
        runtime: "npx",
        args: &["-y", "@upstash/context7-mcp"],
        takes_root: false,
        tools: "resolve-library-id, get-library-docs",
        env: &[],
    },
    Curated {
        id: "puppeteer",
        title: "Puppeteer",
        summary: "headless Chrome: navigate, screenshot and run scripts (archived upstream)",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-puppeteer"],
        takes_root: false,
        tools: "puppeteer_navigate, puppeteer_screenshot, puppeteer_click, puppeteer_evaluate",
        env: &[],
    },
    Curated {
        id: "github",
        title: "GitHub",
        summary: "issues, pull requests, files and code search (needs a token)",
        runtime: "docker",
        args: &[
            "run",
            "-i",
            "--rm",
            "-e",
            "GITHUB_PERSONAL_ACCESS_TOKEN",
            "ghcr.io/github/github-mcp-server",
        ],
        takes_root: false,
        tools: "search_code, issue_read, create_pull_request, get_file_contents and more",
        env: &[(
            "GITHUB_PERSONAL_ACCESS_TOKEN",
            "GitHub personal access token",
            true,
        )],
    },
    Curated {
        id: "notion",
        title: "Notion",
        summary: "search and edit Notion pages and databases (needs an integration token)",
        runtime: "npx",
        args: &["-y", "@notionhq/notion-mcp-server"],
        takes_root: false,
        tools: "API-post-search, API-retrieve-a-page, API-patch-block-children and more",
        env: &[(
            "OPENAPI_MCP_HEADERS",
            "JSON with Authorization: Bearer <integration token> and Notion-Version",
            true,
        )],
    },
    Curated {
        id: "brave-search",
        title: "Brave search",
        summary: "web and local search through the Brave Search API (needs a key; archived upstream)",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-brave-search"],
        takes_root: false,
        tools: "brave_web_search, brave_local_search",
        env: &[("BRAVE_API_KEY", "Brave Search API key", true)],
    },
    Curated {
        id: "slack",
        title: "Slack",
        summary: "read channels and post messages (needs a bot token; archived upstream)",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-slack"],
        takes_root: false,
        tools: "slack_list_channels, slack_post_message, slack_get_channel_history",
        env: &[
            ("SLACK_BOT_TOKEN", "Slack bot token (xoxb-...)", true),
            ("SLACK_TEAM_ID", "Slack workspace ID", true),
        ],
    },
    Curated {
        id: "gitlab",
        title: "GitLab",
        summary: "projects, merge requests and issues (needs a token; archived upstream)",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-gitlab"],
        takes_root: false,
        tools: "create_merge_request, list_issues, get_file_contents and more",
        env: &[
            (
                "GITLAB_PERSONAL_ACCESS_TOKEN",
                "GitLab personal access token",
                true,
            ),
            ("GITLAB_API_URL", "API URL of a self-hosted GitLab", false),
        ],
    },
];

/// Where listings are read from. Tests point both at a loopback fixture.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub official: String,
    pub smithery: String,
}

impl Endpoints {
    pub fn live() -> Self {
        Self {
            official: registry::OFFICIAL_URL.to_owned(),
            smithery: smithery::registry_url(),
        }
    }
}

/// Where a candidate was listed. The first source of a merged candidate decides how it installs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Official,
    Smithery,
    Curated,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Official => "official",
            Source::Smithery => "smithery",
            Source::Curated => "curated",
        }
    }
}

/// An environment variable or setting the user must supply before the server can start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub key: String,
    pub description: String,
    pub required: bool,
    /// Shown masked while typing.
    pub secret: bool,
}

/// How a candidate runs, decided from its listing alone.
#[derive(Clone, Debug)]
pub enum Install {
    /// A stdio program or a remote URL. Required inputs are filled in as environment entries.
    Server(McpServerConfig),
    /// Cannot be installed from here. The reason is shown to the user.
    Blocked(String),
}

/// One server from any listing, in the shape every browser and tool uses.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// Listing name: registry name, curated id, or Smithery qualified name.
    pub name: String,
    pub title: String,
    pub description: String,
    /// Listings that returned this server, first one first.
    pub sources: Vec<Source>,
    /// Extra badges such as `verified`.
    pub badges: Vec<String>,
    pub install: Install,
    pub inputs: Vec<Input>,
}

impl Candidate {
    pub fn origin(&self) -> Source {
        self.sources.first().copied().unwrap_or(Source::Official)
    }

    pub fn installable(&self) -> bool {
        matches!(self.install, Install::Server(_))
    }

    /// The first required input with no value in `values` (blank counts as missing).
    pub fn missing_required(&self, values: &BTreeMap<String, String>) -> Option<&Input> {
        self.inputs.iter().find(|input| {
            input.required
                && values
                    .get(&input.key)
                    .is_none_or(|value| value.trim().is_empty())
        })
    }

    /// The server config to save: the install with each non-blank supplied value as an env entry.
    pub fn server(&self, values: &BTreeMap<String, String>) -> Result<McpServerConfig> {
        let mut server = match &self.install {
            Install::Server(server) => server.clone(),
            Install::Blocked(reason) => bail!("{reason}"),
        };
        if let Some(input) = self.missing_required(values) {
            bail!(
                "`{}` is required before `{}` can start",
                input.key,
                self.name
            );
        }
        for input in &self.inputs {
            if let Some(value) = values
                .get(&input.key)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
            {
                server.env.insert(input.key.clone(), value.to_owned());
            }
        }
        Ok(server)
    }

    /// One line for the details pane and the agent: the command or URL, or why it is blocked.
    pub fn install_summary(&self) -> String {
        match &self.install {
            Install::Server(server) => command_line(server),
            Install::Blocked(reason) => format!("not installable here: {reason}"),
        }
    }
}

/// `program arg arg`, or the URL of a remote server.
pub fn command_line(server: &McpServerConfig) -> String {
    match &server.url {
        Some(url) => url.clone(),
        None => std::iter::once(server.program.display().to_string())
            .chain(server.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// Whether a setting name holds a credential, so its value is masked and never echoed.
pub fn looks_secret(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["key", "token", "secret", "password"]
        .iter()
        .any(|word| key.contains(word))
}

/// `<user_dir>/mcp`, where package caches are redirected so nothing lands in a global cache.
pub fn mcp_cache_dir(user_dir: &Path) -> PathBuf {
    user_dir.join("mcp")
}

/// Environment that keeps npm and uv caches under Dial's user directory.
pub fn cache_env(user_dir: &Path) -> BTreeMap<String, String> {
    let cache = mcp_cache_dir(user_dir);
    BTreeMap::from([
        (
            "npm_config_cache".to_string(),
            cache.join("npm").display().to_string(),
        ),
        (
            "UV_CACHE_DIR".to_string(),
            cache.join("uv").display().to_string(),
        ),
    ])
}

/// The config table name for a listing name: the last path segment, lowercased, with only
/// `[a-z0-9_-]` kept. Dots would make a TOML path nest, so they are never kept.
pub fn config_name(name: &str) -> String {
    let last = name.trim().rsplit('/').next().unwrap_or("");
    let mut out = String::new();
    for c in last.chars().map(|c| c.to_ascii_lowercase()) {
        let c = if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            c
        } else {
            '-'
        };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "mcp-server".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Finds an executable on PATH without running it.
pub fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Starts `server` once and counts its tools. `Err(why)` when it lists none or fails to start.
pub fn check(
    project_root: &Path,
    name: &str,
    server: &McpServerConfig,
) -> std::result::Result<usize, String> {
    let specs = BTreeMap::from([(name.to_string(), server.clone())]);
    let load = super::connect_extra(&specs, project_root);
    let count = load.tools.len();
    if count == 0 {
        return Err(load
            .notices
            .first()
            .cloned()
            .unwrap_or_else(|| "the server listed no tools".into()));
    }
    Ok(count)
}

// ---- Curated ----------------------------------------------------------------------------------

/// The candidate for a curated entry. Its install is ready; required env entries are inputs.
pub fn curated_candidate(item: &Curated, user_dir: &Path, project_root: &Path) -> Candidate {
    let mut args: Vec<String> = item.args.iter().map(|s| s.to_string()).collect();
    if item.takes_root {
        args.push(project_root.display().to_string());
    }
    let server = McpServerConfig {
        program: PathBuf::from(item.runtime),
        args,
        env: cache_env(user_dir),
        enabled: true,
        ..Default::default()
    };
    Candidate {
        name: item.id.to_owned(),
        title: item.title.to_owned(),
        description: item.summary.to_owned(),
        sources: vec![Source::Curated],
        badges: Vec::new(),
        install: Install::Server(server),
        inputs: item
            .env
            .iter()
            .map(|(key, description, required)| Input {
                key: (*key).to_owned(),
                description: (*description).to_owned(),
                required: *required,
                secret: looks_secret(key),
            })
            .collect(),
    }
}

/// Curated entries whose id, title or summary contains `query` (all of them when empty).
pub fn curated_matches(query: &str, user_dir: &Path, project_root: &Path) -> Vec<Candidate> {
    let needle = query.trim().to_lowercase();
    CURATED
        .iter()
        .filter(|item| {
            needle.is_empty()
                || [item.id, item.title, item.summary]
                    .iter()
                    .any(|text| text.to_lowercase().contains(&needle))
        })
        .map(|item| curated_candidate(item, user_dir, project_root))
        .collect()
}

// ---- Merge ------------------------------------------------------------------------------------

/// Owner-qualified identity used to spot one server listed twice: `owner/name` with any leading `@`
/// and `io.github.` removed, lowercased. Bare curated ids have no owner, so they never match
/// an unrelated listing that happens to end in the same word.
pub fn dedupe_key(name: &str) -> String {
    name.trim()
        .trim_start_matches('@')
        .trim_start_matches("io.github.")
        .to_ascii_lowercase()
}

/// Folds listings of the same server into one candidate. The first listing keeps its install;
/// later ones add their source (and badges) to it.
pub fn merge(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for candidate in candidates {
        let key = dedupe_key(&candidate.name);
        match index.get(&key) {
            Some(&slot) => {
                let existing = &mut out[slot];
                for source in candidate.sources {
                    if !existing.sources.contains(&source) {
                        existing.sources.push(source);
                    }
                }
                for badge in candidate.badges {
                    if !existing.badges.contains(&badge) {
                        existing.badges.push(badge);
                    }
                }
            }
            None => {
                index.insert(key, out.len());
                out.push(candidate);
            }
        }
    }
    out
}

// ---- Discovery and resolution -----------------------------------------------------------------

/// One page of merged results. A listing that fails only adds a notice; the others still show.
#[derive(Debug, Default)]
pub struct Discovery {
    pub candidates: Vec<Candidate>,
    pub more: bool,
    pub notices: Vec<String>,
}

/// Official registry, then Smithery, then curated. Curated entries appear on the first page only.
pub fn discover(endpoints: &Endpoints, config: &Config, query: &str, page: usize) -> Discovery {
    let page = page.max(1);
    let query = query.trim();
    let mut found = Vec::new();
    let mut notices = Vec::new();
    let mut more = false;
    match registry::search_page_at(&endpoints.official, query, page, &config.user_dir) {
        Ok((rows, next)) => {
            found.extend(rows);
            more |= next;
        }
        Err(error) => notices.push(format!("official registry: {error:#}")),
    }
    let key = match smithery::resolve_api_key(&config.user_dir) {
        Ok(key) => key,
        Err(error) => {
            notices.push(format!("Smithery key: {error:#}"));
            None
        }
    };
    match smithery::browse_page_at(&endpoints.smithery, query, page, key.as_deref()) {
        Ok((rows, next)) => {
            found.extend(rows);
            more |= next;
        }
        Err(error) => notices.push(format!("Smithery: {}", error.message)),
    }
    if page == 1 {
        found.extend(curated_matches(
            query,
            &config.user_dir,
            &config.project_root,
        ));
    }
    Discovery {
        candidates: merge(found),
        more,
        notices,
    }
}

/// The candidate to install for `id`: a curated id first, then an exact official registry name.
/// Smithery names are not resolved here; they install through their own settings form.
pub fn resolve(endpoints: &Endpoints, config: &Config, id: &str) -> Result<Candidate> {
    let id = id.trim();
    if let Some(item) = CURATED.iter().find(|item| item.id == id) {
        return Ok(curated_candidate(
            item,
            &config.user_dir,
            &config.project_root,
        ));
    }
    registry::find_at(&endpoints.official, id, &config.user_dir)?.ok_or_else(
        || {
            anyhow!(
                "`{id}` is not in the official MCP registry or the curated list. Search first, then use the exact name."
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio(program: &str) -> McpServerConfig {
        McpServerConfig {
            program: PathBuf::from(program),
            ..Default::default()
        }
    }

    fn candidate(name: &str, source: Source, inputs: Vec<Input>) -> Candidate {
        Candidate {
            name: name.into(),
            title: name.into(),
            description: String::new(),
            sources: vec![source],
            badges: Vec::new(),
            install: Install::Server(stdio("npx")),
            inputs,
        }
    }

    #[test]
    fn curated_ids_are_unique_and_run_through_a_known_runtime() {
        let mut ids: Vec<&str> = CURATED.iter().map(|c| c.id).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total);
        for item in CURATED {
            assert!(
                matches!(item.runtime, "npx" | "uvx" | "docker"),
                "{}",
                item.id
            );
            assert_eq!(
                item.id,
                config_name(item.id),
                "{} must be a valid table name",
                item.id
            );
        }
    }

    #[test]
    fn nuxt_is_discoverable_without_registry_access_and_resolves_for_install() {
        let config = Config::default();
        let found = curated_matches("NuXt", &config.user_dir, &config.project_root);
        assert_eq!(found.len(), 1);
        let candidate = resolve(
            &Endpoints {
                official: "invalid://unused".into(),
                smithery: "invalid://unused".into(),
            },
            &config,
            &found[0].name,
        )
        .unwrap();
        let server = candidate.server(&BTreeMap::new()).unwrap();
        assert_eq!(server.program, PathBuf::from("npx"));
        assert_eq!(server.args, ["-y", "mcp-remote", "https://nuxt.com/mcp"]);
        assert!(candidate.inputs.is_empty());
    }

    #[test]
    fn curated_required_env_is_an_input_and_blank_values_are_missing() {
        let item = CURATED.iter().find(|c| c.id == "github").unwrap();
        let candidate = curated_candidate(item, Path::new("/u"), Path::new("/p"));
        assert_eq!(candidate.inputs.len(), 1);
        assert!(candidate.inputs[0].secret && candidate.inputs[0].required);
        let blank =
            BTreeMap::from([("GITHUB_PERSONAL_ACCESS_TOKEN".to_string(), "  ".to_string())]);
        assert!(candidate.missing_required(&blank).is_some());
        assert!(candidate.server(&BTreeMap::new()).is_err());
        let filled = BTreeMap::from([(
            "GITHUB_PERSONAL_ACCESS_TOKEN".to_string(),
            " tok ".to_string(),
        )]);
        let server = candidate.server(&filled).unwrap();
        assert_eq!(
            server
                .env
                .get("GITHUB_PERSONAL_ACCESS_TOKEN")
                .map(String::as_str),
            Some("tok")
        );
    }

    #[test]
    fn filesystem_curated_candidate_appends_the_project_root() {
        let item = CURATED.iter().find(|c| c.id == "filesystem").unwrap();
        let candidate = curated_candidate(item, Path::new("/u"), Path::new("/work"));
        let Install::Server(server) = candidate.install else {
            panic!("curated servers are installable");
        };
        assert_eq!(server.args.last().map(String::as_str), Some("/work"));
        assert_eq!(
            server.env.get("UV_CACHE_DIR").map(String::as_str),
            Some("/u/mcp/uv")
        );
    }

    #[test]
    fn config_names_never_contain_dots_or_slashes() {
        assert_eq!(config_name("memory"), "memory");
        assert_eq!(
            config_name("io.github.github/github-mcp-server"),
            "github-mcp-server"
        );
        assert_eq!(config_name("@Acme/Notes.v2"), "notes-v2");
        assert_eq!(config_name("a//b  c"), "b-c");
        assert_eq!(config_name("///"), "mcp-server");
    }

    #[test]
    fn dedupe_matches_owner_qualified_names_only() {
        assert_eq!(
            dedupe_key("io.github.github/github-mcp-server"),
            dedupe_key("@github/github-mcp-server")
        );
        assert_ne!(dedupe_key("github"), dedupe_key("io.github.someone/github"));
    }

    #[test]
    fn merge_keeps_the_first_install_and_adds_sources_once() {
        let official = candidate("io.github.acme/notes", Source::Official, vec![]);
        let mut smithery = candidate("@acme/notes", Source::Smithery, vec![]);
        smithery.badges.push("verified".into());
        let other = candidate("acme-other", Source::Curated, vec![]);
        let again = candidate("@acme/notes", Source::Smithery, vec![]);
        let merged = merge(vec![official, smithery, other, again]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "io.github.acme/notes");
        assert_eq!(merged[0].sources, vec![Source::Official, Source::Smithery]);
        assert_eq!(merged[0].badges, vec!["verified".to_string()]);
        assert_eq!(merged[1].origin(), Source::Curated);
    }

    #[test]
    fn install_summary_names_the_command_or_the_blocker() {
        let server = McpServerConfig {
            program: PathBuf::from("uvx"),
            args: vec!["mcp-server-git".into(), "--repository".into()],
            ..Default::default()
        };
        let mut c = candidate("git", Source::Curated, vec![]);
        c.install = Install::Server(server);
        assert_eq!(c.install_summary(), "uvx mcp-server-git --repository");
        c.install = Install::Blocked("needs an Authorization header".into());
        assert!(c.install_summary().contains("Authorization"));
        assert!(!c.installable());
    }

    #[test]
    fn secret_names_are_detected_loosely() {
        assert!(looks_secret("SLACK_BOT_TOKEN"));
        assert!(looks_secret("apiKey"));
        assert!(!looks_secret("GITLAB_API_URL"));
    }
}
