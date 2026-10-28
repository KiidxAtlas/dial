//! One install shape for every MCP listing: the built-in curated list, the official MCP Registry,
//! Smithery, npm and GitHub. Discovery only reads listings. Nothing here starts a server or writes
//! config; callers (the `/extensions` browser and the `mcp_servers` agent tool) run the health
//! check and save the returned [`Change`](crate::config::settings_store::Change) themselves,
//! after approval.
//!
//! Secrets are never invented: a server's required environment variables come back as [`Input`]s
//! that the user must fill in, and a remote that needs a header this module cannot supply is
//! [`Install::Blocked`] with the reason.
use super::{github, npm, registry, smithery};
use crate::config::types::{Config, McpServerConfig};
use anyhow::{Result, anyhow, bail};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// How a curated server is pointed at the project.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectRoot {
    /// The server does not use the project.
    Unused,
    /// The project root is appended as the last argument (filesystem and git servers).
    LastArg,
    /// The server runs with the project root as its working directory and finds its own files
    /// there (Graphify reads `graphify-out/graph.json`). The entry saves `cwd = "."`, which the
    /// client resolves against the project Dial opened, so one entry serves every project.
    WorkingDir,
}

/// A built-in MCP server. It runs through `runtime`, which must be on PATH.
pub struct Curated {
    pub id: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    /// Runtime that must be on PATH; also the program that runs the server.
    pub runtime: &'static str,
    pub args: &'static [&'static str],
    pub project: ProjectRoot,
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
        project: ProjectRoot::Unused,
        tools: "list-documentation-pages, get-documentation-page, list-modules, get-module and more",
        env: &[],
    },
    Curated {
        id: "memory",
        title: "Memory",
        summary: "persistent knowledge graph across sessions",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-memory"],
        project: ProjectRoot::Unused,
        tools: "create_entities, create_relations, search_nodes, read_graph",
        env: &[],
    },
    Curated {
        id: "filesystem",
        title: "Filesystem",
        summary: "read and write files under the project root",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-filesystem"],
        project: ProjectRoot::LastArg,
        tools: "read_file, write_file, list_directory, search_files",
        env: &[],
    },
    Curated {
        id: "sequential-thinking",
        title: "Sequential thinking",
        summary: "structured step-by-step reasoning tool",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-sequential-thinking"],
        project: ProjectRoot::Unused,
        tools: "sequentialthinking",
        env: &[],
    },
    Curated {
        id: "fetch",
        title: "Fetch",
        summary: "download web pages as markdown",
        runtime: "uvx",
        args: &["mcp-server-fetch"],
        project: ProjectRoot::Unused,
        tools: "fetch",
        env: &[],
    },
    Curated {
        id: "time",
        title: "Time",
        summary: "current time and timezone conversion",
        runtime: "uvx",
        args: &["mcp-server-time"],
        project: ProjectRoot::Unused,
        tools: "get_current_time, convert_time",
        env: &[],
    },
    Curated {
        id: "everything",
        title: "Everything",
        summary: "reference server that exercises every MCP feature (for testing Dial)",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-everything"],
        project: ProjectRoot::Unused,
        tools: "echo, add, longRunningOperation, getTinyImage and more",
        env: &[],
    },
    Curated {
        id: "git",
        title: "Git",
        summary: "read history, diffs and status of the project's git repository",
        runtime: "uvx",
        args: &["mcp-server-git", "--repository"],
        project: ProjectRoot::LastArg,
        tools: "git_status, git_diff, git_log, git_add, git_commit and more",
        env: &[],
    },
    // Local stdio server from the graphifyy PyPI package (`mcp` extra):
    // https://github.com/Graphify-Labs/graphify. It starts without a graph and loads
    // graphify-out/graph.json (or $GRAPHIFY_OUT) from its working directory on each call.
    Curated {
        id: "graphify",
        title: "Graphify",
        summary: "query a local code knowledge graph (build it first: graphify extract . --code-only)",
        runtime: "uvx",
        args: &["--from", "graphifyy[mcp]", "graphify-mcp"],
        project: ProjectRoot::WorkingDir,
        tools: "query_graph, get_node, get_neighbors, shortest_path, god_nodes and more",
        env: &[],
    },
    Curated {
        id: "playwright",
        title: "Playwright",
        summary: "drive a real browser: navigate, click, type and screenshot",
        runtime: "npx",
        args: &["-y", "@playwright/mcp@latest"],
        project: ProjectRoot::Unused,
        tools: "browser_navigate, browser_click, browser_snapshot, browser_take_screenshot",
        env: &[],
    },
    Curated {
        id: "context7",
        title: "Context7",
        summary: "current library documentation and code examples",
        runtime: "npx",
        args: &["-y", "@upstash/context7-mcp"],
        project: ProjectRoot::Unused,
        tools: "resolve-library-id, get-library-docs",
        env: &[],
    },
    Curated {
        id: "puppeteer",
        title: "Puppeteer",
        summary: "headless Chrome: navigate, screenshot and run scripts (archived upstream)",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-puppeteer"],
        project: ProjectRoot::Unused,
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
        project: ProjectRoot::Unused,
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
        project: ProjectRoot::Unused,
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
        project: ProjectRoot::Unused,
        tools: "brave_web_search, brave_local_search",
        env: &[("BRAVE_API_KEY", "Brave Search API key", true)],
    },
    Curated {
        id: "slack",
        title: "Slack",
        summary: "read channels and post messages (needs a bot token; archived upstream)",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-slack"],
        project: ProjectRoot::Unused,
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
        project: ProjectRoot::Unused,
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

/// Where listings are read from. Tests point them at a loopback fixture.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub official: String,
    pub smithery: String,
    pub npm: String,
    pub pypi: String,
    pub github_api: String,
    pub github_raw: String,
}

