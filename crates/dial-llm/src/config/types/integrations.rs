//! External programs and services: lifecycle hooks, MCP servers and the web tools.
use super::*;

/// A program run at a lifecycle event. Trusted and unsandboxed, like MCP servers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    /// Lifecycle event; see `config::hooks::EVENTS` (alias names such as `tool_call` are accepted).
    pub event: String,
    /// Anchored regex on the tool name (tool events only); omitted matches every tool.
    #[serde(default)]
    pub matcher: Option<String>,
    /// Program and arguments; the event payload arrives as JSON on stdin.
    pub command: Vec<String>,
    /// Seconds before the hook is killed (default 30); 0 disables the deadline.
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
    /// Whether a crash, timeout or start failure blocks the action. Defaults to true for
    /// `pre_tool_use` (policy hooks) and false for every other event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_closed: Option<bool>,
}

fn default_hook_timeout() -> u64 {
    30
}

/// An MCP server: a stdio program or a streamable-HTTP endpoint. Like `[tools.*]` it
/// is a trusted program: it runs unsandboxed. See the "HTTP MCP" section of README.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpServerConfig {
    /// Stdio server executable. Exactly one of `program` and `url` is set.
    pub program: PathBuf,
    /// Remote endpoint (https, or http on localhost).
    pub url: Option<String>,
    /// Wire protocol for `url` servers: `http` (streamable HTTP, the default) or `sse` (legacy
    /// HTTP+SSE: the event stream announces where requests are POSTed). Stdio servers leave it unset or `stdio`.
    pub transport: Option<McpTransport>,
    /// Name of an environment variable holding a bearer token for `url` servers.
    pub bearer_env: Option<String>,
    /// Extra non-secret request headers for `url` servers.
    pub headers: BTreeMap<String, String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Working directory for stdio servers; defaults to dial's own. A relative path resolves
    /// against the project root, so `"."` runs the server in whichever project Dial opened.
    pub cwd: Option<PathBuf>,
    /// Per request timeout; 0 waits until the server answers or the call is cancelled (Esc).
    pub timeout_secs: u64,
    /// Budget for spawn/handshake/tool listing. Connections run in the background either way.
    pub connect_timeout_secs: u64,
    /// Let tools annotated `readOnlyHint: true` run without approval (and in read-only subagents). Default off: every MCP tool requires approval unless this is set.
    pub trust_read_only_hint: bool,
    /// Set false to keep the entry without starting it.
    pub enabled: bool,
    /// OAuth 2.1 settings for `url` servers that require authorization.
    pub oauth: McpOAuthConfig,
    /// Lifecycle events to forward to this server as `notifications/dial/event`.
    pub lifecycle_hooks: Vec<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            program: PathBuf::new(),
            url: None,
            transport: None,
            bearer_env: None,
            headers: BTreeMap::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            timeout_secs: 120,
            connect_timeout_secs: 30,
            trust_read_only_hint: false,
            enabled: true,
            oauth: McpOAuthConfig::default(),
            lifecycle_hooks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
    Sse,
}

/// OAuth for one remote MCP server. Discovery (RFC 9728 + RFC 8414) and dynamic client
/// registration need no settings; a pre-registered client sets `client_id`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpOAuthConfig {
    /// Pre-registered client id; empty uses dynamic client registration.
    pub client_id: Option<String>,
    /// Environment variable holding the pre-registered client's secret, if it has one.
    pub client_secret_env: Option<String>,
    /// Space-separated scopes; empty uses what the server advertises.
    pub scopes: Option<String>,
    /// Loopback callback port; 0 picks a free port.
    pub callback_port: u16,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    /// Search endpoint override; empty uses the built-in default provider.
    pub search_url: String,
    /// Environment variable holding the search endpoint's API key, when it needs one.
    pub search_api_key_env: Option<String>,
    /// Reader chain for HTML pages, tried in order after the markdown alternates. Known readers:
    /// `native`, `trafilatura`, `lynx`, `jina` (jina also needs `jina_reader = true`). Empty uses
    /// `native`, `trafilatura`, `lynx`.
    pub reader_chain: Vec<String>,
    /// Allow the `jina` reader, which sends the page URL to r.jina.ai. Off by default.
    pub jina_reader: bool,
    /// Search backend: `auto` (default; empty also means auto) tries every provider whose
    /// environment key is set, then the keyless DuckDuckGo, Startpage, Mojeek and Ecosia, falling
    /// through on failure. Or name one: brave, tavily, exa, serper, kagi, jina, perplexity, google,
    /// bing, searxng, firecrawl, parallel, zai, xai, duckduckgo, startpage, mojeek, ecosia.
    /// Ignored when `search_url` is set.
    pub search_provider: String,
}
