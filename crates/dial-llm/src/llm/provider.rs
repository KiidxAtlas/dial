//! Where completions come from: a local `mlx_lm.server`, an OpenAI-compatible
//! endpoint, or Anthropic's Messages API.
//!
//! Endpoint rules (see [`endpoint_url`]):
//! * `base_url` must be `http` or `https`, with no userinfo, query or fragment
//!   (credentials belong in `api_key`; they would otherwise leak into logs).
//! * Plain `http` is accepted only for local hosts (loopback, private ranges,
//!   `localhost`, `.local`) unless the backend was built
//!   [`with_allow_insecure`](Backend::with_allow_insecure): a bearer token must
//!   not cross the internet in clear text.
//! * The request goes to `{base_url}/chat/completions`; a bare
//!   `scheme://host[:port]` gets `/v1` inserted first, so `https://api.openai.com/v1`
//!   and `http://localhost:11434` both work.
//!
//! Neither prompts nor the API key appear in any error produced here.

pub(super) mod compaction;
pub use compaction::{NativeCompactionFailure, NativeCompactionReply};

use super::model_config::ApiProtocol;
use super::server::{self, ModelServer};
use super::{self as llm, Chat, Reply, SERVER_MODEL};
use anyhow::{Context, Result, bail, ensure};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

const RETRY_BACKOFF: Duration = Duration::from_secs(1);
/// `config.json` files larger than this are not read for the context hint.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
/// Shown once per conversation when llama.cpp's `/props` `chat_template_caps` say the model's chat
/// template cannot render tools natively; llama.cpp's own log warns its fallback may do badly.
const LLAMA_TOOL_TEMPLATE_NOTICE: &str = "llama-server reports this model's chat template has no native tool support (/props chat_template_caps), so tool calls may be unreliable. Restart it with --jinja --chat-template-file <a tool-use template for this model>; see llama.cpp docs/function-calling.md.";

#[derive(Clone, PartialEq, Eq)]
pub enum ProviderKind {
    /// Dial's own ChatGPT OAuth registration, refreshed per request.
    ChatGpt { model: String, user_dir: PathBuf },
    /// A local model directory served by a supervised `mlx_lm.server`.
    Local { model_dir: String },
    /// A remote (or self-hosted) OpenAI-compatible chat endpoint.
    Endpoint {
        base_url: String,
        model: String,
        api_key: Option<String>,
    },
    /// Anthropic's native Messages API endpoint.
    AnthropicEndpoint {
        base_url: String,
        model: String,
        api_key: Option<String>,
    },
    /// Claude subscription OAuth refreshed before each request.
    AnthropicSubscription { model: String, user_dir: PathBuf },
    /// Apple's on-device system model through Dial's FoundationModels helper (`llm::apple`).
    Apple,
}
// Not derived: the key must never reach a log through `{:?}`.
impl fmt::Debug for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderKind::ChatGpt { model, .. } => {
                f.debug_struct("ChatGpt").field("model", model).finish()
            }
            ProviderKind::AnthropicSubscription { model, .. } => f
                .debug_struct("AnthropicSubscription")
                .field("model", model)
                .finish(),
            ProviderKind::Local { model_dir } => f
                .debug_struct("Local")
                .field("model_dir", model_dir)
                .finish(),
            ProviderKind::Endpoint {
                base_url,
                model,
                api_key,
            } => f
                .debug_struct("Endpoint")
                .field("base_url", &llm::display_url(base_url))
                .field("model", model)
                .field("api_key", &api_key.as_ref().map(|_| "<redacted>"))
                .finish(),
            ProviderKind::AnthropicEndpoint {
                base_url,
                model,
                api_key,
            } => f
                .debug_struct("AnthropicEndpoint")
                .field("base_url", &llm::display_url(base_url))
                .field("model", model)
                .field("api_key", &api_key.as_ref().map(|_| "<redacted>"))
                .finish(),
            ProviderKind::Apple => f.write_str("Apple"),
        }
    }
}

/// Validate an endpoint `base_url` and return the chat-completions URL.
pub fn endpoint_url(base_url: &str, allow_insecure: bool) -> Result<String> {
    // Unresolved built-in templates name the config setting that would fill them.
    if base_url.contains("<account>") {
        bail!(
            "endpoint base_url has an unset <account> placeholder: set snowflake_account (Snowflake Cortex) or cloudflare_account_id (Cloudflare AI Gateway)"
        );
    }
    if base_url.contains("<gateway>") {
        bail!(
            "endpoint base_url has an unset <gateway> placeholder: set cloudflare_gateway_id (Cloudflare AI Gateway)"
        );
    }
    // Messages below never quote `base_url`: it may carry userinfo.
    let uri: ureq::http::Uri = base_url
        .parse()
        .map_err(|_| anyhow::anyhow!("endpoint base_url is not a valid URL"))?;
    let scheme = uri.scheme_str().unwrap_or_default();
    ensure!(
        scheme == "http" || scheme == "https",
        "endpoint base_url must start with http:// or https://"
    );
    let Some(authority) = uri.authority() else {
        bail!("endpoint base_url has no host")
    };
    ensure!(
        !authority.as_str().contains('@'),
        "endpoint base_url must not contain credentials; use the api key setting"
    );
    ensure!(
        uri.query().is_none(),
        "endpoint base_url must not contain a query string"
    );
    let host = authority.host();
    ensure!(!host.is_empty(), "endpoint base_url has no host");
    if scheme == "http" && !allow_insecure && !llm::is_local_host(host) {
        bail!(
            "refusing plain http to non-local host {host}: use https, or opt in explicitly to insecure endpoints"
        );
    }
    Ok(llm::completions_url(base_url))
}

/// Validate an Anthropic base URL and return its Messages API URL.
pub fn anthropic_url(base_url: &str, allow_insecure: bool) -> Result<String> {
    let completions = endpoint_url(base_url, allow_insecure)?;
    let prefix = completions
        .strip_suffix("/chat/completions")
        .context("invalid Anthropic endpoint URL")?;
    Ok(format!("{prefix}/messages"))
}

/// Which exchange function carries a request; derived from the configured [`ApiProtocol`] by
/// [`Wire::of`] so a new protocol must be routed deliberately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    ChatCompletions,
    Anthropic,
    /// ChatGPT subscription backend (Codex Responses over WebSocket or HTTP).
    Codex,
    /// Responses API over HTTP.
    Responses,
    Gemini,
    Ollama,
    /// Everything served through `llm::transports`.
    Transport,
}

impl Wire {
    fn of(protocol: ApiProtocol) -> Self {
        match protocol {
            ApiProtocol::ChatCompletions => Self::ChatCompletions,
            ApiProtocol::Anthropic => Self::Anthropic,
            ApiProtocol::Responses => Self::Responses,
            ApiProtocol::Gemini => Self::Gemini,
            ApiProtocol::Ollama => Self::Ollama,
            ApiProtocol::Bedrock
            | ApiProtocol::Vertex
            | ApiProtocol::VertexAnthropic
            | ApiProtocol::AzureResponses
            | ApiProtocol::Copilot
            | ApiProtocol::CloudCodeAssist
            | ApiProtocol::Antigravity
            | ApiProtocol::GitlabDuo
            | ApiProtocol::FactoryDroid
            | ApiProtocol::DevinAgent
            | ApiProtocol::Cursor => Self::Transport,
        }
    }
}

/// Validate a provider without contacting it (for config loading).
pub fn validate(kind: &ProviderKind, allow_insecure: bool) -> Result<()> {
    match kind {
        ProviderKind::ChatGpt { model, user_dir } => {
            ensure!(
                !model.trim().is_empty() && !user_dir.as_os_str().is_empty(),
                "ChatGPT requires a model and private user storage"
            );
            Ok(())
        }
        ProviderKind::AnthropicSubscription { model, user_dir } => {
            ensure!(
                !model.trim().is_empty() && !user_dir.as_os_str().is_empty(),
                "Anthropic subscription requires a model and private user storage"
            );
            Ok(())
        }
        ProviderKind::Local { model_dir } => {
            ensure!(
                !model_dir.is_empty(),
                "local model directory must not be empty"
            );
            Ok(())
        }
        ProviderKind::Endpoint {
            base_url,
            model,
            api_key,
        }
        | ProviderKind::AnthropicEndpoint {
            base_url,
            model,
            api_key,
        } => {
            ensure!(!model.trim().is_empty(), "endpoint model must not be empty");
            if let Some(key) = api_key {
                ensure!(
                    !key.is_empty() && key.bytes().all(|b| b.is_ascii_graphic()),
                    "api key is empty or contains characters that are not allowed in an HTTP header"
                );
            }
            endpoint_url(base_url, allow_insecure).map(drop)
        }
        ProviderKind::Apple => Ok(()),
    }
}

/// A grounded local or bundled remote context window.
pub fn context_window_hint(kind: &ProviderKind) -> Option<u32> {
    match kind {
        ProviderKind::ChatGpt { model, .. }
        | ProviderKind::AnthropicSubscription { model, .. }
        | ProviderKind::Endpoint { model, .. }
        | ProviderKind::AnthropicEndpoint { model, .. } => {
            llm::catalog::known_context_window(model)
        }
        ProviderKind::Local { model_dir } => local_context_window(Path::new(model_dir)),
        ProviderKind::Apple => Some(llm::apple::availability().context_size),
    }
}