impl Endpoints {
    pub fn live() -> Self {
        Self {
            official: registry::OFFICIAL_URL.to_owned(),
            smithery: smithery::registry_url(),
            npm: npm::URL.to_owned(),
            pypi: "https://pypi.org/pypi".to_owned(),
            github_api: github::API_URL.to_owned(),
            github_raw: github::RAW_URL.to_owned(),
        }
    }

    /// Every listing on one base URL, for tests that serve them all from one fixture.
    pub fn at(base: &str) -> Self {
        Self {
            official: base.to_owned(),
            smithery: base.to_owned(),
            npm: base.to_owned(),
            pypi: base.to_owned(),
            github_api: base.to_owned(),
            github_raw: base.to_owned(),
        }
    }
}

/// Where a candidate was listed. The first source of a merged candidate decides how it installs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Official,
    Smithery,
    Npm,
    Github,
    Curated,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Official => "official",
            Source::Smithery => "smithery",
            Source::Npm => "npm",
            Source::Github => "github",
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
    Server(Box<McpServerConfig>),
    /// Decided by [`resolve`] when installing, from details the listing does not carry. The text
    /// says what is looked up.
    Deferred(String),
    /// Cannot be installed from here. The reason is shown to the user.
    Blocked(String),
}

/// One server from any listing, in the shape every browser and tool uses.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// Listing name: registry name, curated id, Smithery qualified name, `npm:<package>` or
    /// `github:<owner>/<repo>`.
    pub name: String,
    pub title: String,
    pub description: String,
    /// Listings that returned this server, first one first.
    pub sources: Vec<Source>,
    /// Extra badges such as `verified`.
    pub badges: Vec<String>,
    pub install: Install,
    pub inputs: Vec<Input>,
    /// Source repository URL, when the listing names one.
    pub repository: Option<String>,
}

impl Candidate {
    pub fn origin(&self) -> Source {
        self.sources.first().copied().unwrap_or(Source::Official)
    }

    /// Whether installing can be attempted: a ready install or one [`resolve`] decides.
    pub fn installable(&self) -> bool {
        matches!(self.install, Install::Server(_) | Install::Deferred(_))
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
            Install::Server(server) => (**server).clone(),
            Install::Deferred(_) => bail!("`{}` must be resolved before it installs", self.name),
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
            Install::Deferred(lookup) => format!("decided when installing: {lookup}"),
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
    key == "openapi_mcp_headers"
        || key == "authorization"
        || ["key", "token", "secret", "password"]
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
    let name = name.trim();
    let name = [npm::PREFIX, github::PREFIX]
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))
        .unwrap_or(name);
    let last = name.rsplit('/').next().unwrap_or("");
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

/// Why a health check found no tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckFailure {
    /// The server answered HTTP 401 with an OAuth (Bearer) challenge. It is reachable and only
    /// needs `/mcp login <name>`, which requires a config entry, so installers save it anyway.
    SignIn(String),
    /// Anything else: the server did not start, did not answer, or listed no tools.
    Failed(String),
}

impl std::fmt::Display for CheckFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SignIn(why) | Self::Failed(why) => f.write_str(why),
        }
    }
}

/// Starts `server` once and counts its tools. `Err` when it lists none or fails to start;
/// [`CheckFailure::SignIn`] when it only lacks an OAuth sign-in.
pub fn check(
    config: &Config,
    name: &str,
    server: &McpServerConfig,
) -> std::result::Result<usize, CheckFailure> {
    // An explicit health check tests disabled entries without enabling them in saved config.
    let mut server = server.clone();
    server.enabled = true;
    let specs = BTreeMap::from([(name.to_string(), server)]);
    let load = super::connect_extra(&specs, config);
    let count = load.live.offered_tools().len();
    if count == 0 {
        let why = load
            .notices
            .first()
            .cloned()
            .unwrap_or_else(|| "the server listed no tools".into());
        return Err(if load.sign_in.iter().any(|signed| signed == name) {
            CheckFailure::SignIn(why)
        } else {
            CheckFailure::Failed(why)
        });
    }
    Ok(count)
}