fn local_context_window(model_dir: &Path) -> Option<u32> {
    let mut text = String::new();
    std::fs::File::open(model_dir.join("config.json"))
        .ok()?
        .take(MAX_CONFIG_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    let config: serde_json::Value = serde_json::from_str(&text).ok()?;
    [
        config.pointer("/max_position_embeddings"),
        config.pointer("/text_config/max_position_embeddings"),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| value.as_u64())
    .and_then(|window| u32::try_from(window).ok())
    .filter(|&window| window > 0)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AdapterPhase {
    #[default]
    Idle,
    Loading,
    Loaded,
    Used,
    Failed,
}

#[derive(Clone, Debug, Default)]
pub struct AdapterRuntime {
    pub adapter: Option<PathBuf>,
    pub phase: AdapterPhase,
}

pub type AdapterMonitor = Arc<parking_lot::Mutex<AdapterRuntime>>;

/// The inputs `catalog::resolve_credential` needs to read an endpoint's key again at each dispatch.
struct CredentialSource {
    user_dir: PathBuf,
    base_url: String,
    api_key_env: Option<String>,
}

impl CredentialSource {
    /// The key `resolve_credential` gives for this endpoint right now.
    fn resolve(&self) -> Result<Option<String>> {
        self.resolve_with(self.api_key_env.as_deref())
    }

    /// Whether `variable` currently yields a key for this endpoint.
    fn has_key(&self, variable: &str) -> bool {
        llm::key_pool::env_present(variable)
            && matches!(self.resolve_with(Some(variable)), Ok(Some(_)))
    }

    /// The key `resolve_credential` gives for this endpoint when read through `variable`.
    fn resolve_with(&self, variable: Option<&str>) -> Result<Option<String>> {
        llm::catalog::resolve_credential(&self.user_dir, &self.base_url, variable)
    }
}

/// One-shot controls retained by a logical request's caller across rejected inference attempts.
/// Does not capture credentials, cancellation, usage or provider response/transport state.
pub struct RequestControls {
    profile: Option<llm::profiles::ModelProfile>,
    local_hints: Option<llm::local_hints::LocalHints>,
    reasoning_guard: Option<crate::config::types::ReasoningGuardConfig>,
    failover: Option<crate::config::types::FallbackOn>,
}

pub struct Backend {
    #[cfg(any(test, feature = "test-support"))]
    pub chatgpt_url: Option<String>,
    kind: ProviderKind,
    python: PathBuf,
    startup_timeout_secs: u64,
    allow_insecure: bool,
    pub retry_backoff: Duration,
    remote_idle_timeout_secs: u64,
    server: Option<ModelServer>,
    adapter_monitor: AdapterMonitor,
    metadata: llm::catalog::ModelMetadata,
    profile: Option<llm::profiles::ModelProfile>,
    next_profile: Option<llm::profiles::ModelProfile>,
    protocol: Option<ApiProtocol>,
    headers: llm::model_config::RequestHeaders,
    next_reasoning_guard: Option<crate::config::types::ReasoningGuardConfig>,
    /// A fallback model is ready for the next request; see [`Backend::next_request_failover`].
    next_failover: Option<crate::config::types::FallbackOn>,
    steering_config: Option<crate::config::types::Config>,
    steering_runtime: serde_json::Value,
    usage_scope: Option<llm::usage::Scope>,
    account_id: Option<String>,
    prompt_cache_key: Option<String>,
    session_identity_error: Option<String>,
    /// Provider OAuth grant resolved (and refreshed) before every endpoint attempt.
    oauth: Option<llm::oauth::Binding>,
    /// Where a key-authenticated endpoint reads its key at each dispatch; `None` for other routes.
    credential: Option<CredentialSource>,
    /// Several API keys for the same endpoint; the active one replaces `credential`'s variable.
    key_pool: Option<llm::key_pool::KeyPool>,
    /// Stable conversation identity for provider proxy attribution and live steering.
    session_id: Option<String>,
    /// A generated transport correlation ID is not a persistent conversation binding.
    /// Set only when an owner supplies a session ID or binds session state.
    session_bound: bool,
    codex_websocket: bool,
    image_notices: llm::images::NoticeTracker,
    /// The llama.cpp tool-template notice was shown in this conversation.
    tool_template_noticed: bool,
    wire: llm::wire::Shared,
    /// Provider-bound secret masking (`[secrets]`); `None` when off.
    secrets: Option<Arc<llm::secrets::Masker>>,
    /// Test-only idle limit for this backend's model calls, carried to the worker thread that runs them.
    #[cfg(any(test, feature = "test-support"))]
    idle_limit: Option<Duration>,
    /// Persistent local hints for every request (`set_local_hints`).
    local_hints: llm::local_hints::LocalHints,
    /// Hints for the next request only (`next_local_hints`), consumed at the request boundary.
    next_local_hints: Option<llm::local_hints::LocalHints>,
    /// Merged hints the previous request used; the base of a loop-penalty retry.
    last_local_hints: llm::local_hints::LocalHints,
    /// Penalty hints for loop recovery (`set_loop_penalty`); `None` disables retries.
    loop_penalty: Option<llm::local_hints::LocalHints>,
    loop_penalty_retries: u32,
}

/// The conversation state a backend binds to. The storage layer's `Session` implements it, so the
/// provider never depends on session persistence.
pub trait BoundSession {
    fn id(&self) -> &str;
    fn messages(&self) -> Vec<serde_json::Value>;
    fn control_state(&self) -> serde_json::Value;
    fn set_control_state(&mut self, state: serde_json::Value) -> Result<()>;
}

impl Backend {
    pub fn wire_observation(&self) -> Option<llm::wire::Observation> {
        self.wire.lock().latest()
    }
    pub fn new(kind: ProviderKind, python: PathBuf, startup_timeout_secs: u64) -> Self {
        let account = match &kind {
            ProviderKind::ChatGpt { user_dir, .. } => {
                Some(llm::chatgpt::resolve_account(user_dir, None))
            }
            _ => None,
        };
        let account_id = account.as_ref().and_then(|a| a.as_ref().ok()).cloned();
        let session_identity_error = account.and_then(|a| a.err()).map(|e| format!("{e:#}"));
        Backend {
            #[cfg(any(test, feature = "test-support"))]
            chatgpt_url: None,
            kind,
            python,
            startup_timeout_secs,
            allow_insecure: false,
            retry_backoff: RETRY_BACKOFF,
            remote_idle_timeout_secs: crate::config::types::DEFAULT_REMOTE_IDLE_TIMEOUT_SECS,
            server: None,
            adapter_monitor: Default::default(),
            metadata: Default::default(),
            profile: None,
            protocol: None,
            headers: Vec::new(),
            next_profile: None,
            local_hints: Default::default(),
            next_local_hints: None,
            last_local_hints: Default::default(),
            loop_penalty: None,
            loop_penalty_retries: 0,
            next_reasoning_guard: None,
            next_failover: None,
            steering_config: None,
            steering_runtime: serde_json::json!({"state":"disabled","uses":0}),
            usage_scope: None,
            account_id,
            session_id: llm::factory_droid::new_request_id().ok(),
            session_bound: false,
            prompt_cache_key: None,
            session_identity_error,
            oauth: None,
            credential: None,
            key_pool: None,
            codex_websocket: false,
            image_notices: Default::default(),
            tool_template_noticed: false,
            wire: Default::default(),
            secrets: None,
            #[cfg(any(test, feature = "test-support"))]
            idle_limit: None,
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
    pub fn with_codex_websocket(mut self, enabled: bool) -> Self {
        self.codex_websocket = enabled;
        self
    }
    /// Session key of a Codex WebSocket response that may accept live steering; `None` means
    /// steering must be journaled and replayed locally. Matches the key `exchange` registers.
    pub fn live_steering_key(&self) -> Option<String> {
        self.codex_websocket.then(|| {
            self.session_id
                .clone()
                .unwrap_or_else(|| "codex-default".into())
        })
    }
    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }
    pub fn require_session_identity(&self) -> Result<()> {
        if let Some(error) = &self.session_identity_error {
            bail!("{error}");
        }
        Ok(())
    }

    pub fn with_account(mut self, account_id: String) -> Self {
        self.account_id = Some(account_id);
        self.session_identity_error = None;
        self
    }
    /// Bind proxy attribution to an owning conversation. Replacements and fallback backends
    /// must copy this identifier rather than generate a new one.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        if self.session_id.as_deref() != Some(&session_id) {
            self.image_notices = Default::default();
            self.tool_template_noticed = false;
            self.wire = Default::default();
        }
        self.session_id = Some(session_id);
        self.session_bound = true;
        self
    }

    fn cache_namespace_key(
        &self,
        namespace: &str,
        config: &crate::config::types::Config,
    ) -> Result<String> {
        use sha2::{Digest, Sha256};
        let selection = config.selection();
        let scope = serde_json::json!([
            "dial-cache-v1",
            namespace,
            selection.provider,
            selection.base_url,
            selection
                .protocol
                .unwrap_or_else(|| ApiProtocol::builtin(&selection.provider)),
            self.account_id,
            config.request_headers()
        ]);
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(&scope)?)))
    }

    pub fn with_cache_namespace(
        mut self,
        namespace: &str,
        config: &crate::config::types::Config,
    ) -> Result<Self> {
        self.prompt_cache_key = Some(self.cache_namespace_key(namespace, config)?);
        Ok(self)
    }

    /// Borrow a private request namespace without rebinding persistent session identity.
    pub fn swap_cache_namespace(
        &mut self,
        namespace: &str,
        config: &crate::config::types::Config,
    ) -> Result<Option<String>> {
        let key = self.cache_namespace_key(namespace, config)?;
        Ok(self.prompt_cache_key.replace(key))
    }

    pub fn restore_cache_namespace(&mut self, previous: Option<String>) {
        self.prompt_cache_key = previous;
    }
    /// Session state stores public identity and a one-way namespace hash, never auth contents.
    pub fn bind_session(
        &mut self,
        config: &crate::config::types::Config,
        session: &mut impl BoundSession,
    ) -> Result<()> {
        if self.session_id.as_deref() != Some(session.id()) {
            self.image_notices = Default::default();
            self.tool_template_noticed = false;
            self.wire = Default::default();
        }
        self.session_id = Some(session.id().to_owned());
        self.session_bound = true;
        use sha2::{Digest, Sha256};
        let mut state = session.control_state();
        if !state.is_object() {
            state = serde_json::json!({});
        }
        let previous = &state["provider_identity"];
        let mut selection = config.selection();
        if let ProviderKind::ChatGpt { user_dir, .. } = &self.kind {
            let remembered = previous["selection"]["account_id"].as_str();
            let requested = remembered
                .or(selection.account_id.as_deref())
                .or(self.account_id.as_deref());
            let id = llm::chatgpt::resolve_account(user_dir, requested)?;
            ensure!(
                config
                    .chatgpt_allowed_accounts
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&id)),
                "ChatGPT account is restricted by private global policy"
            );
            if let Some(old) = remembered {
                ensure!(
                    old == id
                        || !session.messages().iter().any(|m| m["dial_provider_content"]
                            .as_array()
                            .is_some_and(|items| items
                                .iter()
                                .any(|i| i["type"] == "dial_response_item"))),
                    "ChatGPT account changed with native replay history; start a clean chat"
                );
            }
            self.account_id = Some(id.clone());
            selection.account_id = Some(id);
            if config.active_model_profile().is_none() {
                self.profile = llm::chatgpt::scoped_model_profile(
                    user_dir,
                    self.account_id.as_deref(),
                    &selection.model,
                )?;
            }
            self.metadata = llm::catalog::ModelMetadata {
                vision: self.profile.as_ref().and_then(|p| p.capabilities.vision),
                ..Default::default()
            };
        }
        let history_generation = state["provider_identity"]["history_generation"]
            .as_u64()
            .unwrap_or(0);
        let scope = serde_json::json!([
            "dial-cache-v1",
            session.id(),
            &selection.provider,
            &selection.base_url,
            selection
                .protocol
                .unwrap_or_else(|| ApiProtocol::builtin(&selection.provider)),
            &selection.account_id,
            config.request_headers(),
            history_generation
        ]);
        let key = format!("{:x}", Sha256::digest(serde_json::to_vec(&scope)?));
        self.prompt_cache_key = Some(key.clone());
        let role_accounts = state["provider_identity"]["role_accounts"].clone();
        state["provider_identity"] = serde_json::json!({"selection":selection,"cache_key":key,
            "role_accounts":role_accounts,"history_generation":history_generation});
        session.set_control_state(state)?;
        self.session_identity_error = None;
        Ok(())
    }

    pub fn remember_session_error(&mut self, error: anyhow::Error) {
        self.session_identity_error = Some(format!("{error:#}"));
    }
    pub fn set_usage_scope(
        &mut self,
        scope: Option<llm::usage::Scope>,
    ) -> Option<llm::usage::Scope> {
        let protocol = match &self.kind {
            ProviderKind::ChatGpt { .. } => "chatgpt",
            ProviderKind::Local { .. } => "mlx",
            ProviderKind::Apple => llm::apple::PROVIDER,
            ProviderKind::Endpoint { .. } => "openai-compatible",
            ProviderKind::AnthropicEndpoint { .. } | ProviderKind::AnthropicSubscription { .. } => {
                "anthropic"
            }
        };
        let protocol = if matches!(
            self.kind,
            ProviderKind::Endpoint { .. } | ProviderKind::AnthropicEndpoint { .. }
        ) {
            self.protocol.map(ApiProtocol::as_str).unwrap_or(protocol)
        } else {
            protocol
        };
        let model = match &self.kind {
            ProviderKind::Local { model_dir } => Path::new(model_dir)
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or(SERVER_MODEL),
            _ => self.model_label(),
        };
        let label = format!("{protocol}/{model}");
        std::mem::replace(&mut self.usage_scope, scope.map(|s| s.for_model(label)))
    }
    /// Snapshot the observed counters owned by this backend's current usage scope.
    pub fn usage_scope_snapshot(&self) -> llm::usage::Groups {
        self.usage_scope
            .as_ref()
            .map(llm::usage::Scope::snapshot)
            .map(|(total, _)| total)
            .unwrap_or_default()
    }

    pub fn record_discarded_response(&self) -> Result<()> {
        if let Some(scope) = &self.usage_scope {
            scope.record_discarded_response()?;
        }
        Ok(())
    }

    pub fn with_protocol(mut self, protocol: Option<ApiProtocol>) -> Self {
        self.protocol = protocol;
        self
    }

    /// Authenticate an endpoint route with a stored provider OAuth grant instead of its API key.
    pub fn with_oauth(mut self, binding: Option<llm::oauth::Binding>) -> Self {
        if matches!(
            self.kind,
            ProviderKind::Endpoint { .. } | ProviderKind::AnthropicEndpoint { .. }
        ) {
            self.oauth = binding;
        }
        self
    }

    /// Read a key-authenticated endpoint's key at every dispatch instead of the copy made when the
    /// backend was built, so a saved, replaced or removed key applies to the next request. OAuth
    /// routes and other provider kinds keep their own credentials.
    pub fn with_credential_source(
        mut self,
        user_dir: &Path,
        base_url: &str,
        api_key_env: Option<&str>,
    ) -> Self {
        if matches!(
            self.kind,
            ProviderKind::Endpoint { .. } | ProviderKind::AnthropicEndpoint { .. }
        ) && self.oauth.is_none()
        {
            self.credential = Some(CredentialSource {
                user_dir: user_dir.to_path_buf(),
                base_url: base_url.to_owned(),
                api_key_env: api_key_env.map(str::to_owned),
            });
        }
        self
    }

    /// Let this endpoint use several API keys: `envs` name the variables, in preference order. A
    /// key that is throttled or out of quota is skipped until its cooldown ends. Needs a
    /// credential source (the pool replaces which variable it reads); without one it does nothing.
    pub fn with_key_pool(mut self, envs: impl IntoIterator<Item = String>) -> Self {
        if self.credential.is_some() {
            self.key_pool = llm::key_pool::KeyPool::new(envs);
        }
        self
    }

    /// Leave an active pool key that another request in this process already blocked.
    fn settle_key_pool(&mut self) {
        if let (Some(pool), Some(source)) = (self.key_pool.as_mut(), self.credential.as_ref()) {
            pool.settle(
                llm::key_pool::process_blocks(),
                &source.base_url,
                Instant::now(),
                |variable| source.has_key(variable),
            );
        }
    }

    /// The API key an endpoint request sends at this dispatch: re-read through the credential source
    /// when the backend has one, otherwise the key it was built with.
    pub(super) fn endpoint_api_key(&self) -> Result<Option<String>> {
        if let Some(source) = &self.credential {
            return match &self.key_pool {
                Some(pool) => source.resolve_with(Some(pool.active())),
                None => source.resolve(),
            };
        }
        Ok(match &self.kind {
            ProviderKind::Endpoint { api_key, .. }
            | ProviderKind::AnthropicEndpoint { api_key, .. } => api_key.clone(),
            _ => None,
        })
    }

    pub fn with_headers(mut self, headers: llm::model_config::RequestHeaders) -> Result<Self> {
        llm::model_config::validate_headers(&headers)?;
        ensure!(
            headers.is_empty()
                || matches!(
                    self.kind,
                    ProviderKind::Endpoint { .. } | ProviderKind::AnthropicEndpoint { .. }
                ),
            "subscription and Dial-owned local providers do not permit custom request headers"
        );
        self.headers = headers;
        Ok(self)
    }

    /// Permit plain `http` to non-local endpoint hosts.
    pub fn with_allow_insecure(mut self, allow: bool) -> Self {
        self.allow_insecure = allow;
        self
    }

    /// Mask secrets in every request sent to a hosted model and restore them in its reply
    /// (`[secrets]`). Dial-launched local models and the on-device model never leave the machine
    /// and receive the real text. Fails if the install key cannot be read or created.
    pub fn with_secrets(
        mut self,
        config: &crate::config::secrets::Config,
        user_dir: &Path,
    ) -> Result<Self> {
        self.secrets = if matches!(self.kind, ProviderKind::Local { .. } | ProviderKind::Apple) {
            None
        } else {
            llm::secrets::Masker::from_config(config, user_dir)?.map(Arc::new)
        };
        Ok(self)
    }

    pub fn with_remote_idle_timeout(mut self, seconds: u64) -> Self {
        self.remote_idle_timeout_secs = seconds;
        self
    }

    /// Test-only: bound this backend's streamed silence after progress without touching other threads.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_test_idle_limit(mut self, limit: Duration) -> Self {
        self.idle_limit = Some(limit);
        self
    }

    fn response_timeout(&self, worker_timeout_secs: u64) -> u64 {
        let remote = match &self.kind {
            ProviderKind::ChatGpt { .. } | ProviderKind::AnthropicSubscription { .. } => true,
            ProviderKind::Local { .. } | ProviderKind::Apple => false,
            ProviderKind::Endpoint { base_url, .. }
            | ProviderKind::AnthropicEndpoint { base_url, .. } => {
                !llm::host_of(base_url).is_some_and(|host| llm::is_local_host(&host))
            }
        };
        if remote {
            match (worker_timeout_secs, self.remote_idle_timeout_secs) {
                (0, remote) => remote,
                (worker, 0) => worker,
                (worker, remote) => worker.min(remote),
            }
        } else {
            worker_timeout_secs
        }
    }

    pub fn with_steering(mut self, config: &crate::config::types::Config) -> Self {
        if !matches!(self.kind, ProviderKind::Local { .. }) {
            self.steering_runtime = serde_json::json!({"state":"unsupported","reason":"this provider has no Dial-owned activation hook"});
        }
        self.steering_config = Some(config.clone());
        self
    }
    pub fn without_steering(mut self) -> Self {
        self.release();
        self.next_profile = None;
        self.steering_config = None;
        self
    }
    pub fn steering_runtime(&self) -> serde_json::Value {
        self.steering_runtime.clone()
    }

    /// Keep the status available, but announce an unchanged bypass only once.
    fn steering_bypass(runtime: &mut serde_json::Value, reason: String) -> Option<String> {
        let state = serde_json::json!({"state":"bypassed","reason":reason,"uses":0});
        let changed = *runtime != state;
        *runtime = state;
        changed.then(|| format!("Steering bypassed: {reason}"))
    }

    pub fn with_metadata(mut self, metadata: llm::catalog::ModelMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Resolve the operator's unlimited output setting to a finite protocol budget.
    /// A profile's explicit output limit remains authoritative.
    pub fn output_budget(&self, requested: u32) -> u32 {
        self.resolved_model(self.profile.as_ref())
            .output_budget(requested)
    }

    fn resolved_model(
        &self,
        profile: Option<&llm::profiles::ModelProfile>,
    ) -> llm::policy::ResolvedModel {
        let provider = profile
            .map(|profile| profile.provider.as_str())
            .unwrap_or_else(|| match &self.kind {
                ProviderKind::Local { .. } => "mlx",
                ProviderKind::Apple => llm::apple::PROVIDER,
                ProviderKind::ChatGpt { .. } => "chatgpt",
                ProviderKind::AnthropicSubscription { .. }
                | ProviderKind::AnthropicEndpoint { .. } => "anthropic",
                ProviderKind::Endpoint { .. } => "endpoint",
            });
        llm::policy::ResolvedModel::resolve(
            provider,
            self.model_label(),
            profile,
            &self.metadata,
            self.context_capacity(),
        )
    }

    fn context_capacity(&self) -> u32 {
        self.profile
            .as_ref()
            .and_then(|p| p.context_window)
            .or_else(|| self.steering_config.as_ref().map(|c| c.context_window))
            .or_else(|| context_window_hint(&self.kind))
            .filter(|n| *n > 0)
            .unwrap_or(32768)
    }
    pub fn model_profile(&self) -> Option<&llm::profiles::ModelProfile> {
        self.profile.as_ref()
    }
    /// Capture pending controls before dispatch when the caller owns request-level recovery.
    pub fn request_controls(&self) -> RequestControls {
        RequestControls {
            profile: self.next_profile.clone(),
            local_hints: self.next_local_hints.clone(),
            reasoning_guard: self.next_reasoning_guard.clone(),
            failover: self.next_failover,
        }
    }
    /// Re-arm only the controls consumed by a rejected attempt of the same logical request.
    pub fn restore_request_controls(&mut self, controls: &RequestControls) {
        self.next_profile = controls.profile.clone();
        self.next_local_hints = controls.local_hints.clone();
        self.next_reasoning_guard = controls.reasoning_guard.clone();
        self.next_failover = controls.failover;
    }

    /// Consumed once at the request boundary; transport retries keep the same profile.
    pub fn next_request_profile(&mut self, profile: Option<llm::profiles::ModelProfile>) {
        self.next_profile = profile;
    }
    /// The caller has a fallback model ready for failures of these classes: the next request
    /// retries them briefly and returns the error so the caller can fail over, instead of
    /// spending every `retry.max_attempts`. Consumed once at the request boundary.
    pub fn next_request_failover(&mut self, on: Option<crate::config::types::FallbackOn>) {
        self.next_failover = on;
    }
    /// Persistent local hints for every request of this backend.
    pub fn set_local_hints(&mut self, hints: llm::local_hints::LocalHints) {
        self.local_hints = hints;
    }
    /// Hints for the next request only: consumed at the request boundary (transport retries keep
    /// them) and merged over the persistent hints, the next request's value winning per field.
    pub fn next_local_hints(&mut self, hints: llm::local_hints::LocalHints) {
        self.next_local_hints = Some(hints);
    }
    /// Penalty hints for loop recovery on local routes; `None` disables retries.
    pub fn set_loop_penalty(&mut self, hints: Option<llm::local_hints::LocalHints>) {
        self.loop_penalty = hints;
    }
    /// Re-samples the next request with the loop penalty overlaid on the hints the previous request
    /// used (penalty fields win; the request-side never-overwrite rule still keeps profile values).
    /// Returns false, without retrying, when no penalty is configured or the route is hosted.
    pub fn retry_with_loop_penalty(&mut self) -> bool {
        let Some(penalty) = self.loop_penalty.clone() else {
            return false;
        };
        if self.local_server().is_none() {
            return false;
        }
        self.next_local_hints = Some(penalty.over(&self.last_local_hints));
        self.loop_penalty_retries += 1;
        true
    }
    /// Drains the number of loop-penalty retries requested since the last call.
    pub fn take_loop_penalty_retries(&mut self) -> u32 {
        std::mem::take(&mut self.loop_penalty_retries)
    }
    /// The local server behind this backend's route, `None` for hosted routes.
    pub fn local_server(&self) -> Option<llm::local_hints::LocalServer> {
        let key = self.endpoint_api_key().ok().flatten();
        self.detect_local(
            self.next_profile
                .as_ref()
                .or(self.profile.as_ref())
                .map(|p| p.provider.as_str()),
            None,
            key.as_deref(),
        )
    }
    /// Provider id a local route is detected under: `mlx` for Dial's runtime, `ollama` for the
    /// Ollama protocol, else the profile's provider (`endpoint` without one).
    fn local_provider<'a>(&self, profile_provider: Option<&'a str>) -> &'a str {
        match &self.kind {
            ProviderKind::Local { .. } => "mlx",
            _ if self.protocol == Some(ApiProtocol::Ollama) => "ollama",
            _ => profile_provider.unwrap_or("endpoint"),
        }
    }
    fn route_model(&self) -> &str {
        match &self.kind {
            ProviderKind::Apple => llm::apple::MODEL,
            ProviderKind::Local { model_dir } => model_dir.as_str(),
            ProviderKind::Endpoint { model, .. }
            | ProviderKind::AnthropicEndpoint { model, .. } => model.as_str(),
            ProviderKind::ChatGpt { model, .. }
            | ProviderKind::AnthropicSubscription { model, .. } => model.as_str(),
        }
    }
    /// The local server for a route. Only Dial's runtime and OpenAI-compatible chat endpoints on a
    /// local host qualify; every other route (hosted, Anthropic, Responses) returns `None`.
    fn detect_local(
        &self,
        profile_provider: Option<&str>,
        base_url: Option<&str>,
        token: Option<&str>,
    ) -> Option<llm::local_hints::LocalServer> {
        let provider = self.local_provider(profile_provider);
        match &self.kind {
            ProviderKind::Local { model_dir } => Some(llm::local_hints::LocalServer::detect(
                provider,
                model_dir,
                None,
                None,
                &[],
            )),
            ProviderKind::Endpoint {
                base_url: configured,
                ..
            } if matches!(
                self.protocol.unwrap_or(ApiProtocol::ChatCompletions),
                ApiProtocol::ChatCompletions | ApiProtocol::Ollama
            ) =>
            {
                let base = base_url.unwrap_or(configured.as_str());
                let local = llm::host_of(base).is_some_and(|host| llm::is_local_host(&host));
                local.then(|| {
                    llm::local_hints::LocalServer::detect(
                        provider,
                        "",
                        Some(base),
                        token,
                        &self.headers,
                    )
                })
            }
            _ => None,
        }
    }
    /// Scoped side requests restore a pending main profile even if no request boundary was reached.
    pub fn swap_request_profile(
        &mut self,
        profile: Option<llm::profiles::ModelProfile>,
    ) -> Option<llm::profiles::ModelProfile> {
        std::mem::replace(&mut self.next_profile, profile)
    }
    /// Only Dial-owned MLX receives decoder controls; external providers retain their protocol.
    pub fn next_reasoning_guard(&mut self, config: &crate::config::types::ReasoningGuardConfig) {
        self.next_reasoning_guard =
            matches!(self.kind, ProviderKind::Local { .. }).then(|| config.clone());
    }
    /// Forgets the bound Codex WebSocket chain so the next request is sent unchained.
    pub fn reset_provider_stream(&mut self) {
        llm::codex_ws::forget(self.session_id.as_deref().unwrap_or("codex-default"));
    }
    /// After the caller's silence watchdog has settled an attempt, avoid retrying a stuck Codex
    /// WebSocket route. This backend uses HTTP streaming until rebuilt; configuration is unchanged.
    /// Never call this for ordinary user cancellation.
    pub fn recover_stalled_stream(&mut self) -> bool {
        if !self.codex_websocket || !matches!(self.kind, ProviderKind::ChatGpt { .. }) {
            return false;
        }
        self.reset_provider_stream();
        self.codex_websocket = false;
        true
    }

    pub fn with_profile(mut self, profile: llm::profiles::ModelProfile) -> Self {
        self.profile = Some(profile);
        self
    }
    /// The structured-output mechanism the live endpoint enforces, detected rather than declared: a
    /// loopback/private OpenAI-compatible endpoint that answers llama.cpp's `/props` enforces
    /// `json_schema`. Hosted endpoints are never probed; `catalog::is_llama_server` caches per base URL.
    pub fn endpoint_structured_output(&self) -> Option<llm::profiles::StructuredOutput> {
        let (base_url, api_key) = self.local_chat_completions_endpoint()?;
        llm::catalog::is_llama_server(base_url, api_key, &self.headers)
            .then_some(llm::profiles::StructuredOutput::JsonSchema)
    }

    /// Base URL and key of a Chat Completions endpoint on a loopback/private host, the only routes
    /// that can be an external llama.cpp server.
    fn local_chat_completions_endpoint(&self) -> Option<(&str, Option<&str>)> {
        let ProviderKind::Endpoint {
            base_url, api_key, ..
        } = &self.kind
        else {
            return None;
        };
        (self.protocol.unwrap_or(ApiProtocol::ChatCompletions) == ApiProtocol::ChatCompletions
            && llm::host_of(base_url).is_some_and(|host| llm::is_local_host(&host)))
        .then_some((base_url.as_str(), api_key.as_deref()))
    }

    /// Once per conversation, when a request offers tools to an external llama.cpp server whose
    /// `/props` (already fetched by detection; never probed here) says the model's chat template
    /// cannot render tools natively.
    fn tool_template_notice(&mut self, req: &Chat, on_notice: &mut dyn FnMut(&str)) {
        if req.tools.is_empty() || self.tool_template_noticed {
            return;
        }
        let lacking = self
            .local_chat_completions_endpoint()
            .and_then(|(base_url, _)| llm::catalog::known_llama_props(base_url))
            .is_some_and(|props| props.native_tool_template == Some(false));
        if lacking {
            self.tool_template_noticed = true;
            on_notice(LLAMA_TOOL_TEMPLATE_NOTICE);
        }
    }

    pub fn reply_cost(&self, reply: &Reply) -> Option<f64> {
        let remote = match &self.kind {
            ProviderKind::ChatGpt { .. } | ProviderKind::AnthropicSubscription { .. } => {
                return None;
            }
            ProviderKind::Local { .. } | ProviderKind::Apple => false,
            ProviderKind::Endpoint { base_url, .. }
            | ProviderKind::AnthropicEndpoint { base_url, .. } => {
                !llm::host_of(base_url).is_some_and(|host| llm::is_local_host(&host))
            }
        };
        if !remote
            || self
                .oauth
                .as_ref()
                .is_some_and(|binding| binding.flow().subscription)
        {
            return None;
        }
        if let Some(pricing) = self.metadata.pricing {
            return pricing.reply_cost(reply);
        }
        let model = match &self.kind {
            ProviderKind::Endpoint { model, .. }
            | ProviderKind::AnthropicEndpoint { model, .. }
            | ProviderKind::ChatGpt { model, .. }
            | ProviderKind::AnthropicSubscription { model, .. } => model,
            ProviderKind::Local { .. } | ProviderKind::Apple => return None,
        };
        let bundled = llm::catalog::model_capabilities(model)?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis()
            .try_into()
            .ok()?;
        bundled.list_price?.reply_cost(reply, now_ms)
    }

    /// Human-readable summary; never contains the API key.
    pub fn describe(&self) -> String {
        match &self.kind {
            ProviderKind::ChatGpt { model, .. } => format!(
                "{model} via ChatGPT subscription [account {}]",
                self.account_id.as_deref().unwrap_or("unavailable")
            ),
            ProviderKind::AnthropicSubscription { model, .. } => {
                format!("{model} via Claude subscription")
            }
            ProviderKind::Local { model_dir } => {
                let name = Path::new(model_dir)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(model_dir);
                format!("local mlx_lm.server ({name})")
            }
            ProviderKind::Endpoint {
                base_url, model, ..
            }
            | ProviderKind::AnthropicEndpoint {
                base_url, model, ..
            } => {
                format!("{model} via {}", llm::display_url(base_url))
            }
            ProviderKind::Apple => format!("{} via FoundationModels", llm::apple::LABEL),
        }
    }

    /// Only a local server can load a LoRA adapter.
    pub fn supports_adapters(&self) -> bool {
        matches!(self.kind, ProviderKind::Local { .. })
    }

    pub fn local_inference(&self) -> bool {
        match &self.kind {
            ProviderKind::Local { .. } | ProviderKind::Apple => true,
            ProviderKind::Endpoint { base_url, .. }
            | ProviderKind::AnthropicEndpoint { base_url, .. } => {
                llm::host_of(base_url).is_some_and(|host| llm::is_local_host(&host))
            }
            _ => false,
        }
    }

    pub fn adapter_monitor(&self) -> AdapterMonitor {
        Arc::clone(&self.adapter_monitor)
    }

    pub fn model_label(&self) -> &str {
        match &self.kind {
            ProviderKind::ChatGpt { model, .. } => model,
            ProviderKind::AnthropicSubscription { model, .. } => model,
            ProviderKind::Local { .. } => SERVER_MODEL,
            ProviderKind::Apple => llm::apple::MODEL,
            ProviderKind::Endpoint { model, .. }
            | ProviderKind::AnthropicEndpoint { model, .. } => model,
        }
    }

    /// Stop the local server (if any) to free its memory. A later `chat`
    /// starts a new one.
    pub fn release(&mut self) {
        *self.adapter_monitor.lock() = AdapterRuntime::default();
        if let Some(mut server) = self.server.take() {
            server.stop();
        }
    }

    /// Move the resident local server out without stopping it, so a rebuilt backend for the same
    /// session can adopt it instead of reloading the weights.
    pub fn take_resident_server(&mut self) -> Option<ModelServer> {
        self.server.take()
    }

    /// Adopt a server handed over by [`Self::take_resident_server`]. The next request reuses it
    /// only if it still matches the model, adapter, runtime and context this backend asks for
    /// (`ModelServer::ready_for`); otherwise it is stopped and replaced then. A backend that does
    /// not run a local server stops it now.
    pub fn adopt_resident_server(&mut self, server: ModelServer) {
        if matches!(self.kind, ProviderKind::Local { .. }) && self.server.is_none() {
            self.server = Some(server);
        }
    }

    /// One completion with default control hooks. Interactive callers use `chat_with_control`.
    pub fn chat(
        &mut self,
        adapter: Option<&Path>,
        req: &Chat,
        timeout_secs: u64,
        on_text: Option<&mut dyn FnMut(&str) -> bool>,
    ) -> Result<Reply> {
        self.chat_with_control(
            adapter,
            req,
            timeout_secs,
            on_text,
            &mut |_| {},
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// At most `retry_policy.max_attempts` attempts (`retry.max_attempts`, default 10); each backoff
    /// wait is capped at `BACKOFF_CAP` (60 seconds) and `retry.max_delay_secs`.
    /// Body/stream errors are never replayed, even when no visible text arrived.
    pub fn chat_with_control(
        &mut self,
        adapter: Option<&Path>,
        req: &Chat,
        timeout_secs: u64,
        on_text: Option<&mut dyn FnMut(&str) -> bool>,
        on_notice: &mut dyn FnMut(&str),
        interrupt: Arc<AtomicBool>,
    ) -> Result<Reply> {
        self.chat_with_control_events(
            adapter,
            req,
            llm::Limits {
                timeout_secs,
                interrupt,
            },
            on_text,
            on_notice,
            &mut |_| true,
        )
    }

    /// Local vision requires a vision encoder; unknown external endpoints may attempt images.
    pub fn accepts_image_input(&self) -> bool {
        self.image_input_for(self.profile.as_ref())
    }

    fn image_input_for(&self, profile: Option<&llm::profiles::ModelProfile>) -> bool {
        let supported = self.resolved_model(profile).vision.unwrap_or(true);
        supported
            && match &self.kind {
                ProviderKind::Local { model_dir } => {
                    llm::catalog::local_model_metadata(Path::new(model_dir)).vision == Some(true)
                }
                // The system model reads text only.
                ProviderKind::Apple => false,
                _ => true,
            }
    }

    pub fn chat_with_control_events(
        &mut self,
        adapter: Option<&Path>,
        req: &Chat,
        limits: llm::Limits,
        on_text: Option<&mut dyn FnMut(&str) -> bool>,
        on_notice: &mut dyn FnMut(&str),
        on_event: &mut dyn FnMut(&llm::StreamEvent) -> bool,
    ) -> Result<Reply> {
        let _wire = llm::wire::Scope::enter(self.wire.clone());
        #[cfg(any(test, feature = "test-support"))]
        let _idle_limit = llm::cancel::TestIdleLimitScope::enter(self.idle_limit);
        let billing = match &self.kind {
            ProviderKind::Local { .. } | ProviderKind::Apple => llm::usage::Billing::Local,
            ProviderKind::ChatGpt { .. } | ProviderKind::AnthropicSubscription { .. } => {
                llm::usage::Billing::Subscription
            }
            ProviderKind::Endpoint { base_url, .. }
            | ProviderKind::AnthropicEndpoint { base_url, .. }
                if llm::host_of(base_url).is_some_and(|host| llm::is_local_host(&host)) =>
            {
                llm::usage::Billing::Local
            }
            _ if self
                .oauth
                .as_ref()
                .is_some_and(|binding| binding.flow().subscription) =>
            {
                llm::usage::Billing::Subscription
            }
            _ => llm::usage::Billing::Api,
        };
        let scope = self.usage_scope.clone();
        if let Some(scope) = &scope {
            scope.begin(billing)?;
        }
        let transport = llm::usage::Transport::enter(scope.clone());
        let mut result = match self.secrets.clone() {
            Some(masker) => {
                let (masked, restorer) = masker.mask_chat(req);
                restorer.run(on_text, |on_text| {
                    self.chat_with_control_events_inner(
                        adapter, &masked, limits, on_text, on_notice, on_event,
                    )
                })
            }
            None => self
                .chat_with_control_events_inner(adapter, req, limits, on_text, on_notice, on_event),
        };
        if let Ok(reply) = &mut result {
            reply.wire = self.wire.lock().latest();
        }
        transport.settle(result.is_ok())?;
        if let Some(scope) = scope {
            let cost = result
                .as_ref()
                .ok()
                .and_then(|reply| self.reply_cost(reply));
            scope.finish(result.as_ref().ok(), cost, billing, transport.attempts())?;
        }
        result
    }

    fn chat_with_control_events_inner(
        &mut self,
        adapter: Option<&Path>,
        req: &Chat,
        limits: llm::Limits,
        mut on_text: Option<&mut dyn FnMut(&str) -> bool>,
        on_notice: &mut dyn FnMut(&str),
        on_event: &mut dyn FnMut(&llm::StreamEvent) -> bool,
    ) -> Result<Reply> {
        let llm::Limits {
            timeout_secs,
            interrupt,
        } = limits;
        ensure!(
            self.metadata.kind.supports_chat(),
            "this is a {} model, not a chat model; select a chat model in /models or assign an embedding model to the embedding role",
            self.metadata.kind.label()
        );
        let mut profile = self.next_profile.take().or_else(|| self.profile.clone());
        // The detected mechanism only matters to a constrained retry that offers tools; ordinary
        // requests are never probed.
        if let Some(profile) = profile.as_mut()
            && profile.tool_repair
            && !req.tools.is_empty()
            && profile.endpoint_structured_output.is_none()
        {
            profile.endpoint_structured_output = self.endpoint_structured_output();
        }
        ensure!(
            self.resolved_model(profile.as_ref()).tools != Some(false) || req.tools.is_empty(),
            "the resolved model declares tool calls unsupported; choose a tool-capable model"
        );
        let reasoning_guard = self.next_reasoning_guard.take();
        let failover = self.next_failover.take();
        let interrupted = || interrupt.load(Ordering::SeqCst);
        let timeout_secs = self.response_timeout(timeout_secs);
        let vision = self.image_input_for(profile.as_ref());
        let prepared = llm::images::prepare(req, vision, &self.image_notices, on_notice);
        let req = prepared.as_ref().unwrap_or(req);
        let mut resolved;
        let req = if req.max_tokens == 0 {
            resolved = req.clone();
            resolved.max_tokens = profile
                .as_ref()
                .and_then(|p| p.max_tokens)
                .filter(|n| *n > 0)
                .unwrap_or_else(|| self.output_budget(0));
            &resolved
        } else {
            req
        };
        if let Some(error) = &self.session_identity_error {
            bail!("{error}");
        }
        let chatgpt_access = match &self.kind {
            ProviderKind::ChatGpt { user_dir, .. } => Some(llm::chatgpt::scoped_account_access(
                user_dir,
                self.account_id
                    .as_deref()
                    .context("ChatGPT backend has no pinned account")?,
                Arc::clone(&interrupt),
            )?),
            _ => None,
        };
        let anthropic_token = match &self.kind {
            ProviderKind::AnthropicSubscription { user_dir, .. } => Some(
                llm::anthropic_oauth::access_token(user_dir, Arc::clone(&interrupt))?,
            ),
            _ => None,
        };
        // A grant that routes by its own identifiers (a Cloudflare gateway or Snowflake account) fixes
        // the endpoint before the configured base URL is validated, so unset template placeholders do
        // not fail a route the grant already completes.
        let grant_route = match (&self.kind, &self.oauth) {
            (
                ProviderKind::Endpoint { .. } | ProviderKind::AnthropicEndpoint { .. },
                Some(binding),
            ) => binding.access(&interrupt)?.base_url,
            _ => None,
        };
        // Key-authenticated endpoints read their key at dispatch, so a key saved, replaced or removed
        // since the backend was built applies to this request. Precedence and origin checks stay in
        // `resolve_credential`.
        self.settle_key_pool();
        let endpoint_key = self.endpoint_api_key()?;
        if matches!(self.kind, ProviderKind::Apple) {
            ensure!(
                adapter.is_none(),
                "the Apple on-device model cannot load LoRA adapters"
            );
            return llm::apple::chat(req, timeout_secs, &interrupt, on_text, on_event);
        }
        // Local hints ride the profile as request-scoped carriers; a hosted route never gets any.
        // Detection (which may probe a local server) runs only when there is something to send.
        let merged_hints = self
            .next_local_hints
            .take()
            .unwrap_or_default()
            .over(&self.local_hints);
        let local_server = if merged_hints.is_empty() {
            None
        } else {
            self.detect_local(
                profile.as_ref().map(|p| p.provider.as_str()),
                grant_route.as_deref(),
                endpoint_key.as_deref(),
            )
        };
        self.last_local_hints = merged_hints.clone();
        if local_server.is_some() && profile.is_none() {
            profile = Some(llm::profiles::ModelProfile::minimal(
                self.local_provider(None),
                self.route_model(),
            ));
        }
        if let Some(profile) = profile.as_mut() {
            profile.local_hints = if local_server.is_some() {
                merged_hints
            } else {
                Default::default()
            };
            profile.local_server = local_server;
        }
        self.tool_template_notice(req, on_notice);
        let (url, auth, model, wire, codex_account_id) = match &self.kind {
            ProviderKind::Apple => unreachable!("handled above"),
            ProviderKind::ChatGpt { model, .. } => {
                ensure!(
                    self.protocol.is_none_or(|p| p == ApiProtocol::Responses),
                    "ChatGPT subscription requires Responses protocol"
                );
                ensure!(
                    adapter.is_none(),
                    "ChatGPT providers cannot load LoRA adapters"
                );
                let url = format!("{}/codex/responses", llm::chatgpt::BASE_URL);
                #[cfg(any(test, feature = "test-support"))]
                let url = self.chatgpt_url.clone().unwrap_or(url);
                let (account_id, token) = chatgpt_access.as_ref().unwrap();
                (
                    url,
                    Some(token.as_str()),
                    model,
                    Wire::Codex,
                    Some(account_id.as_str()),
                )
            }
            ProviderKind::AnthropicSubscription { model, .. } => {
                ensure!(
                    self.protocol.is_none_or(|p| p == ApiProtocol::Anthropic),
                    "Claude subscription requires Anthropic protocol"
                );
                ensure!(
                    adapter.is_none(),
                    "Claude subscription providers cannot load LoRA adapters"
                );
                (
                    format!("{}/messages", llm::anthropic::BASE_URL),
                    anthropic_token.as_ref().map(|t| t.as_str()),
                    model,
                    Wire::Anthropic,
                    None,
                )
            }
            ProviderKind::Local { model_dir } => {
                let mut req = req.clone();
                req.messages = llm::messages::local(&req.messages)?;
                if interrupted() {
                    return Err(llm::Interrupted.into());
                }
                *self.adapter_monitor.lock() = AdapterRuntime {
                    adapter: adapter.map(Path::to_path_buf),
                    phase: AdapterPhase::Loading,
                };
                let steering = self.steering_config.as_ref().and_then(|config| {
                    if let Some(reason) = crate::llm::steering::local_unsupported_reason(model_dir)
                    {
                        if (config.steering.enabled || config.steering.automatic)
                            && config.steering.strength > 0.0
                        {
                            let state =
                                serde_json::json!({"state":"unsupported","reason":reason,"uses":0});
                            let changed = self.steering_runtime != state;
                            self.steering_runtime = state;
                            if config.steering.enabled && changed {
                                on_notice(&format!("Activation steering unavailable: {reason}"));
                            }
                        } else {
                            self.steering_runtime =
                                serde_json::json!({"state":"disabled","uses":0});
                        }
                        return None;
                    }
                    match crate::llm::steering::specification(config, model_dir) {
                        Ok(spec) => spec,
                        Err(error) => {
                            if let Some(notice) = Self::steering_bypass(
                                &mut self.steering_runtime,
                                format!("{error:#}"),
                            ) {
                                on_notice(&notice);
                            }
                            None
                        }
                    }
                });
                let runtime = profile
                    .as_ref()
                    .and_then(|p| p.local_runtime.clone())
                    .or_else(|| {
                        self.steering_config
                            .as_ref()
                            .map(|config| config.local_runtime.clone())
                    })
                    .unwrap_or_default();
                if steering.is_some() && runtime.draft_model.is_some() {
                    let reason = "steering does not support speculative inference".to_string();
                    if let Some(notice) = Self::steering_bypass(&mut self.steering_runtime, reason)
                    {
                        on_notice(&notice);
                    }
                }
                let phase = if self.server.as_ref().is_some_and(|server| {
                    server.ready_for(
                        model_dir,
                        adapter,
                        steering.as_ref(),
                        &runtime,
                        self.steering_config.as_ref(),
                    )
                }) {
                    llm::InferencePhase::Processing
                } else {
                    llm::InferencePhase::Loading
                };
                if !on_event(&llm::StreamEvent::Progress(llm::ModelProgress::new(phase))) {
                    return Err(llm::Interrupted.into());
                }
                let started = {
                    // Startup's 1-token warmup is not an attempt of the user's logical request.
                    let _startup_accounting = llm::usage::Transport::suspend();
                    server::ensure_profile_tracked(
                        &mut self.server,
                        server::ServerProfile {
                            python: &self.python,
                            model: model_dir,
                            adapter,
                            steering: steering.as_ref(),
                            runtime: &runtime,
                            context: self.steering_config.as_ref(),
                        },
                        self.startup_timeout_secs,
                        Some(Arc::clone(&interrupt)),
                    )
                };
                let server = match started {
                    Ok(server) => server,
                    Err(error) => {
                        self.adapter_monitor.lock().phase = AdapterPhase::Failed;
                        return Err(error);
                    }
                };
                self.adapter_monitor.lock().phase = AdapterPhase::Loaded;
                if !on_event(&llm::StreamEvent::Progress(llm::ModelProgress::new(
                    llm::InferencePhase::Processing,
                ))) {
                    self.adapter_monitor.lock().phase = AdapterPhase::Failed;
                    return Err(server.abort_request(llm::Interrupted.into()));
                }
                if let Err(error) = server.begin_request() {
                    self.adapter_monitor.lock().phase = AdapterPhase::Failed;
                    return Err(server.abort_request(error));
                }
                let result = exchange(
                    llm::Endpoint {
                        headers: &[],
                        url: &llm::completions_url(server.base_url()),
                        auth: None,
                        model: server.request_model(),
                    },
                    &req,
                    timeout_secs,
                    &mut on_text,
                    llm::Control {
                        interrupt: Some(Arc::clone(&interrupt)),
                        profile: profile.as_ref(),
                        reasoning_guard: reasoning_guard.as_ref(),
                        on_event,
                    },
                );
                let result = if interrupted() {
                    Err(llm::Interrupted.into())
                } else {
                    result
                };
                let result = result.and_then(|reply| server.phase("loaded").map(|()| reply));
                let result = result.map_err(|error| server.abort_request(error));
                if result.is_ok() && steering.is_some() {
                    let previous = self.steering_runtime["state"].clone();
                    self.steering_runtime = crate::llm::steering::runtime(server.base_url())
                        .unwrap_or_else(
                            |e| serde_json::json!({"state":"unconfirmed","reason":e.to_string()}),
                        );
                    if self.steering_runtime["state"] != previous {
                        if let Some(reason) = self.steering_runtime["reason"].as_str() {
                            on_notice(&format!(
                                "Activation steering {}: {reason}",
                                self.steering_runtime["state"]
                                    .as_str()
                                    .unwrap_or("unconfirmed")
                            ));
                        } else if self.steering_runtime["state"] == "used" {
                            on_notice(
                                "Activation steering used · evaluated profile active in this local response",
                            );
                        }
                    }
                }
                self.adapter_monitor.lock().phase = if result.is_ok() {
                    AdapterPhase::Used
                } else {
                    AdapterPhase::Failed
                };
                return result.map(|mut reply| {
                    if profile
                        .as_ref()
                        .is_some_and(|p| p.capabilities.strict_tools)
                    {
                        crate::llm::reliability::normalize_reply(&mut reply, &req.tools);
                    }
                    reply
                });
            }
            ProviderKind::Endpoint {
                base_url, model, ..
            } => {
                ensure!(
                    adapter.is_none(),
                    "endpoint providers cannot load LoRA adapters"
                );
                let base_url = grant_route.as_deref().unwrap_or(base_url.as_str());
                let protocol = self.protocol.unwrap_or(ApiProtocol::ChatCompletions);
                (
                    llm::transports::route_url(protocol, base_url, self.allow_insecure)?,
                    endpoint_key.as_deref(),
                    model,
                    Wire::of(protocol),
                    None,
                )
            }
            ProviderKind::AnthropicEndpoint {
                base_url, model, ..
            } => {
                ensure!(
                    adapter.is_none(),
                    "endpoint providers cannot load LoRA adapters"
                );
                let base_url = grant_route.as_deref().unwrap_or(base_url.as_str());
                let protocol = self.protocol.unwrap_or(ApiProtocol::Anthropic);
                (
                    llm::transports::route_url(protocol, base_url, self.allow_insecure)?,
                    endpoint_key.as_deref(),
                    model,
                    Wire::of(protocol),
                    None,
                )
            }
        };
        let retry_policy = llm::retry::Policy::new(
            &self
                .steering_config
                .as_ref()
                .map(|config| config.retry.clone())
                .unwrap_or_default(),
            self.retry_backoff,
        )
        .with_failover(failover);
        // An endpoint that already refused the reasoning parameter is not asked again.
        let reasoning_scope = (url.clone(), model.to_owned());
        let mut reasoning_stripped = false;
        if llm::reasoning_memory::process_memory()
            .remembered(&reasoning_scope.0, &reasoning_scope.1)
            && let Some(current) = profile.take()
        {
            profile = Some(llm::reasoning_memory::without_reasoning(current));
        }
        let mut active_account = self.account_id.clone();
        let mut tried_accounts = active_account
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        // 0: no OAuth refresh requested; 1: refresh before the next attempt; 2: already refreshed.
        let mut oauth_retry = 0u8;
        let request_id = if self.protocol == Some(ApiProtocol::FactoryDroid) {
            Some(llm::factory_droid::new_request_id()?)
        } else {
            None
        };
        for attempt in 1..=retry_policy.max_attempts {
            if interrupted() {
                return Err(llm::Interrupted.into());
            }
            // Reported with a failure: seconds tell a stalled server or dropped path from a refusal.
            let attempt_started = Instant::now();
            if attempt > 1 && !on_event(&llm::StreamEvent::Retry { waiting: false }) {
                return Err(llm::Interrupted.into());
            }
            // A retry is a fresh dispatch: sign-out/restriction/rotation state remains authoritative.
            let refreshed_access = if attempt > 1 {
                match &self.kind {
                    ProviderKind::ChatGpt { user_dir, .. } => {
                        Some(llm::chatgpt::scoped_account_access(
                            user_dir,
                            active_account
                                .as_deref()
                                .context("ChatGPT backend has no pinned account")?,
                            Arc::clone(&interrupt),
                        )?)
                    }
                    _ => None,
                }
            } else {
                None
            };
            // Keep first-dispatch preparation unchanged, but never reuse a key across backoff:
            // removal must resolve to no auth, not fall back to the previous attempt's key.
            let retry_endpoint_key = if attempt > 1 {
                self.endpoint_api_key()?
            } else {
                None
            };
            let auth = if attempt > 1
                && matches!(
                    self.kind,
                    ProviderKind::Endpoint { .. } | ProviderKind::AnthropicEndpoint { .. }
                ) {
                retry_endpoint_key.as_deref()
            } else {
                auth
            };
            let auth = refreshed_access
                .as_ref()
                .map(|(_, token)| token.as_str())
                .or(auth);
            let oauth_access = match &self.oauth {
                Some(binding) if oauth_retry == 1 => {
                    oauth_retry = 2;
                    Some(binding.refreshed_access(&interrupt)?)
                }
                Some(binding) => Some(binding.access(&interrupt)?),
                None => None,
            };
            let auth = match oauth_access.as_ref() {
                Some(access) => Some(access.token.as_str()),
                None => auth,
            };
            let oauth_url = match oauth_access
                .as_ref()
                .and_then(|access| access.base_url.as_deref())
            {
                Some(base) => Some(llm::transports::route_url(
                    self.protocol.unwrap_or(ApiProtocol::ChatCompletions),
                    base,
                    self.allow_insecure,
                )?),
                None => None,
            };
            let url: &str = oauth_url.as_deref().unwrap_or(url.as_str());
            if let Some(binding) = &self.oauth {
                binding.ensure_route(
                    oauth_url.is_some(),
                    url,
                    self.protocol.unwrap_or(ApiProtocol::ChatCompletions),
                )?;
            }
            let resolved_headers = llm::model_config::resolve_validated_headers(&self.headers)?;
            // The CoreWeave project header is attached only to CoreWeave's own endpoint.
            let coreweave_project = match &self.kind {
                ProviderKind::Endpoint { base_url, .. }
                    if llm::coreweave::is_endpoint(base_url) =>
                {
                    llm::coreweave::project_from_process_env()
                }
                _ => None,
            };
            let mut headers = resolved_headers.pairs().collect::<Vec<_>>();
            headers.extend(
                coreweave_project
                    .iter()
                    .map(|project| (llm::coreweave::PROJECT_HEADER, project.as_str())),
            );
            headers.extend(oauth_access.iter().flat_map(|access| {
                access
                    .headers
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str()))
            }));
            let result = match wire {
                Wire::Codex => {
                    let endpoint = llm::responses::Endpoint {
                        url,
                        token: auth.unwrap(),
                        account_id: Some(
                            active_account
                                .as_deref()
                                .or(codex_account_id)
                                .context("Codex backend has no pinned account")?,
                        ),
                        model,
                        headers: &[],
                    };
                    let websocket = if self.codex_websocket {
                        llm::codex_ws::exchange(
                            endpoint,
                            req,
                            timeout_secs,
                            &mut on_text,
                            Arc::clone(&interrupt),
                            profile.as_ref(),
                            self.prompt_cache_key.as_deref(),
                            on_event,
                            self.session_id.as_deref().unwrap_or("codex-default"),
                        )
                    } else {
                        Ok(None)
                    };
                    match websocket {
                        Ok(Some(reply)) => Ok(reply),
                        Ok(None) => llm::responses::exchange_codex(
                            endpoint,
                            req,
                            timeout_secs,
                            &mut on_text,
                            Arc::clone(&interrupt),
                            profile.as_ref(),
                            self.prompt_cache_key.as_deref(),
                            on_event,
                            self.session_id.as_deref(),
                        ),
                        Err(error) => Err(error),
                    }
                }
                Wire::Anthropic => anthropic_exchange(
                    llm::Endpoint {
                        headers: &headers,
                        url,
                        auth,
                        model,
                    },
                    req,
                    timeout_secs,
                    &mut on_text,
                    Arc::clone(&interrupt),
                    profile.as_ref(),
                    on_event,
                ),
                Wire::Responses => llm::responses::exchange_api(
                    llm::Endpoint {
                        headers: &headers,
                        url,
                        auth,
                        model,
                    },
                    req,
                    timeout_secs,
                    &mut on_text,
                    Arc::clone(&interrupt),
                    profile.as_ref(),
                    self.prompt_cache_key.as_deref(),
                    on_event,
                ),
                Wire::Gemini | Wire::Ollama => {
                    let endpoint = llm::Endpoint {
                        headers: &headers,
                        url,
                        auth,
                        model,
                    };
                    if wire == Wire::Gemini {
                        llm::gemini::exchange(
                            endpoint,
                            req,
                            timeout_secs,
                            &mut on_text,
                            Arc::clone(&interrupt),
                            profile.as_ref(),
                            on_event,
                        )
                    } else {
                        llm::ollama::exchange(
                            endpoint,
                            req,
                            timeout_secs,
                            &mut on_text,
                            Arc::clone(&interrupt),
                            profile.as_ref(),
                            Some(self.context_capacity()),
                            on_event,
                        )
                    }
                }
                Wire::Transport => llm::transports::exchange(
                    &llm::transports::Route {
                        protocol: self.protocol.unwrap_or(ApiProtocol::ChatCompletions),
                        base_url: url,
                        model,
                        auth,
                        session_id: self.session_id.as_deref(),
                        request_id: request_id.as_deref(),
                        headers: &headers,
                        account_id: oauth_access
                            .as_ref()
                            .and_then(|access| access.account_id.as_deref()),
                        project_id: oauth_access
                            .as_ref()
                            .and_then(|access| access.project_id.as_deref()),
                        allow_insecure: self.allow_insecure,
                    },
                    req,
                    timeout_secs,
                    &mut on_text,
                    Arc::clone(&interrupt),
                    profile.as_ref(),
                    on_event,
                ),
                Wire::ChatCompletions => exchange(
                    llm::Endpoint {
                        headers: &headers,
                        url,
                        auth,
                        model,
                    },
                    req,
                    timeout_secs,
                    &mut on_text,
                    llm::Control {
                        interrupt: Some(Arc::clone(&interrupt)),
                        profile: profile.as_ref(),
                        reasoning_guard: None,
                        on_event,
                    },
                ),
            };
            let result = result.map_err(|error| resolved_headers.redact(error));
            if interrupted() {
                return Err(llm::Interrupted.into());
            }
            let error = match result {
                Ok(mut reply) => {
                    if reasoning_stripped {
                        llm::reasoning_memory::process_memory()
                            .remember(&reasoning_scope.0, &reasoning_scope.1);
                    }
                    if profile
                        .as_ref()
                        .is_some_and(|p| p.capabilities.strict_tools)
                    {
                        crate::llm::reliability::normalize_reply(&mut reply, &req.tools);
                    }
                    return Ok(reply);
                }
                Err(error) => error,
            };
            if !reasoning_stripped
                && attempt < retry_policy.max_attempts
                && llm::reasoning_memory::rejects_reasoning(&error)
                && profile
                    .as_ref()
                    .is_some_and(|p| p.reasoning_effort.is_some() || p.thinking == Some(false))
                && let Some(current) = profile.take()
            {
                profile = Some(llm::reasoning_memory::without_reasoning(current));
                reasoning_stripped = true;
                on_notice(
                    "Provider rejected the reasoning parameter; retrying without it and remembering this for the session",
                );
                continue;
            }
            if self.oauth.is_some()
                && oauth_retry == 0
                && attempt < retry_policy.max_attempts
                && error
                    .downcast_ref::<llm::HttpError>()
                    .is_some_and(|http| matches!(http.status, 401 | 403))
            {
                oauth_retry = 1;
                on_notice(
                    "Provider rejected the OAuth access token; refreshing the sign-in and retrying",
                );
                continue;
            }
            let class = llm::errors::classify(&error);
            if attempt < retry_policy.max_attempts
                && let Some(cooldown) = llm::key_pool::cooldown(
                    &class,
                    error
                        .downcast_ref::<llm::HttpError>()
                        .and_then(|http| http.retry_after),
                    std::time::SystemTime::now(),
                )
                && let (Some(pool), Some(source)) =
                    (self.key_pool.as_mut(), self.credential.as_ref())
                && pool
                    .rotate(
                        llm::key_pool::process_blocks(),
                        &source.base_url,
                        Instant::now(),
                        cooldown,
                        |variable| source.has_key(variable),
                    )
                    .is_some()
            {
                on_notice(
                    "API key was rate limited or out of quota; retrying with the next configured key",
                );
                continue;
            }
            if attempt < retry_policy.max_attempts
                && retry_policy.rotate_accounts
                && matches!(
                    class,
                    llm::errors::ErrorClass::Auth | llm::errors::ErrorClass::QuotaExhausted { .. }
                )
                && let ProviderKind::ChatGpt { user_dir, .. } = &self.kind
            {
                let allowed = self
                    .steering_config
                    .as_ref()
                    .and_then(|config| config.chatgpt_allowed_accounts.as_ref());
                if let Some(candidate) =
                    llm::chatgpt::accounts(user_dir)?
                        .into_iter()
                        .find(|account| {
                            account.signed_in
                                && !tried_accounts.contains(&account.client_id)
                                && allowed.is_none_or(|set| set.contains(&account.client_id))
                        })
                {
                    // A request does not own the persistent session/role binding. Changing only
                    // its auth would stamp B-owned history into an A-bound conversation. An
                    // operator must explicitly select/rebind a clean session instead.
                    if self.session_bound
                        || self.prompt_cache_key.is_some()
                        || llm::responses::has_native_history(req)
                    {
                        return Err(error.context(
                            "ChatGPT account failover refused: this request has a session/cache identity or native replay history; explicitly select another allowed account and start a clean chat (or rebind portable history) before retrying",
                        ));
                    }
                    // The request was prepared using this profile. Failover must not silently
                    // change capabilities, image handling or token budgets halfway through it.
                    if self
                        .steering_config
                        .as_ref()
                        .and_then(|config| config.active_model_profile())
                        .is_none()
                    {
                        let candidate_profile = llm::chatgpt::scoped_model_profile(
                            user_dir,
                            Some(&candidate.client_id),
                            model,
                        )?;
                        ensure!(
                            serde_json::to_value(&candidate_profile)?
                                == serde_json::to_value(&self.profile)?,
                            "ChatGPT account failover requires a different model profile; explicitly select the account and rebuild the backend before retrying"
                        );
                    }
                    tried_accounts.insert(candidate.client_id.clone());
                    active_account = Some(candidate.client_id);
                    // Unbound portable requests have no external identity to reconcile. Pin
                    // the backend as well as this attempt, even if the next attempt fails:
                    // the next request must not revert to the rejected account.
                    self.account_id = active_account.clone();
                    on_notice(
                        "ChatGPT credential was rejected or quota exhausted; retrying with another signed-in account",
                    );
                    continue;
                }
            }
            if let Some(status @ (401 | 403)) = error
                .downcast_ref::<llm::HttpError>()
                .filter(|_| matches!(class, llm::errors::ErrorClass::Auth))
                .map(|http| http.status)
            {
                let host = llm::host_of(url);
                let key = match host.as_deref() {
                    Some("api.openai.com") => "OPENAI_API_KEY",
                    Some("api.anthropic.com") => "ANTHROPIC_API_KEY",
                    Some("openrouter.ai") => "OPENROUTER_API_KEY",
                    Some("api.groq.com") => "GROQ_API_KEY",
                    Some("api.mistral.ai") => "MISTRAL_API_KEY",
                    Some("api.deepseek.com") => "DEEPSEEK_API_KEY",
                    _ => "the endpoint's configured API key",
                };
                return Err(error.context(format!("provider authentication failed (HTTP {status}); check {key} or sign in through /providers")));
            }
            let class = llm::errors::classify(&error);
            let hinted = error
                .downcast_ref::<llm::HttpError>()
                .and_then(|http| http.retry_after);
            let delay = match llm::retry::decide(&class, hinted, attempt, &retry_policy) {
                llm::retry::Decision::Retry { delay, reason } => {
                    if error
                        .downcast_ref::<llm::stream::IncompleteStream>()
                        .is_some()
                        || error
                            .downcast_ref::<llm::anthropic::IncompleteStream>()
                            .is_some()
                    {
                        on_notice(
                            "Provider stream disconnected or was incomplete; discarding the partial reply and retrying. No partial tool calls will execute.",
                        );
                    }
                    (delay, reason)
                }
                llm::retry::Decision::Stop(Some(message)) => return Err(error.context(message)),
                llm::retry::Decision::Stop(None) => return Err(error),
            };
            let (delay, reason) = delay;
            let cause = format!("{error:#}");
            let cause = cause.lines().next().unwrap_or("").trim();
            let cause: String = if cause.chars().count() > 160 {
                cause
                    .chars()
                    .take(159)
                    .chain(std::iter::once('…'))
                    .collect()
            } else {
                cause.to_owned()
            };
            // How long the failed attempt ran, so a full silence limit and a quick refusal read differently.
            let cause = format!(
                "{cause} (after {:.1}s)",
                attempt_started.elapsed().as_secs_f64()
            );
            on_notice(&format!(
                "{reason}: {cause}; retrying in {:.1}s (attempt {}/{})",
                delay.as_secs_f64(),
                attempt + 1,
                retry_policy.attempts(&class)
            ));
            if !on_event(&llm::StreamEvent::Retry { waiting: true }) {
                return Err(llm::Interrupted.into());
            }
            let start = Instant::now();
            while start.elapsed() < delay {
                if interrupted() {
                    return Err(llm::Interrupted.into());
                }
                std::thread::sleep(
                    delay
                        .saturating_sub(start.elapsed())
                        .min(Duration::from_millis(50)),
                );
            }
        }
        unreachable!("bounded retries return on the final attempt")
    }
}

pub(super) fn exchange(
    endpoint: llm::Endpoint<'_>,
    req: &Chat,
    timeout_secs: u64,
    on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
    control: llm::Control<'_>,
) -> Result<Reply> {
    match on_text {
        Some(callback) => {
            llm::chat_stream_at_controlled(endpoint, req, timeout_secs, &mut **callback, control)
        }
        None => llm::chat_at_controlled(endpoint, req, timeout_secs, control),
    }
}

fn anthropic_exchange(
    endpoint: llm::Endpoint<'_>,
    req: &Chat,
    timeout_secs: u64,
    on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&llm::profiles::ModelProfile>,
    on_event: &mut dyn FnMut(&llm::StreamEvent) -> bool,
) -> Result<Reply> {
    anthropic_exchange_shaped(
        endpoint,
        req,
        timeout_secs,
        on_text,
        interrupt,
        profile,
        on_event,
        None,
    )
}

/// Anthropic Messages with an optional body hook for proxies (Vertex, Copilot, GitLab, Factory).
#[allow(clippy::too_many_arguments)]
pub(super) fn anthropic_exchange_shaped(
    endpoint: llm::Endpoint<'_>,
    req: &Chat,
    timeout_secs: u64,
    on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&llm::profiles::ModelProfile>,
    on_event: &mut dyn FnMut(&llm::StreamEvent) -> bool,
    shape: Option<llm::transports::AnthropicShape<'_>>,
) -> Result<Reply> {
    let llm::Endpoint {
        url,
        auth: api_key,
        model,
        headers,
    } = endpoint;
    if let Some(key) = api_key {
        ensure!(
            !key.is_empty() && key.bytes().all(|b| b.is_ascii_graphic()),
            "api key is empty or contains characters that are not allowed in an HTTP header"
        );
    }
    let claude_code_oauth = api_key.is_some_and(llm::anthropic::subscription_token);
    let replay_scope = llm::model_config::native_replay_scope(
        "anthropic-compaction",
        url,
        Some(model),
        None,
        api_key,
        headers,
    );
    validate_anthropic_compaction_scope(req, &replay_scope, model)?;
    let mut body = anthropic_request_body(model, req, claude_code_oauth, profile)?;
    let streaming = on_text.is_some();
    body["stream"] = serde_json::json!(streaming);
    llm::anthropic_cache::apply(
        &mut body,
        profile,
        claude_code_oauth && llm::host_of(url).as_deref() == Some("api.anthropic.com"),
    );
    if let Some(shape) = shape {
        shape(&mut body)?;
    }
    let use_proxy = !llm::host_of(url).is_some_and(|host| llm::is_local_host(&host));
    // Native Anthropic endpoints have no managed local worker to wait for,
    // even when reached through a loopback proxy. Bound first-event silence too.
    let patience = streaming.then(|| llm::Patience::for_stream(timeout_secs, false));
    let native_compaction = req
        .messages
        .iter()
        .any(|m| m["dial_anthropic_compaction"].is_object());
    let key_scope = api_key.map(anthropic_key_scope);
    let mut fast = profile.is_some_and(|p| p.request_tiers.anthropic_fast(&p.provider))
        && !key_scope
            .as_ref()
            .is_some_and(|scope| FAST_MODE_REJECTED.lock().contains(scope));
    // The 1M-context beta is API-key only; subscription OAuth never advertises it.
    let context_1m = !claude_code_oauth
        && api_key.is_some()
        && profile.is_some_and(|p| p.request_tiers.extended_context);
    let mut response = loop {
        let mut request = llm::agent_with(
            timeout_secs,
            use_proxy,
            Some(interrupt.clone()),
            patience.clone(),
        )
        .post(url)
        .header("anthropic-version", "2023-06-01");
        if streaming {
            request = request.header("Accept", "text/event-stream");
        }
        let mut auth_headers = api_key
            .map(|key| llm::anthropic::auth_headers(url, key))
            .transpose()?
            .unwrap_or_default();
        let mut betas: Vec<&str> = Vec::new();
        if native_compaction {
            betas.push("compact-2026-09-04");
        }
        if fast {
            betas.push("fast-mode-2026-02-01");
        }
        if context_1m {
            betas.push("context-1m-2025-08-07");
        }
        if !betas.is_empty() {
            if let Some((_, beta)) = auth_headers
                .iter_mut()
                .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
            {
                for extra in &betas {
                    beta.push(',');
                    beta.push_str(extra);
                }
            } else {
                auth_headers.push(("anthropic-beta", betas.join(",")));
            }
        }
        for (name, value) in auth_headers {
            request = request.header(name, value);
        }
        ensure!(
            !claude_code_oauth || headers.is_empty(),
            "Claude subscription does not permit custom request headers"
        );
        for (name, value) in headers {
            request = request.header(*name, llm::model_config::private_header_value(value)?);
        }
        // Mutations happen on a per-attempt copy, so a speed retry never re-attests a sent body.
        let mut wire = body.clone();
        if fast {
            wire["speed"] = serde_json::json!("fast");
        }
        if claude_code_oauth {
            // Attest after all body mutations, so the fingerprint covers the wire payload.
            attest_claude_code_body(&mut wire)?;
        }
        llm::usage::transport_attempt()?;
        // Compact JSON: ureq's send_json pretty-prints and the indentation is billed as prompt tokens.
        let bytes = serde_json::to_vec(&wire)?;
        llm::wire::record(&bytes, "http");
        let mut response = request
            .header("Content-Type", "application/json")
            .send(&bytes)
            .map_err(|error| llm::transport_error(error, url, api_key))?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            break response;
        }
        let retry_after = llm::retry::server_delay(response.headers());
        let bytes = llm::read_bounded(response.body_mut().as_reader(), llm::ERROR_EXCERPT_BYTES)
            .context("reading Anthropic error response")?;
        if fast && fast_mode_rejected(status, &bytes.data) {
            // This account refused fast mode: remember it for the process and resend once without it.
            if let Some(scope) = &key_scope {
                FAST_MODE_REJECTED.lock().insert(scope.clone());
            }
            fast = false;
            continue;
        }
        // Classify from the real body (quota wording turns 401/403 into QuotaExhausted); only the
        // displayed excerpt is replaced, so no echoed request or credential reaches the user.
        if matches!(status, 401 | 403) {
            let mut error = llm::http_error(status, &bytes, api_key, retry_after);
            if let Some(http) = error.downcast_mut::<llm::HttpError>() {
                http.excerpt = "check ANTHROPIC_API_KEY or sign in through /providers".into();
            }
            return Err(error);
        }
        return Err(llm::http_error(status, &bytes, api_key, retry_after));
    };
    let is_sse = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"));
    if is_sse {
        let mut accumulator = if claude_code_oauth {
            llm::anthropic::Accumulator::for_claude_code()
        } else {
            llm::anthropic::Accumulator::default()
        };
        let mut forward = |piece: &str| match on_text {
            Some(callback) => callback(piece),
            None => true,
        };
        let patience = patience.unwrap_or_else(|| llm::Patience::for_stream(timeout_secs, false));
        llm::stream::read_events_with_progress(
            response.body_mut().as_reader(),
            llm::MAX_STREAM_BYTES,
            llm::stream::MAX_LINE_BYTES,
            patience,
            llm::anthropic::advances_reply,
            |data| accumulator.feed(data, &mut forward, on_event),
        )
        .map_err(|error| {
            if error.downcast_ref::<llm::stream::StalledStream>().is_some() {
                llm::stream::IncompleteStream(
                    "chat stream was silent until the idle limit; the server may be stuck or overloaded",
                ).into()
            } else {
                llm::response_error(error, api_key)
            }
        })?;
        let mut reply = accumulator
            .finish()
            .map_err(|error| llm::response_error(error, api_key))?;
        if claude_code_oauth {
            for call in &mut reply.tool_calls {
                if let Some(name) = call.name.strip_prefix('_') {
                    call.name = name.to_owned();
                }
            }
        }
        return Ok(reply);
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take((llm::MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("reading Anthropic response")?;
    ensure!(
        bytes.len() <= llm::MAX_RESPONSE_BYTES,
        "Anthropic response exceeds {} bytes",
        llm::MAX_RESPONSE_BYTES
    );
    let mut reply =
        anthropic_reply(&serde_json::from_slice(&bytes).context("invalid Anthropic response")?)?;
    if claude_code_oauth {
        for call in &mut reply.tool_calls {
            if let Some(name) = call.name.strip_prefix('_') {
                call.name = name.to_owned();
            }
        }
    }
    if !reply.reasoning.is_empty()
        && !on_event(&llm::StreamEvent::Reasoning(reply.reasoning.clone()))
    {
        return Err(llm::Interrupted.into());
    }
    if !reply.text.is_empty()
        && let Some(callback) = on_text
        && !callback(&reply.text)
    {
        return Err(llm::Interrupted.into());
    }
    Ok(reply)
}

#[cfg(test)]
fn anthropic_request(model: &str, req: &Chat) -> Result<serde_json::Value> {
    anthropic_request_variant(model, req, false)
}

fn anthropic_request_variant(
    model: &str,
    req: &Chat,
    claude_code_oauth: bool,
) -> Result<serde_json::Value> {
    use serde_json::{Value, json};
    const CLAUDE_CODE_SYSTEM: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in &req.messages {
        let before = messages.len();
        match message["role"].as_str().unwrap_or_default() {
            "assistant" if message["dial_anthropic_compaction"].is_object() => {
                let replay = message["dial_anthropic_compaction"]["history"]
                    .as_array()
                    .context("invalid stored Anthropic compaction history")?;
                ensure!(
                    replay
                        .iter()
                        .all(|m| matches!(m["role"].as_str(), Some("user" | "assistant"))
                            && m["content"].is_array()),
                    "invalid stored Anthropic native compaction messages"
                );
                messages.extend_from_slice(replay);
            }
            // Responses native replay uses Dial wrappers, not Anthropic content blocks. Old
            // sessions may contain both protocols; fall through to shared text/tool calls for
            // foreign replay data, while keeping Claude's opaque signed blocks unchanged.
            "assistant"
                if message["dial_provider_content"]
                    .as_array()
                    .is_some_and(|blocks| {
                        !blocks.is_empty()
                            && blocks.iter().all(|block| {
                                matches!(
                                    block["type"].as_str(),
                                    Some(
                                        "text"
                                            | "tool_use"
                                            | "server_tool_use"
                                            | "thinking"
                                            | "redacted_thinking"
                                            | "web_search_tool_result"
                                            | "web_fetch_tool_result"
                                            | "code_execution_tool_result"
                                    )
                                )
                            })
                    }) =>
            {
                let mut content = message["dial_provider_content"].clone();
                if claude_code_oauth {
                    for block in content.as_array_mut().into_iter().flatten() {
                        if block["type"] == "tool_use"
                            && let Some(name) =
                                block["name"].as_str().filter(|name| !name.starts_with('_'))
                        {
                            block["name"] = json!(format!("_{name}"));
                        }
                    }
                }
                messages.push(json!({"role":"assistant","content":content}));
            }
            "system" | "developer" => {
                if let Some(text) = message["content"].as_str() {
                    // Request-local guidance must not enter the cached system prefix.
                    if message["dial_transient"] == true {
                        messages.push(json!({"role":"user","content":text}));
                    } else {
                        system.push(text.to_owned());
                    }
                }
            }
            "tool" => {
                let mut block = json!({
                    "type":"tool_result","tool_use_id":message["tool_call_id"],"content":message["content"]
                });
                if message["is_error"] == true {
                    block["is_error"] = json!(true);
                }
                // Results of one parallel batch share a single user message: one message per
                // result teaches Claude to stop issuing parallel tool calls.
                let batch = messages.last_mut().filter(|last: &&mut Value| {
                    message["dial_transient"] != true
                        && last["role"] == "user"
                        && last["content"].as_array().is_some_and(|blocks| {
                            blocks.iter().all(|block| block["type"] == "tool_result")
                        })
                });
                match batch.and_then(|last| last["content"].as_array_mut()) {
                    Some(blocks) => blocks.push(block),
                    None => messages.push(json!({"role":"user","content":[block]})),
                }
            }
            "assistant" if message["tool_calls"].is_array() => {
                let mut content = Vec::new();
                if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
                    content.push(json!({"type":"text","text":text}));
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let raw = &call["function"]["arguments"];
                    let parsed: Value = match raw {
                        Value::String(text) => {
                            serde_json::from_str(text).unwrap_or_else(|_| json!({}))
                        }
                        value => value.clone(),
                    };
                    let input = if parsed.is_object() {
                        parsed
                    } else {
                        json!({})
                    };
                    let name = call["function"]["name"].as_str().unwrap_or_default();
                    content.push(json!({"type":"tool_use","id":call["id"],"name":if claude_code_oauth{format!("_{name}")}else{name.to_owned()},"input":input}));
                }
                messages.push(json!({"role":"assistant","content":content}));
            }
            role @ ("user" | "assistant") => {
                let content = llm::images::anthropic_content(&message["content"]);
                // The Messages API rejects empty text blocks and null content.
                if !content.is_null() && !content.as_str().is_some_and(str::is_empty) {
                    messages.push(json!({"role":role,"content":content}));
                }
            }
            _ => {}
        }
        if message["dial_transient"] == true {
            for converted in &mut messages[before..] {
                converted["dial_transient"] = json!(true);
            }
        }
    }
    if claude_code_oauth {
        let first_user_message = req
            .messages
            .iter()
            .find(|message| message["role"] == "user")
            .map(|message| {
                if let Some(text) = message["content"].as_str() {
                    text.to_owned()
                } else {
                    message["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|block| block["text"].as_str())
                        .collect::<String>()
                }
            })
            .unwrap_or_default();
        system.insert(0, claude_billing_header(&first_user_message));
        system.insert(1, CLAUDE_CODE_SYSTEM.into());
    }
    let mut body = json!({"model":model,"max_tokens":req.max_tokens,"messages":messages});
    if !system.is_empty() {
        body["system"] = if claude_code_oauth {
            json!(
                system
                    .iter()
                    .map(|text| json!({"type":"text","text":text}))
                    .collect::<Vec<_>>()
            )
        } else {
            json!(system.join("\n\n"))
        };
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .filter_map(|tool| {
                    let function = &tool["function"];
                    function["name"].as_str().map(|name| {
                        let name = if claude_code_oauth {
                            format!("_{name}")
                        } else {
                            name.to_owned()
                        };
                        let mut schema = json!({"name":name,"input_schema":function["parameters"]});
                        if let Some(description) = function["description"].as_str() {
                            schema["description"] = json!(description);
                        }
                        schema
                    })
                })
                .collect(),
        );
    }
    Ok(body)
}

fn validate_anthropic_compaction_scope(req: &Chat, scope: &str, model: &str) -> Result<()> {
    for message in &req.messages {
        ensure!(
            message["dial_compaction_model"].is_null(),
            "Responses compaction state cannot be replayed through Anthropic; start a clean chat or explicitly compact to a portable summary"
        );
        if let Some(native) = message["dial_anthropic_compaction"].as_object() {
            ensure!(
                native.get("scope").and_then(serde_json::Value::as_str) == Some(scope)
                    && native.get("model").and_then(serde_json::Value::as_str) == Some(model),
                "Anthropic compaction state belongs to another model/account/endpoint; start a clean chat"
            );
        }
    }
    Ok(())
}

/// Key scopes (SHA-256 hex of the API key, never the key) whose account refused `speed:"fast"`.
static FAST_MODE_REJECTED: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashSet::new()));