// ---- Curated ----------------------------------------------------------------------------------

/// The candidate for a curated entry. Its install is ready; required env entries are inputs.
pub fn curated_candidate(item: &Curated, user_dir: &Path, project_root: &Path) -> Candidate {
    let mut args: Vec<String> = item.args.iter().map(|s| s.to_string()).collect();
    if item.project == ProjectRoot::LastArg {
        args.push(project_root.display().to_string());
    }
    let server = McpServerConfig {
        program: PathBuf::from(item.runtime),
        args,
        env: cache_env(user_dir),
        cwd: (item.project == ProjectRoot::WorkingDir).then(|| PathBuf::from(".")),
        enabled: true,
        ..Default::default()
    };
    Candidate {
        name: item.id.to_owned(),
        title: item.title.to_owned(),
        description: item.summary.to_owned(),
        sources: vec![Source::Curated],
        badges: Vec::new(),
        install: Install::Server(Box::new(server)),
        repository: None,
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
/// and `io.github.` removed, lowercased. Bare curated ids and unscoped npm packages have no owner,
/// so they never match an unrelated listing that happens to end in the same word.
pub fn dedupe_key(name: &str) -> String {
    let name = name.trim();
    let name = name.strip_prefix(github::PREFIX).unwrap_or(name);
    if let Some(package) = name.strip_prefix(npm::PREFIX)
        && !package.starts_with('@')
    {
        return name.to_ascii_lowercase();
    }
    let name = name.strip_prefix(npm::PREFIX).unwrap_or(name);
    name.trim_start_matches('@')
        .trim_start_matches("io.github.")
        .to_ascii_lowercase()
}

/// The package a stdio install runs as a whole (`npx -y <pkg>[@v]`, `npx -y --package=<pkg> <cmd>`,
/// `uvx [--from <pkg>[extras]==v] <cmd>`), so listings that run the same package fold together.
/// Installs with further arguments (a bridge to a URL, a project root) are not a package identity.
fn package_key(server: &McpServerConfig) -> Option<String> {
    let args: Vec<&str> = server.args.iter().map(String::as_str).collect();
    let program = server.program.to_str()?;
    let (kind, package) = match (program, args.as_slice()) {
        ("npx", ["-y", spec]) => ("npm", unversioned(spec)),
        ("npx", ["-y", spec, _]) => ("npm", unversioned(spec.strip_prefix("--package=")?)),
        // Extras (`graphifyy[mcp]`) select optional dependencies of the same package.
        ("uvx", ["--from", spec, _]) => ("pypi", spec.split(['=', '[']).next()?),
        ("uvx", [package]) if !package.starts_with('-') => ("pypi", *package),
        _ => return None,
    };
    Some(format!("package:{kind}:{}", package.to_ascii_lowercase()))
}

/// `name@version` or `@scope/name@version` without the version.
fn unversioned(spec: &str) -> &str {
    let start = usize::from(spec.starts_with('@'));
    match spec[start..].find('@') {
        Some(at) => &spec[..start + at],
        None => spec,
    }
}

/// `https://<host>/<owner>/<repo>` for a repository given as a git URL, a page inside the
/// repository, or npm's `github:owner/repo` and `owner/repo` shorthands. `None` when it names no
/// owner and repository.
pub fn normalize_repository(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let raw = raw.strip_prefix("git+").unwrap_or(raw);
    let shorthand = raw
        .strip_prefix("github:")
        .or_else(|| (!raw.contains(':')).then_some(raw));
    let url = if let Some(path) = shorthand {
        format!("https://github.com/{path}")
    } else if let Some(path) = raw.strip_prefix("git@") {
        format!("https://{}", path.replacen(':', "/", 1))
    } else {
        raw.replacen("ssh://git@", "https://", 1)
            .replacen("git://", "https://", 1)
    };
    let url = url::Url::parse(&url).ok()?;
    if !matches!(url.scheme(), "https" | "http") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let mut segments = url.path_segments()?.filter(|segment| !segment.is_empty());
    let owner = segments.next()?;
    let repo = segments.next()?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    let plain = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (plain(owner) && plain(repo)).then(|| format!("https://{host}/{owner}/{repo}"))
}

/// Folds listings of the same server into one candidate. The first listing keeps its install;
/// later ones add their source (and badges) to it. Listings match on their owner-qualified name
/// or the package their install runs. A GitHub row also matches the one listing that names its
/// repository; a repository named by several listings holds several servers, so it matches none.
pub fn merge(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut repositories: HashMap<String, Option<usize>> = HashMap::new();
    for candidate in candidates {
        let mut keys = vec![dedupe_key(&candidate.name)];
        if let Install::Server(server) = &candidate.install {
            keys.extend(package_key(server));
        }
        let repository = candidate.repository.as_deref().map(str::to_ascii_lowercase);
        let github = candidate.origin() == Source::Github;
        let found = keys
            .iter()
            .find_map(|key| index.get(key).copied())
            .or_else(|| {
                github
                    .then(|| repositories.get(repository.as_deref()?).copied().flatten())
                    .flatten()
            });
        let slot = match found {
            Some(slot) => {
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
                if existing.repository.is_none() {
                    existing.repository = candidate.repository;
                }
                slot
            }
            None => {
                out.push(candidate);
                out.len() - 1
            }
        };
        for key in keys {
            index.entry(key).or_insert(slot);
        }
        if let Some(repository) = repository
            && !github
        {
            repositories
                .entry(repository)
                .and_modify(|owner| {
                    if *owner != Some(slot) {
                        *owner = None;
                    }
                })
                .or_insert(Some(slot));
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

/// Official registry, Smithery, npm and GitHub, read concurrently and merged in that order, then
/// curated. Curated entries appear on the first page only.
pub fn discover(endpoints: &Endpoints, config: &Config, query: &str, page: usize) -> Discovery {
    let page = page.max(1);
    let query = query.trim();
    let user_dir = &config.user_dir;
    let mut found = Vec::new();
    let mut notices = Vec::new();
    let mut more = false;
    let key = match smithery::resolve_api_key(user_dir) {
        Ok(key) => key,
        Err(error) => {
            notices.push(format!("Smithery key: {error:#}"));
            None
        }
    };
    let key = key.as_deref();
    let pages = std::thread::scope(|scope| {
        [
            (
                "official registry",
                scope
                    .spawn(|| registry::search_page_at(&endpoints.official, query, page, user_dir)),
            ),
            (
                "Smithery",
                scope.spawn(|| {
                    smithery::browse_page_at(&endpoints.smithery, query, page, key)
                        .map_err(|error| anyhow!(error.message))
                }),
            ),
            (
                "npm",
                scope.spawn(|| npm::search_page_at(&endpoints.npm, query, page, user_dir)),
            ),
            (
                "GitHub",
                scope.spawn(|| github::search_page_at(&endpoints.github_api, query, page)),
            ),
        ]
        .map(|(label, handle)| {
            let result = handle
                .join()
                .unwrap_or_else(|_| Err(anyhow!("the listing request panicked")));
            (label, result)
        })
    });
    for (label, result) in pages {
        match result {
            Ok((rows, next)) => {
                found.extend(rows);
                more |= next;
            }
            Err(error) => notices.push(format!("{label}: {error:#}")),
        }
    }
    if page == 1 {
        found.extend(curated_matches(query, user_dir, &config.project_root));
    }
    let mut candidates = merge(found);
    rank(&mut candidates, query);
    Discovery {
        candidates,
        more,
        notices,
    }
}

/// Moves rows whose name is the query to the top, then rows whose name contains it, keeping
/// each listing's own order within those groups. Loose provider matches (Smithery matches
/// descriptions) would otherwise push an exact npm or GitHub match far down the page.
fn rank(candidates: &mut [Candidate], query: &str) {
    let query = normalized(query);
    if query.is_empty() {
        return;
    }
    candidates.sort_by_key(|candidate| {
        let names = [normalized(&candidate.title), config_name(&candidate.name)];
        if names.contains(&query) {
            0
        } else if names.iter().any(|name| name.contains(&query)) {
            1
        } else {
            2
        }
    });
}

fn normalized(text: &str) -> String {
    text.trim()
        .to_ascii_lowercase()
        .replace(['_', '.', ' '], "-")
}

/// The candidate to install for `id`: a curated id, an `npm:` package, a `github:` repository, or
/// an exact official registry name. Smithery names are not resolved here; they install through
/// their own settings form.
pub fn resolve(endpoints: &Endpoints, config: &Config, id: &str) -> Result<Candidate> {
    let id = id.trim();
    if let Some(package) = id.strip_prefix(npm::PREFIX) {
        return npm::resolve(&endpoints.npm, package, &config.user_dir);
    }
    if let Some(repository) = id.strip_prefix(github::PREFIX) {
        return github::resolve(endpoints, repository, &config.user_dir);
    }
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
                "`{id}` is not in the official MCP registry or the curated list. Search first, then use the exact id of a result."
            )
        },
    )
}

#[cfg(test)]
mod tests;