fn anthropic_key_scope(key: &str) -> String {
    format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(key.as_bytes())
    )
}

/// Fast-mode rejection: a 400 `invalid_request_error` that names the `speed`
/// parameter as not supported, or a 429 rate-limit error about fast-mode entitlement.
fn fast_mode_rejected(status: u16, body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    match status {
        400 => {
            text.contains("invalid_request_error")
                && text.contains("not support")
                && text
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .any(|word| word == "speed")
        }
        429 => text.contains("rate_limit_error") && text.contains("fast mode"),
        _ => false,
    }
}

fn anthropic_request_body(
    model: &str,
    req: &Chat,
    oauth: bool,
    profile: Option<&llm::profiles::ModelProfile>,
) -> Result<serde_json::Value> {
    let mut body = anthropic_request_variant(model, req, oauth)?;
    // Claude thinks by default: a profile that leaves `thinking` unset gets the catalog's mode for
    // this exact model (adaptive where documented, otherwise a bounded budget). `thinking = false`
    // or an unknown model keeps the provider default. Thinking excludes temperature and top_p.
    let default_thinking = profile
        .is_none_or(|p| p.thinking.is_none() && p.top_p.is_none())
        .then(|| llm::profiles::default_anthropic_thinking(model))
        .flatten()
        .and_then(|api| match api {
            llm::profiles::ReasoningApi::AnthropicAdaptive => {
                Some(serde_json::json!({"type":"adaptive"}))
            }
            _ => llm::profiles::default_thinking_budget(req.max_tokens)
                .map(|budget| serde_json::json!({"type":"enabled","budget_tokens":budget})),
        });
    if let Some(thinking) = &default_thinking {
        body["thinking"] = thinking.clone();
    }
    if let Some(profile) = profile {
        profile.validate_request(req, true)?;
        if profile.capabilities.temperature
            && profile.thinking != Some(true)
            && default_thinking.is_none()
        {
            body["temperature"] = serde_json::json!(profile.temperature.unwrap_or(req.temperature));
        }
        if let Some(p) = profile.top_p {
            body["top_p"] = serde_json::json!(p);
        }
        if let Some(thinking) = profile.thinking {
            body["thinking"] = if !thinking {
                serde_json::json!({"type":"disabled"})
            } else if profile.capabilities.reasoning
                == llm::profiles::ReasoningApi::AnthropicAdaptive
            {
                serde_json::json!({"type":"adaptive"})
            } else {
                serde_json::json!({"type":"enabled","budget_tokens":profile.thinking_budget})
            };
        }
        if let Some(effort) = profile.reasoning_effort {
            body["output_config"] = serde_json::json!({"effort":effort.as_str()});
        }
        if !req.tools.is_empty()
            && let Some(parallel) = profile.capabilities.parallel_tool_calls
        {
            body["tool_choice"] =
                serde_json::json!({"type":"auto","disable_parallel_tool_use":!parallel});
        }
        if profile.capabilities.strict_tools {
            for tool in body
                .get_mut("tools")
                .and_then(serde_json::Value::as_array_mut)
                .into_iter()
                .flatten()
            {
                if let Some(parameters) =
                    crate::llm::reliability::strict_parameters(&tool["input_schema"])
                {
                    tool["input_schema"] = parameters;
                    tool["strict"] = serde_json::json!(true);
                }
            }
        }
    }
    Ok(body)
}

const CLAUDE_CODE_VERSION: &str = "2.1.280";
pub(super) const CLAUDE_BILLING_PREFIX: &str = "x-anthropic-billing-header:";
pub(super) const CCH_PLACEHOLDER: &str = "cch=00000";
const CCH_SEED: u64 = 0x4d659218e32a3268;

fn claude_billing_header(first_user_message: &str) -> String {
    use sha2::Digest;
    let chars: Vec<u16> = first_user_message.encode_utf16().collect();
    let key: String = [4, 7, 20]
        .into_iter()
        .map(|index| {
            chars
                .get(index)
                .and_then(|unit| char::from_u32(u32::from(*unit)))
                .unwrap_or('\u{fffd}')
        })
        .collect();
    let suffix = format!(
        "{:x}",
        sha2::Sha256::digest(format!("59cf53e54c78{key}{CLAUDE_CODE_VERSION}").as_bytes())
    );
    format!(
        "{CLAUDE_BILLING_PREFIX} cc_version={CLAUDE_CODE_VERSION}.{}; cc_entrypoint=cli; {CCH_PLACEHOLDER};",
        &suffix[..3]
    )
}

fn xxhash64(bytes: &[u8], seed: u64) -> u64 {
    const P1: u64 = 0x9e3779b185ebca87;
    const P2: u64 = 0xc2b2ae3d27d4eb4f;
    const P3: u64 = 0x165667b19e3779f9;
    const P4: u64 = 0x85ebca77c2b2ae63;
    const P5: u64 = 0x27d4eb2f165667c5;
    fn round(acc: u64, input: u64) -> u64 {
        acc.wrapping_add(input.wrapping_mul(P2))
            .rotate_left(31)
            .wrapping_mul(P1)
    }
    let mut offset = 0;
    let mut hash;
    if bytes.len() >= 32 {
        let mut v1 = seed.wrapping_add(P1).wrapping_add(P2);
        let mut v2 = seed.wrapping_add(P2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(P1);
        while offset <= bytes.len() - 32 {
            v1 = round(
                v1,
                u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()),
            );
            v2 = round(
                v2,
                u64::from_le_bytes(bytes[offset + 8..offset + 16].try_into().unwrap()),
            );
            v3 = round(
                v3,
                u64::from_le_bytes(bytes[offset + 16..offset + 24].try_into().unwrap()),
            );
            v4 = round(
                v4,
                u64::from_le_bytes(bytes[offset + 24..offset + 32].try_into().unwrap()),
            );
            offset += 32;
        }
        hash = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        for value in [v1, v2, v3, v4] {
            let merged = round(0, value);
            hash = (hash ^ merged).wrapping_mul(P1).wrapping_add(P4);
        }
    } else {
        hash = seed.wrapping_add(P5);
    }
    hash = hash.wrapping_add(bytes.len() as u64);
    while offset + 8 <= bytes.len() {
        hash = (hash
            ^ round(
                0,
                u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()),
            ))
        .rotate_left(27)
        .wrapping_mul(P1)
        .wrapping_add(P4);
        offset += 8;
    }
    if offset + 4 <= bytes.len() {
        hash = (hash
            ^ (u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as u64)
                .wrapping_mul(P1))
        .rotate_left(23)
        .wrapping_mul(P2)
        .wrapping_add(P3);
        offset += 4;
    }
    while offset < bytes.len() {
        hash = (hash ^ (bytes[offset] as u64).wrapping_mul(P5))
            .rotate_left(11)
            .wrapping_mul(P1);
        offset += 1;
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(P2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(P3);
    hash ^ (hash >> 32)
}

fn attest_claude_code_body(body: &mut serde_json::Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(body)?;
    let marker = format!("\"system\":[{{\"text\":\"{CLAUDE_BILLING_PREFIX}");
    let Some(start) = bytes
        .windows(marker.len())
        .position(|window| window == marker.as_bytes())
    else {
        return Ok(());
    };
    let search_start = start + marker.len();
    let search_end = (search_start + 150).min(bytes.len());
    let Some(relative) = bytes[search_start..search_end]
        .windows(CCH_PLACEHOLDER.len())
        .position(|window| window == CCH_PLACEHOLDER.as_bytes())
    else {
        bail!("Claude billing attestation placeholder is not anchored to the system header");
    };
    let index = search_start + relative + "cch=".len();
    let attestation = format!("{:05x}", xxhash64(&bytes, CCH_SEED) & 0xfffff);
    bytes[index..index + 5].copy_from_slice(attestation.as_bytes());
    *body = serde_json::from_slice(&bytes).context("encoding Claude billing attestation")?;
    Ok(())
}

/// Anthropic prompt-cache writes from a Messages `usage` object: `(total, 5m, 1h)`. The split is
/// `usage.cache_creation.ephemeral_{5m,1h}_input_tokens`; a bucket the provider did not report is
/// `None`, never zero. The total is `cache_creation_input_tokens`, or the sum of the reported
/// buckets when only the split is present.
pub(super) fn anthropic_cache_writes(
    usage: &serde_json::Value,
) -> (Option<u64>, Option<u64>, Option<u64>) {
    let split = usage.get("cache_creation").filter(|v| v.is_object());
    let five_minute = split.and_then(|s| s["ephemeral_5m_input_tokens"].as_u64());
    let one_hour = split.and_then(|s| s["ephemeral_1h_input_tokens"].as_u64());
    let total = usage["cache_creation_input_tokens"].as_u64().or_else(|| {
        (five_minute.is_some() || one_hour.is_some()).then(|| {
            five_minute
                .unwrap_or(0)
                .saturating_add(one_hour.unwrap_or(0))
        })
    });
    (total, five_minute, one_hour)
}

pub(super) fn anthropic_usage_snapshot(usage: &serde_json::Value) -> llm::usage::Snapshot {
    let cache_read = usage["cache_read_input_tokens"].as_u64();
    let cache_write = anthropic_cache_writes(usage).0;
    llm::usage::Snapshot {
        input: usage["input_tokens"].as_u64().and_then(|input| {
            input
                .checked_add(cache_read.unwrap_or(0))?
                .checked_add(cache_write.unwrap_or(0))
        }),
        output: usage["output_tokens"].as_u64(),
        cache_read,
        cache_write,
        ..Default::default()
    }
}

pub(super) fn anthropic_reply(value: &serde_json::Value) -> Result<Reply> {
    use serde_json::Value;
    let (cache_writes, cache_writes_5m, cache_writes_1h) =
        anthropic_cache_writes(value.get("usage").unwrap_or(&Value::Null));
    let usage = anthropic_usage_snapshot(&value["usage"]);
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let content = value["content"]
        .as_array()
        .context("Anthropic response content is not an array")?;
    ensure!(
        content.len() <= 128,
        "Anthropic response carries too many content blocks"
    );
    for block in content {
        match block["type"].as_str().unwrap_or_default() {
            "text" => text.push_str(block["text"].as_str().unwrap_or_default()),
            "thinking" => reasoning.push_str(block["thinking"].as_str().unwrap_or_default()),
            "tool_use" => tool_calls.push(crate::llm::ToolCall {
                id: block["id"].as_str().unwrap_or_default().to_owned(),
                name: block["name"].as_str().unwrap_or_default().to_owned(),
                arguments: if block["input"].is_object() {
                    block["input"].clone()
                } else {
                    Value::Null
                },
                malformed_arguments: !block["input"].is_object(),
            }),
            _ => {}
        }
    }
    let mut ids = std::collections::BTreeSet::new();
    for call in &tool_calls {
        ensure!(
            !call.id.is_empty() && ids.insert(&call.id),
            "Anthropic tool call has missing or duplicate id"
        );
    }
    let reason = value["stop_reason"].as_str().unwrap_or("stop");
    Ok(Reply {
        text,
        tool_calls,
        finish_reason: match reason {
            "tool_use" => "tool_calls",
            "end_turn" | "stop_sequence" => "stop",
            "max_tokens" | "model_context_window_exceeded" => "length",
            other => other,
        }
        .into(),
        prompt_tokens: usage.input,
        completion_tokens: usage.output,
        reasoning,
        cached_prompt_tokens: value
            .pointer("/usage/cache_read_input_tokens")
            .and_then(Value::as_u64),
        cache_creation_tokens: cache_writes,
        cache_creation_5m_tokens: cache_writes_5m,
        cache_creation_1h_tokens: cache_writes_1h,
        provider_content: value["content"].as_array().cloned().unwrap_or_default(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod real_server;

#[cfg(test)]
mod chatgpt_tests;
