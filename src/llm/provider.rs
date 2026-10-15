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

#[path = "compaction.rs"]
pub(super) mod compaction;
pub use compaction::NativeCompactionReply;

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

pub struct Backend {
    #[cfg(test)]
    chatgpt_url: Option<String>,
    kind: ProviderKind,
    python: PathBuf,
    startup_timeout_secs: u64,
    allow_insecure: bool,
    retry_backoff: Duration,
    remote_idle_timeout_secs: u64,
    server: Option<ModelServer>,
    adapter_monitor: AdapterMonitor,
    metadata: llm::catalog::ModelMetadata,
    profile: Option<llm::profiles::ModelProfile>,
    next_profile: Option<llm::profiles::ModelProfile>,
    protocol: Option<ApiProtocol>,
    headers: llm::model_config::RequestHeaders,
    next_reasoning_guard: Option<crate::config::types::ReasoningGuardConfig>,
    steering_config: Option<crate::config::types::Config>,
    steering_runtime: serde_json::Value,
    usage_scope: Option<llm::usage::Scope>,
    account_id: Option<String>,
    prompt_cache_key: Option<String>,
    session_identity_error: Option<String>,
    /// Provider OAuth grant resolved (and refreshed) before every endpoint attempt.
    oauth: Option<llm::oauth::Binding>,
    /// Stable conversation identity for provider proxy attribution and live steering.
    session_id: Option<String>,
    codex_websocket: bool,
    image_notices: llm::images::NoticeTracker,
    wire: llm::wire::Shared,
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
            #[cfg(test)]
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
            next_reasoning_guard: None,
            steering_config: None,
            steering_runtime: serde_json::json!({"state":"disabled","uses":0}),
            usage_scope: None,
            account_id,
            session_id: llm::factory_droid::new_request_id().ok(),
            prompt_cache_key: None,
            session_identity_error,
            oauth: None,
            codex_websocket: false,
            image_notices: Default::default(),
            wire: Default::default(),
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
            self.wire = Default::default();
        }
        self.session_id = Some(session_id);
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
        session: &mut crate::storage::session::Session,
    ) -> Result<()> {
        if self.session_id.as_deref() != Some(session.id()) {
            self.image_notices = Default::default();
            self.wire = Default::default();
        }
        self.session_id = Some(session.id().to_owned());
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

    pub fn with_remote_idle_timeout(mut self, seconds: u64) -> Self {
        self.remote_idle_timeout_secs = seconds;
        self
    }

    fn response_timeout(&self, worker_timeout_secs: u64) -> u64 {
        let remote = match &self.kind {
            ProviderKind::ChatGpt { .. } | ProviderKind::AnthropicSubscription { .. } => true,
            ProviderKind::Local { .. } => false,
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
    /// Consumed once at the request boundary; transport retries keep the same profile.
    pub fn next_request_profile(&mut self, profile: Option<llm::profiles::ModelProfile>) {
        self.next_profile = profile;
    }
    /// Scoped side requests restore a pending main profile even if no request boundary was reached.
    pub(crate) fn swap_request_profile(
        &mut self,
        profile: Option<llm::profiles::ModelProfile>,
    ) -> Option<llm::profiles::ModelProfile> {
        std::mem::replace(&mut self.next_profile, profile)
    }
    /// Only Dial-owned MLX receives decoder controls; external providers retain their protocol.
    pub(crate) fn next_reasoning_guard(
        &mut self,
        config: &crate::config::types::ReasoningGuardConfig,
    ) {
        self.next_reasoning_guard =
            matches!(self.kind, ProviderKind::Local { .. }).then(|| config.clone());
    }
    /// Forgets the bound Codex WebSocket chain so the next request is sent unchained.
    pub fn reset_provider_stream(&mut self) {
        llm::codex_ws::forget(self.session_id.as_deref().unwrap_or("codex-default"));
    }
    pub fn with_profile(mut self, profile: llm::profiles::ModelProfile) -> Self {
        self.profile = Some(profile);
        self
    }

    pub fn reply_cost(&self, reply: &Reply) -> Option<f64> {
        let remote = match &self.kind {
            ProviderKind::ChatGpt { .. } | ProviderKind::AnthropicSubscription { .. } => {
                return None;
            }
            ProviderKind::Local { .. } => false,
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
            ProviderKind::Local { .. } => return None,
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
        }
    }

    /// Only a local server can load a LoRA adapter.
    pub fn supports_adapters(&self) -> bool {
        matches!(self.kind, ProviderKind::Local { .. })
    }

    pub fn local_inference(&self) -> bool {
        match &self.kind {
            ProviderKind::Local { .. } => true,
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
        let billing = match &self.kind {
            ProviderKind::Local { .. } => llm::usage::Billing::Local,
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
        let result =
            self.chat_with_control_events_inner(adapter, req, limits, on_text, on_notice, on_event);
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
        let profile = self.next_profile.take().or_else(|| self.profile.clone());
        ensure!(
            self.resolved_model(profile.as_ref()).tools != Some(false) || req.tools.is_empty(),
            "the resolved model declares tool calls unsupported; choose a tool-capable model"
        );
        let reasoning_guard = self.next_reasoning_guard.take();
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
        let (url, auth, model, protocol, codex_account_id) = match &self.kind {
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
                #[cfg(test)]
                let url = self.chatgpt_url.clone().unwrap_or(url);
                let (account_id, token) = chatgpt_access.as_ref().unwrap();
                (
                    url,
                    Some(token.as_str()),
                    model,
                    2,
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
                    1,
                    None,
                )
            }
            ProviderKind::Local { model_dir } => {
                if interrupted() {
                    return Err(llm::Interrupted.into());
                }
                *self.adapter_monitor.lock() = AdapterRuntime {
                    adapter: adapter.map(Path::to_path_buf),
                    phase: AdapterPhase::Loading,
                };
                let steering = self.steering_config.as_ref().and_then(|config| {
                    if let Some(reason) = crate::steering::local_unsupported_reason(model_dir) {
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
                    match crate::steering::specification(config, model_dir) {
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
                        server::tool_grammar_enabled(self.steering_config.as_ref()),
                    )
                }) {
                    llm::InferencePhase::Processing
                } else {
                    llm::InferencePhase::Loading
                };
                if !on_event(&llm::StreamEvent::Progress(llm::ModelProgress::new(phase))) {
                    return Err(llm::Interrupted.into());
                }
                let server = match {
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
                } {
                    Ok(server) => server,
                    Err(error) => {
                        self.adapter_monitor.lock().phase = AdapterPhase::Failed;
                        return Err(error);
                    }
                };
                self.adapter_monitor.lock().phase = AdapterPhase::Loaded;
                server.phase("generating")?;
                let result = exchange(
                    llm::Endpoint {
                        headers: &[],
                        url: &llm::completions_url(server.base_url()),
                        auth: None,
                        model: server.request_model(),
                    },
                    req,
                    timeout_secs,
                    &mut on_text,
                    llm::Control {
                        interrupt: Some(Arc::clone(&interrupt)),
                        profile: profile.as_ref(),
                        reasoning_guard: reasoning_guard.as_ref(),
                        on_event,
                    },
                );
                server.phase("loaded")?;
                if steering.is_some() {
                    let previous = self.steering_runtime["state"].clone();
                    self.steering_runtime = crate::steering::runtime(server.base_url())
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
                let result = if interrupted() {
                    Err(llm::Interrupted.into())
                } else {
                    result
                };
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
                        crate::reliability::normalize_reply(&mut reply, &req.tools);
                    }
                    reply
                });
            }
            ProviderKind::Endpoint {
                base_url,
                model,
                api_key,
            } => {
                ensure!(
                    adapter.is_none(),
                    "endpoint providers cannot load LoRA adapters"
                );
                (
                    match self.protocol.unwrap_or(ApiProtocol::ChatCompletions) {
                        ApiProtocol::ChatCompletions => {
                            endpoint_url(base_url, self.allow_insecure)?
                        }
                        ApiProtocol::Anthropic => anthropic_url(base_url, self.allow_insecure)?,
                        ApiProtocol::Responses => {
                            endpoint_url(base_url, self.allow_insecure)?
                                .trim_end_matches("/chat/completions")
                                .to_owned()
                                + "/responses"
                        }
                        _ => {
                            endpoint_url(base_url, self.allow_insecure)?;
                            base_url.trim_end_matches('/').to_owned()
                        }
                    },
                    api_key.as_deref(),
                    model,
                    match self.protocol.unwrap_or(ApiProtocol::ChatCompletions) {
                        ApiProtocol::ChatCompletions => 0,
                        ApiProtocol::Anthropic => 1,
                        ApiProtocol::Responses => 3,
                        ApiProtocol::Gemini => 4,
                        ApiProtocol::Ollama => 5,
                        _ => 6,
                    },
                    None,
                )
            }
            ProviderKind::AnthropicEndpoint {
                base_url,
                model,
                api_key,
            } => {
                ensure!(
                    adapter.is_none(),
                    "endpoint providers cannot load LoRA adapters"
                );
                (
                    match self.protocol {
                        None | Some(ApiProtocol::Anthropic) => {
                            anthropic_url(base_url, self.allow_insecure)?
                        }
                        Some(ApiProtocol::ChatCompletions) => {
                            endpoint_url(base_url, self.allow_insecure)?
                        }
                        Some(ApiProtocol::Responses) => {
                            endpoint_url(base_url, self.allow_insecure)?
                                .trim_end_matches("/chat/completions")
                                .to_owned()
                                + "/responses"
                        }
                        Some(_) => {
                            endpoint_url(base_url, self.allow_insecure)?;
                            base_url.trim_end_matches('/').to_owned()
                        }
                    },
                    api_key.as_deref(),
                    model,
                    match self.protocol {
                        None | Some(ApiProtocol::Anthropic) => 1,
                        Some(ApiProtocol::ChatCompletions) => 0,
                        Some(ApiProtocol::Responses) => 3,
                        Some(ApiProtocol::Gemini) => 4,
                        Some(ApiProtocol::Ollama) => 5,
                        Some(_) => 6,
                    },
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
        );
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
            let result = if protocol == 2 {
                let endpoint = llm::responses::Endpoint {
                    url: &url,
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
                    ),
                    Err(error) => Err(error),
                }
            } else if protocol == 1 {
                anthropic_exchange(
                    llm::Endpoint {
                        headers: &headers,
                        url: &url,
                        auth,
                        model,
                    },
                    req,
                    timeout_secs,
                    &mut on_text,
                    Arc::clone(&interrupt),
                    profile.as_ref(),
                    on_event,
                )
            } else if protocol == 3 {
                llm::responses::exchange_api(
                    llm::Endpoint {
                        headers: &headers,
                        url: &url,
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
                )
            } else if protocol == 4 || protocol == 5 {
                let endpoint = llm::Endpoint {
                    headers: &headers,
                    url: &url,
                    auth,
                    model,
                };
                if protocol == 4 {
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
                        on_event,
                    )
                }
            } else if protocol == 6 {
                llm::transports::exchange(
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
                )
            } else {
                exchange(
                    llm::Endpoint {
                        headers: &headers,
                        url: &url,
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
                )
            };
            let result = result.map_err(|error| resolved_headers.redact(error));
            if interrupted() {
                return Err(llm::Interrupted.into());
            }
            let error = match result {
                Ok(mut reply) => {
                    if profile
                        .as_ref()
                        .is_some_and(|p| p.capabilities.strict_tools)
                    {
                        crate::reliability::normalize_reply(&mut reply, &req.tools);
                    }
                    return Ok(reply);
                }
                Err(error) => error,
            };
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
                    tried_accounts.insert(candidate.client_id.clone());
                    active_account = Some(candidate.client_id);
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
                let host = llm::host_of(&url);
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
                    if matches!(
                        error.downcast_ref::<llm::stream::IncompleteStream>(),
                        Some(_)
                    ) || error
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
            on_notice(&format!(
                "{reason}: {cause}; retrying in {:.1}s (attempt {}/{})",
                delay.as_secs_f64(),
                attempt + 1,
                retry_policy.max_attempts
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
                messages.push(json!({"role":"user","content":[block]}));
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
                    crate::reliability::strict_parameters(&tool["input_schema"])
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
const CLAUDE_BILLING_PREFIX: &str = "x-anthropic-billing-header:";
const CCH_PLACEHOLDER: &str = "cch=00000";
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

pub(super) fn anthropic_reply(value: &serde_json::Value) -> Result<Reply> {
    use serde_json::Value;
    let (cache_writes, cache_writes_5m, cache_writes_1h) =
        anthropic_cache_writes(value.get("usage").unwrap_or(&Value::Null));
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
        prompt_tokens: value
            .pointer("/usage/input_tokens")
            .and_then(Value::as_u64)
            .map(|n| {
                n.saturating_add(
                    value
                        .pointer("/usage/cache_read_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                )
                .saturating_add(cache_writes.unwrap_or(0))
            }),
        completion_tokens: value
            .pointer("/usage/output_tokens")
            .and_then(Value::as_u64),
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
mod tests {
    #[test]
    fn steering_bypass_announces_changes_and_retains_status() {
        let mut backend = super::Backend::new(
            super::ProviderKind::Local {
                model_dir: "test".into(),
            },
            "python".into(),
            10,
        );
        assert!(
            super::Backend::steering_bypass(
                &mut backend.steering_runtime,
                "missing coding vector".into()
            )
            .is_some()
        );
        assert!(
            super::Backend::steering_bypass(
                &mut backend.steering_runtime,
                "missing coding vector".into()
            )
            .is_none()
        );
        assert_eq!(
            backend.steering_runtime()["reason"],
            "missing coding vector"
        );
        assert!(
            super::Backend::steering_bypass(
                &mut backend.steering_runtime,
                "invalid evaluation".into()
            )
            .is_some()
        );
        backend.steering_runtime = serde_json::json!({"state":"used","uses":1});
        assert!(
            super::Backend::steering_bypass(
                &mut backend.steering_runtime,
                "invalid evaluation".into()
            )
            .is_some()
        );
    }
    #[cfg(unix)]
    #[test]
    fn lora_runtime_distinguishes_loading_use_failure_and_release() {
        let dir = tempfile::tempdir().unwrap();
        let python = super::server::tests::fake_python(dir.path());
        std::thread::scope(|scope| {
            let (python, adapter_path) = (&python, dir.path().join("adapter"));
            let broken_check = scope.spawn(move || {
                let mut broken = Backend::new(
                    ProviderKind::Local {
                        model_dir: "bad-model".into(),
                    },
                    python.clone(),
                    3,
                );
                assert!(
                    broken
                        .chat(Some(&adapter_path), &request(), 5, None)
                        .is_err()
                );
                assert_eq!(
                    broken.adapter_monitor().lock().phase,
                    super::AdapterPhase::Failed
                );
            });
            let marker = dir.path().join("server.json");
            let adapter = dir.path().join("adapter");
            let mut local = Backend::new(
                ProviderKind::Local {
                    model_dir: marker.to_string_lossy().into_owned(),
                },
                python.clone(),
                10,
            );
            let monitor = local.adapter_monitor();
            assert_eq!(monitor.lock().phase, super::AdapterPhase::Idle);
            let mut observed_loaded = false;
            let mut text = |_: &str| {
                observed_loaded = monitor.lock().phase == super::AdapterPhase::Loaded;
                true
            };
            local
                .chat(Some(&adapter), &request(), 5, Some(&mut text))
                .unwrap();
            assert!(
                observed_loaded,
                "loading alone must not claim a completed response"
            );
            assert_eq!(monitor.lock().phase, super::AdapterPhase::Used);
            assert_eq!(monitor.lock().adapter.as_deref(), Some(adapter.as_path()));
            let argv: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
            assert!(argv["argv"].as_array().unwrap().iter().any(|arg| {
                arg.as_str()
                    .is_some_and(|arg| arg == format!("--adapter-path={}", adapter.display()))
            }));
            assert!(
                local
                    .chat(Some(&adapter), &request(), 5, Some(&mut |_| false))
                    .is_err()
            );
            assert_eq!(monitor.lock().phase, super::AdapterPhase::Failed);
            local.release();
            assert_eq!(monitor.lock().phase, super::AdapterPhase::Idle);
            assert!(monitor.lock().adapter.is_none());
            broken_check.join().unwrap();
        });
    }
    use super::*;
    use crate::llm::cancel::Patience;
    use crate::llm::fake::{self, Script, header, request_line};
    use serde_json::json;

    fn endpoint(base_url: &str) -> ProviderKind {
        ProviderKind::Endpoint {
            base_url: base_url.into(),
            model: "gpt-x".into(),
            api_key: Some("sk-secret".into()),
        }
    }

    fn backend(kind: ProviderKind) -> Backend {
        let mut backend = Backend::new(kind, PathBuf::from("/nonexistent/python"), 5);
        backend.retry_backoff = Duration::from_millis(1);
        backend
    }

    fn anthropic_endpoint(base_url: &str) -> ProviderKind {
        ProviderKind::AnthropicEndpoint {
            base_url: base_url.into(),
            model: "claude-test".into(),
            api_key: Some("anth-secret".into()),
        }
    }

    fn request() -> Chat {
        Chat {
            messages: vec![json!({"role": "user", "content": "SECRET-PROMPT"})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 16,
            seed: None,
        }
    }

    fn ok(text: &str) -> Script {
        fake::json(
            200,
            &json!({"choices": [{"finish_reason": "stop", "message": {"content": text}}]}),
        )
    }

    #[test]
    fn url_normalization() {
        for (base, want) in [
            (
                "https://api.openai.com/v1",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                "https://api.openai.com/v1/",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                "https://openrouter.ai/api/v1",
                "https://openrouter.ai/api/v1/chat/completions",
            ),
            (
                "http://localhost:11434",
                "http://localhost:11434/v1/chat/completions",
            ),
            (
                "http://127.0.0.1:8000/",
                "http://127.0.0.1:8000/v1/chat/completions",
            ),
            (
                "https://example.com",
                "https://example.com/v1/chat/completions",
            ),
            (
                "https://g.example/v1beta/openai",
                "https://g.example/v1beta/openai/chat/completions",
            ),
            (
                "http://[::1]:9000/v1",
                "http://[::1]:9000/v1/chat/completions",
            ),
        ] {
            assert_eq!(endpoint_url(base, false).unwrap(), want, "{base}");
        }
    }

    #[test]
    fn url_validation() {
        for bad in [
            "ftp://example.com/v1",
            "example.com/v1",
            "file:///etc/passwd",
            "https://user:pw@example.com/v1",
            "https://example.com/v1?key=abc",
            "",
            "https://",
        ] {
            let error = endpoint_url(bad, false).unwrap_err().to_string();
            assert!(!error.contains("pw@") && !error.contains("abc"), "{error}");
            assert!(
                endpoint_url(bad, true).is_err(),
                "{bad} must be rejected even when insecure is allowed"
            );
        }
    }

    #[test]
    fn plain_http_only_for_local_hosts_unless_flagged() {
        for local in [
            "http://localhost:1/v1",
            "http://127.0.0.1:1",
            "http://192.168.1.20:8000/v1",
            "http://10.0.0.5/v1",
            "http://172.16.3.4/v1",
            "http://[::1]:1",
            "http://studio.local:1234/v1",
        ] {
            assert!(endpoint_url(local, false).is_ok(), "{local}");
        }
        for remote in [
            "http://api.example.com/v1",
            "http://8.8.8.8/v1",
            "http://172.32.0.1/v1",
        ] {
            let error = endpoint_url(remote, false).unwrap_err().to_string();
            assert!(error.contains("plain http"), "{error}");
            assert!(endpoint_url(remote, true).is_ok(), "{remote}");
        }
        assert!(endpoint_url("https://api.example.com/v1", false).is_ok());
    }

    #[test]
    fn validate_checks_model_and_key() {
        assert!(validate(&endpoint("https://e.com/v1"), false).is_ok());
        let blank = ProviderKind::Endpoint {
            base_url: "https://e.com".into(),
            model: " ".into(),
            api_key: None,
        };
        assert!(validate(&blank, false).is_err());
        let bad_key = ProviderKind::Endpoint {
            base_url: "https://e.com".into(),
            model: "m".into(),
            api_key: Some("a b".into()),
        };
        let error = validate(&bad_key, false).unwrap_err().to_string();
        assert!(!error.contains("a b"));
        assert!(
            validate(
                &ProviderKind::Local {
                    model_dir: String::new()
                },
                false
            )
            .is_err()
        );
    }

    #[test]
    fn anthropic_url_uses_messages_path_and_endpoint_safety() {
        assert_eq!(
            anthropic_url("https://api.anthropic.com/v1", false).unwrap(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            anthropic_url("https://api.anthropic.com", false).unwrap(),
            "https://api.anthropic.com/v1/messages"
        );
        assert!(anthropic_url("https://user:secret@example.com/v1", false).is_err());
        assert!(anthropic_url("http://api.example.com/v1", false).is_err());
    }

    #[test]
    fn anthropic_request_maps_system_tools_and_tool_results() {
        let request = Chat {
            messages: vec![
                json!({"role":"system","content":"Follow rules"}),
                json!({"role":"user","content":"Read a file"}),
                json!({"role":"assistant","content":null,"tool_calls":[{
                    "id":"call-1","type":"function",
                    "function":{"name":"read_file","arguments":"{\"path\":\"a.txt\"}"}
                }]}),
                json!({"role":"tool","tool_call_id":"call-1","content":"file contents"}),
            ],
            tools: vec![json!({"type":"function","function":{
                "name":"read_file","description":"Read a file",
                "parameters":{"type":"object","properties":{"path":{"type":"string"}}}
            }})],
            temperature: 0.0,
            max_tokens: 128,
            seed: None,
        };
        let body = anthropic_request("claude-test", &request).unwrap();
        assert_eq!(body["system"], "Follow rules");
        assert_eq!(body["max_tokens"], 128);
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][1]["content"][0]["input"]["path"], "a.txt");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call-1");
    }

    #[test]
    fn provider_switch_replays_responses_history_as_claude_text_and_tools() {
        let items = json!([
            {"type":"reasoning","id":"r1","encrypted_content":"responses-only","summary":[]},
            {"type":"message","id":"m1","role":"assistant","content":[{"type":"output_text","text":"Reading now."}]},
            {"type":"function_call","id":"f1","call_id":"call-1","namespace":"dial","name":"read","arguments":"{\"path\":\"a.rs\"}"}
        ]);
        let server = fake::serve(vec![Script::Sse(format!(
            "data: {}\n\n",
            json!({"type":"response.completed","response":{"status":"completed","output":items}})
        ))]);
        let reply = llm::responses::exchange(
            llm::responses::Endpoint {
                headers: &[],
                url: &server.url,
                token: "fixture-token",
                account_id: None,
                model: "fixture-model",
            },
            &request(),
            5,
            &mut None,
            Arc::new(AtomicBool::new(false)),
            None,
            &mut |_| true,
        )
        .unwrap();
        server.requests();
        // Old session files have the untagged native wrapper too; test a saved-history roundtrip.
        let saved = serde_json::to_string(&llm::assistant_message(&reply)).unwrap();
        let restored: serde_json::Value = serde_json::from_str(&saved).unwrap();
        let claude_blocks = json!([
            {"type":"thinking","thinking":"checked","signature":"claude-signed"},
            {"type":"redacted_thinking","data":"claude-redacted"},
            {"type":"text","text":"File inspected."}
        ]);
        let claude_reply =
            anthropic_reply(&json!({"content":claude_blocks,"stop_reason":"end_turn"})).unwrap();
        let mut req = request();
        req.messages.extend([
            restored,
            llm::tool_message("call-1", "contents"),
            llm::assistant_message(&claude_reply),
        ]);
        for oauth in [false, true] {
            let body = anthropic_request_variant("claude-test", &req, oauth).unwrap();
            assert_eq!(
                body["messages"][1]["content"][0],
                json!({"type":"text","text":"Reading now."})
            );
            assert_eq!(
                body["messages"][1]["content"][1],
                json!({"type":"tool_use","id":"call-1","name":if oauth {"_read"} else {"read"},"input":{"path":"a.rs"}})
            );
            assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call-1");
            assert_eq!(body["messages"][3]["content"], claude_blocks);
            assert!(!body.to_string().contains("dial_response_item"));
            assert!(!body.to_string().contains("responses-only"));
        }
        // Switching back still preserves Responses IDs/encrypted reasoning, without leaking
        // Claude's signed thinking into the Responses protocol or changing saved history.
        let body = llm::responses::request("fixture-model", &req, None).unwrap();
        assert_eq!(
            &body["input"].as_array().unwrap()[1..4],
            items.as_array().unwrap()
        );
        assert_eq!(body["input"][4]["call_id"], "call-1");
        assert_eq!(body["input"][5]["content"], "File inspected.");
        assert!(!body.to_string().contains("claude-signed"));
    }
    #[test]
    fn claude_subscription_request_matches_claude_code_message_conventions() {
        let request = Chat {
            messages: vec![
                json!({"role":"system","content":"Follow rules"}),
                json!({"role":"user","content":"012345678901234567890"}),
                json!({"role":"assistant","content":null,"tool_calls":[{"id":"call-1","type":"function","function":{"name":"read_file","arguments":"{}"}}]}),
            ],
            tools: vec![
                json!({"type":"function","function":{"name":"read_file","parameters":{"type":"object","properties":{}}}}),
            ],
            temperature: 0.0,
            max_tokens: 128,
            seed: None,
        };
        let mut body = anthropic_request_variant("claude-test", &request, true).unwrap();
        assert_eq!(
            body["system"][0]["text"].as_str().unwrap(),
            claude_billing_header("012345678901234567890")
        );
        assert!(
            body["system"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("x-anthropic-billing-header: cc_version=2.1.280.")
        );
        assert_eq!(
            body["system"][1]["text"],
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
        assert_eq!(body["system"][2]["text"], "Follow rules");
        assert_eq!(body["tools"][0]["name"], "_read_file");
        assert_eq!(body["messages"][1]["content"][0]["name"], "_read_file");
        body["stream"] = serde_json::json!(true);
        attest_claude_code_body(&mut body).unwrap();
        let encoded = serde_json::to_vec(&body).unwrap();
        let text = std::str::from_utf8(&encoded).unwrap();
        let cch = text
            .split("cch=")
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        assert_eq!(cch.len(), 5);
        let mut unattested = encoded.clone();
        let cch_start = unattested
            .windows(4)
            .position(|part| part == b"cch=")
            .unwrap()
            + 4;
        unattested[cch_start..cch_start + 5].copy_from_slice(b"00000");
        assert_eq!(
            cch,
            format!("{:05x}", xxhash64(&unattested, CCH_SEED) & 0xfffff)
        );
    }

    #[test]
    fn claude_subscription_exchange_reaches_transport_after_finalizing_body() {
        use llm::profiles::*;
        let profile = ModelProfile {
            provider: "anthropic".into(),
            model: "claude-test".into(),
            capabilities: ModelCapabilities {
                temperature: true,
                prompt_cache: true,
                ..Default::default()
            },
            temperature: Some(0.3),
            prompt_cache: Some(true),
            ..Default::default()
        };
        for streaming in [false, true] {
            let mut accept_text = |_: &str| true;
            let mut on_text: Option<&mut dyn FnMut(&str) -> bool> =
                streaming.then_some(&mut accept_text);
            // Exercise subscription preparation in the real exchange path, then cancel
            // at the transport boundary before DNS or any credential is sent.
            let error = anthropic_exchange(
                llm::Endpoint {
                    headers: &[],
                    url: "https://api.anthropic.com/v1/messages",
                    auth: Some("sk-ant-oat01-test"),
                    model: "claude-test",
                },
                &request(),
                5,
                &mut on_text,
                Arc::new(AtomicBool::new(true)),
                Some(&profile),
                &mut |_| true,
            )
            .unwrap_err();
            assert!(
                error.downcast_ref::<llm::TransportError>().is_some()
                    && error.to_string().contains("interrupted"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn claude_fingerprint_hash_matches_xxhash64_reference_vector() {
        assert_eq!(
            xxhash64(b"The quick brown fox jumps over the lazy dog", 0),
            0x0b242d361fda71bc
        );
        assert_eq!(xxhash64(b"", 0), 0xef46db3751d8e999);
        assert_ne!(
            claude_billing_header("0123A5678901234567890"),
            claude_billing_header("012345678901234567890")
        );
    }

    #[test]
    fn anthropic_reply_preserves_text_and_tool_calls() {
        let reply = anthropic_reply(&json!({
            "content":[
                {"type":"text","text":"Reading"},
                {"type":"tool_use","id":"tool-9","name":"read_file","input":{"path":"a.txt"}}
            ],
            "stop_reason":"tool_use",
            "usage":{"input_tokens":12,"output_tokens":4}
        }))
        .unwrap();
        assert_eq!(reply.text, "Reading");
        assert_eq!(reply.finish_reason, "tool_calls");
        assert_eq!(reply.prompt_tokens, Some(12));
        assert_eq!(reply.completion_tokens, Some(4));
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].id, "tool-9");
        assert_eq!(reply.tool_calls[0].arguments, json!({"path":"a.txt"}));
    }

    #[test]
    fn anthropic_chat_uses_key_header_and_messages_wire_format() {
        let server = fake::serve(vec![fake::json(
            200,
            &json!({
                "content":[{"type":"text","text":"hello"}],
                "stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}
            }),
        )]);
        let mut remote = backend(anthropic_endpoint(&format!("{}/v1", server.url)));
        let reply = remote.chat(None, &request(), 5, None).unwrap();
        assert_eq!(reply.text, "hello");
        let seen = server.requests().remove(0);
        assert_eq!(request_line(&seen), "POST /v1/messages HTTP/1.1");
        assert_eq!(header(&seen, "x-api-key").as_deref(), Some("anth-secret"));
        assert_eq!(
            header(&seen, "anthropic-version").as_deref(),
            Some("2023-06-01")
        );
        let body = fake::body(&seen);
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["max_tokens"], 16);
        assert_eq!(body["messages"][0]["content"][0]["text"], "SECRET-PROMPT");
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
    }

    #[test]
    fn anthropic_auth_errors_do_not_expose_key_or_prompt() {
        let server = fake::serve(vec![fake::json(
            401,
            &json!({"error":"invalid anth-secret SECRET-PROMPT"}),
        )]);
        let mut remote = backend(anthropic_endpoint(&format!("{}/v1", server.url)));
        let error = remote
            .chat(None, &request(), 5, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("HTTP 401"), "{error}");
        assert!(
            !error.contains("anth-secret") && !error.contains("SECRET-PROMPT"),
            "{error}"
        );
    }

    #[test]
    fn anthropic_callback_receives_buffered_text_once() {
        let server = fake::serve(vec![fake::json(
            200,
            &json!({
                "content":[{"type":"text","text":"one"},{"type":"text","text":" answer"}],
                "stop_reason":"end_turn"
            }),
        )]);
        let mut remote = backend(anthropic_endpoint(&format!("{}/v1", server.url)));
        let mut pieces = Vec::new();
        let mut callback = |text: &str| {
            pieces.push(text.to_owned());
            true
        };
        let reply = remote
            .chat(None, &request(), 5, Some(&mut callback))
            .unwrap();
        assert_eq!(reply.text, "one answer");
        assert_eq!(pieces, ["one answer"]);
    }

    #[test]
    fn describe_and_debug_never_show_the_key_or_userinfo() {
        let kind = ProviderKind::Endpoint {
            base_url: "https://user:hunter2@example.com/v1?token=abc".into(),
            model: "m".into(),
            api_key: Some("sk-secret".into()),
        };
        let shown = format!("{kind:?} {}", backend(kind.clone()).describe());
        for secret in ["sk-secret", "hunter2", "token=abc"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        assert!(shown.contains("example.com"));

        let anthropic = ProviderKind::AnthropicEndpoint {
            base_url: "https://api.anthropic.com/v1".into(),
            model: "claude-test".into(),
            api_key: Some("anth-secret".into()),
        };
        assert!(!format!("{anthropic:?}").contains("anth-secret"));
    }

    #[test]
    fn capabilities_and_labels() {
        let local = backend(ProviderKind::Local {
            model_dir: "/models/SmolLM2".into(),
        });
        assert!(local.supports_adapters());
        assert_eq!(local.model_label(), SERVER_MODEL);
        assert!(local.describe().contains("SmolLM2"));
        let remote = backend(endpoint("https://e.com/v1"));
        assert!(!remote.supports_adapters());
        assert_eq!(remote.model_label(), "gpt-x");
    }

    #[test]
    fn endpoint_rejects_adapters_and_bad_urls_without_network() {
        let mut remote = backend(endpoint("https://e.com/v1"));
        let error = remote
            .chat(Some(Path::new("/adapter")), &request(), 5, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("adapter"), "{error}");
        let mut insecure = backend(endpoint("http://api.example.com/v1"));
        let error = insecure
            .chat(None, &request(), 5, None)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("plain http") && !error.contains("sk-secret"),
            "{error}"
        );
    }

    #[test]
    fn endpoint_chat_sends_key_model_and_path() {
        let server = fake::serve(vec![ok("pong")]);
        let mut remote = backend(endpoint(&format!("{}/v1", server.url)));
        let reply = remote.chat(None, &request(), 5, None).unwrap();
        assert_eq!(reply.text, "pong");
        let seen = server.requests().remove(0);
        assert_eq!(request_line(&seen), "POST /v1/chat/completions HTTP/1.1");
        assert_eq!(
            header(&seen, "authorization").as_deref(),
            Some("Bearer sk-secret")
        );
        assert_eq!(fake::body(&seen)["model"], "gpt-x");
    }

    #[test]
    fn endpoint_streams_when_a_callback_is_given() {
        let chunk = |c: &str| {
            format!(
                "data: {}\n\n",
                json!({"choices": [{"delta": {"content": c}}]})
            )
        };
        let body = format!(
            "{}{}data: {}\n\ndata: [DONE]\n\n",
            chunk("a"),
            chunk("b"),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})
        );
        let server = fake::serve(vec![Script::Sse(body)]);
        let mut remote = backend(endpoint(&format!("{}/v1", server.url)));
        let mut seen = String::new();
        let mut on_text = |piece: &str| {
            seen.push_str(piece);
            true
        };
        let reply = remote
            .chat(None, &request(), 5, Some(&mut on_text))
            .unwrap();
        assert_eq!(reply.text, "ab");
        assert_eq!(seen, "ab");
        assert_eq!(fake::body(&server.requests()[0])["stream"], true);
    }

    #[test]
    fn image_references_reach_both_provider_wire_formats() {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let root = tempfile::tempdir().unwrap();
        let png = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=").unwrap();
        std::fs::write(root.path().join("tiny.png"), &png).unwrap();
        let (message, warnings) = crate::agent::context::expand_message(
            root.path(),
            "Describe @tiny.png",
            &std::sync::atomic::AtomicBool::new(false),
        );
        assert!(warnings.is_empty());
        assert!(!message.to_string().contains("base64"));
        for (anthropic, vision) in [
            (false, Some(true)),
            (true, Some(true)),
            (false, None),
            (true, None),
        ] {
            let response = if anthropic {
                fake::json(
                    200,
                    &json!({"content":[{"type":"text","text":"pixel"}],"stop_reason":"end_turn"}),
                )
            } else {
                ok("pixel")
            };
            let server = fake::serve(vec![response]);
            let kind = if anthropic {
                anthropic_endpoint(&server.url)
            } else {
                endpoint(&server.url)
            };
            let mut remote = backend(kind).with_metadata(llm::catalog::ModelMetadata {
                vision,
                ..Default::default()
            });
            let mut req = request();
            req.messages = vec![message.clone()];
            assert_eq!(remote.chat(None, &req, 5, None).unwrap().text, "pixel");
            let body = fake::body(&server.requests()[0]);
            let image = &body["messages"][0]["content"][1];
            let observation = remote.wire_observation().unwrap();
            assert_eq!(observation.boundary, "final_provider_wire");
            assert!(observation.payload_bytes > png.len());
            assert_eq!(
                observation.payload_sha256,
                format!(
                    "{:x}",
                    <sha2::Sha256 as sha2::Digest>::digest(serde_json::to_vec(&body).unwrap())
                )
            );
            if anthropic {
                assert_eq!(image["type"], "image");
                assert_eq!(image["source"]["media_type"], "image/png");
                assert_eq!(image["source"]["data"], STANDARD.encode(&png));
            } else {
                assert_eq!(image["type"], "image_url");
                assert_eq!(
                    image["image_url"]["url"],
                    format!("data:image/png;base64,{}", STANDARD.encode(&png))
                );
            }
            assert!(!body.to_string().contains("dial_images"));
        }
        let server = fake::serve(vec![ok("text only")]);
        let mut remote =
            backend(endpoint(&server.url)).with_metadata(llm::catalog::ModelMetadata {
                vision: Some(false),
                ..Default::default()
            });
        let mut req = request();
        req.messages = vec![message];
        let mut notices = Vec::new();
        remote
            .chat_with_control(
                None,
                &req,
                5,
                None,
                &mut |text| notices.push(text.to_owned()),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("cannot view images"))
        );
        let content = fake::body(&server.requests()[0])["messages"][0]["content"].clone();
        assert!(content.as_str().unwrap().starts_with("Describe @tiny.png"));
        assert!(content.as_str().unwrap().contains("Image unavailable"));
        assert!(!content.to_string().contains("image_url"));
    }

    #[test]
    fn stale_image_notice_is_once_per_conversation_with_stable_wire_history() {
        let root = tempfile::tempdir().unwrap();
        let image = root.path().join("shot.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\noriginal").unwrap();
        let reference = llm::images::attachment(root.path(), Path::new("shot.png")).unwrap();
        std::fs::write(&image, b"\x89PNG\r\n\x1a\nreplacement").unwrap();
        let server = fake::serve(vec![ok("first"), ok("second"), ok("third")]);
        let mut remote = backend(endpoint(&server.url)).with_session_id("one");
        let mut req = request();
        req.messages = vec![json!({"role":"user","content":"inspect","dial_images":[reference]})];
        let mut notices = Vec::new();
        // Compaction and ordinary requests share notice state.
        remote.prepare_compaction_request(&req, &mut |n| notices.push(n.to_owned()));
        for _ in 0..2 {
            remote
                .chat_with_control(
                    None,
                    &req,
                    5,
                    None,
                    &mut |n| notices.push(n.to_owned()),
                    Arc::new(AtomicBool::new(false)),
                )
                .unwrap();
        }
        assert_eq!(notices.len(), 1);
        remote = remote.with_session_id("two");
        remote
            .chat_with_control(
                None,
                &req,
                5,
                None,
                &mut |n| notices.push(n.to_owned()),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert_eq!(notices.len(), 2, "a new conversation gets its own warning");
        let requests = server.requests();
        assert_eq!(
            fake::body(&requests[0])["messages"],
            fake::body(&requests[1])["messages"]
        );
        let content = fake::body(&requests[0])["messages"][0]["content"].clone();
        assert_eq!(
            content,
            "inspect\n\n[Image unavailable: shot.png; changed; reattach]"
        );
        assert!(!fake::body(&requests[0]).to_string().contains("image_url"));
    }

    #[cfg(unix)]
    #[test]
    fn local_vision_model_receives_pixels_and_uses_one_supervised_vlm_server() {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let root = tempfile::tempdir().unwrap();
        let model = root.path().join("vision-model");
        std::fs::create_dir(&model).unwrap();
        std::fs::write(
            model.join("config.json"),
            r#"{"vision_config":{},"text_config":{}}"#,
        )
        .unwrap();
        // A real 1x1 PNG: image attachments are decoded for dimensions before they reach a model.
        let png = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=",
        )
        .unwrap();
        std::fs::write(root.path().join("image.png"), &png).unwrap();
        let mut local = Backend::new(
            ProviderKind::Local {
                model_dir: model.to_string_lossy().into_owned(),
            },
            server::tests::fake_python(root.path()),
            30,
        );
        assert!(local.accepts_image_input());
        let mut req = request();
        req.messages[0]["dial_images"] =
            json!([llm::images::attachment(root.path(), Path::new("image.png")).unwrap()]);
        let mut notices = Vec::new();
        for _ in 0..2 {
            assert_eq!(
                local
                    .chat_with_control(
                        None,
                        &req,
                        5,
                        None,
                        &mut |notice| notices.push(notice.to_owned()),
                        Arc::new(AtomicBool::new(false))
                    )
                    .unwrap()
                    .text,
                "pong"
            );
        }
        assert!(notices.is_empty(), "{notices:?}");
        let info: serde_json::Value =
            serde_json::from_slice(&std::fs::read(model.join("info.json")).unwrap()).unwrap();
        assert_eq!(info["engine"], "mlx-vlm");
        assert_eq!(info["apc"], "0");
        assert!(
            info["argv"]
                .as_array()
                .unwrap()
                .contains(&json!("--vision-cache-size=1"))
        );
        assert!(!info["argv"].as_array().unwrap().iter().any(|a| {
            a.as_str()
                .is_some_and(|s| s.starts_with("--prompt-cache-bytes="))
        }));
        let requests = info["requests"].as_array().unwrap();
        assert_eq!(
            requests.len(),
            3,
            "one warmup and two requests reuse the same server"
        );
        for wire in &requests[1..] {
            assert_eq!(wire["model"], model.to_string_lossy().as_ref());
            assert_eq!(
                wire["messages"][0]["content"][1]["image_url"]["url"],
                format!("data:image/png;base64,{}", STANDARD.encode(&png))
            );
            assert!(!wire.to_string().contains("dial_images"));
        }
        let disabled = llm::profiles::ModelProfile {
            capabilities: llm::profiles::ModelCapabilities {
                vision: Some(false),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!local.image_input_for(Some(&disabled)));
        drop(local);
        let text = backend(ProviderKind::Local {
            model_dir: root.path().to_string_lossy().into_owned(),
        });
        assert!(!text.accepts_image_input());
    }

    #[cfg(unix)]
    #[test]
    fn vision_steering_status_and_notice_respect_manual_automatic_and_disabled_settings() {
        let shared = tempfile::tempdir().unwrap();
        let python = server::tests::fake_python(shared.path());
        let python = &python;
        std::thread::scope(|scope| {
            for (enabled, automatic, strength, state, notice_count) in [
                (true, false, 0.5, "bypassed", 1),
                (false, true, 0.5, "disabled", 0),
                (true, true, 0.0, "disabled", 0),
                (false, false, 0.5, "disabled", 0),
            ] {
                scope.spawn(move || {
                    let root = tempfile::tempdir().unwrap();
                    let model = root.path().join("vision-model");
                    std::fs::create_dir(&model).unwrap();
                    std::fs::write(model.join("config.json"), r#"{"vision_config":{}}"#).unwrap();
                    let config = crate::config::types::Config {
                        model: model.to_string_lossy().into_owned(),
                        user_dir: root.path().join("user"),
                        state_dir: root.path().join("state"),
                        steering: crate::steering::Config {
                            enabled,
                            automatic,
                            strength,
                            ..Default::default()
                        },
                        ..Default::default()
                    };
                    let mut local = Backend::new(
                        ProviderKind::Local {
                            model_dir: config.model.clone(),
                        },
                        python.clone(),
                        30,
                    )
                    .with_steering(&config);
                    let mut notices = Vec::new();
                    for _ in 0..2 {
                        assert_eq!(
                            local
                                .chat_with_control(
                                    None,
                                    &request(),
                                    5,
                                    None,
                                    &mut |notice| notices.push(notice.to_owned()),
                                    Arc::new(AtomicBool::new(false)),
                                )
                                .unwrap()
                                .text,
                            "pong"
                        );
                    }
                    assert_eq!(local.steering_runtime()["state"], state);
                    assert_eq!(local.steering_runtime()["uses"], 0);
                    assert_eq!(notices.len(), notice_count, "{notices:?}");
                    assert!(!crate::steering::vector_path(&config).exists());
                });
            }
        });
    }

    #[test]
    fn costs_require_complete_usage_supported_prices_and_remote_provider() {
        let metadata = llm::catalog::ModelMetadata {
            vision: None,
            pricing: Some(llm::catalog::TokenPricing {
                prompt: 0.000001,
                completion: 0.000003,
                cache_read: None,
                cache_write: None,
            }),
            ..Default::default()
        };
        let remote = backend(endpoint("https://api.example.com")).with_metadata(metadata);
        let reply = Reply {
            text: String::new(),
            tool_calls: vec![],
            finish_reason: "stop".into(),
            prompt_tokens: Some(1000),
            completion_tokens: Some(500),
            ..Default::default()
        };
        assert!((remote.reply_cost(&reply).unwrap() - 0.0025).abs() < 1e-12);
        let local = backend(endpoint("http://localhost:1234")).with_metadata(metadata);
        assert_eq!(local.reply_cost(&reply), None);
        let mut missing = reply.clone();
        missing.prompt_tokens = None;
        assert_eq!(remote.reply_cost(&missing), None);
        assert_eq!(
            backend(endpoint("https://api.example.com")).reply_cost(&reply),
            None
        );
    }

    #[test]
    fn bad_record_mac_alerts_retry_on_fresh_tls_connections_and_stop_at_the_bound() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let peer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut connections = 0;
            while connections < 10 && Instant::now() < deadline {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("TLS fixture accept: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                // Consume the ClientHello, then send a real TLS fatal BadRecordMac alert.
                // No certificate is accepted and no HTTP prompt or credentials reach the peer.
                let mut header = [0u8; 5];
                stream.read_exact(&mut header).unwrap();
                assert_eq!(header[0], 0x16);
                let length = usize::from(u16::from_be_bytes([header[3], header[4]]));
                stream.read_exact(&mut vec![0; length]).unwrap();
                stream
                    .write_all(&[0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x14])
                    .unwrap();
                connections += 1;
            }
            connections
        });
        let mut remote = backend(endpoint(&url));
        let mut notices = Vec::new();
        let error = remote
            .chat_with_control(
                None,
                &request(),
                5,
                None,
                &mut |notice| notices.push(notice.to_owned()),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap_err();
        assert_eq!(peer.join().unwrap(), 10, "{error:#}");
        assert_eq!(notices.len(), 9);
        assert!(format!("{error:#}").contains("received fatal alert: BadRecordMac"));
        assert!(error.to_string().contains("failed after 10 attempts"));
    }

    #[test]
    fn connection_failures_retry_but_healthy_silence_keeps_the_same_stream() {
        let server = fake::serve(vec![Script::Hangup, ok("reconnected")]);
        let mut remote = backend(endpoint(&server.url));
        assert_eq!(
            remote.chat(None, &request(), 5, None).unwrap().text,
            "reconnected"
        );
        assert_eq!(server.requests().len(), 2);

        let pieces = vec![
            ": thinking\n\n".into(),
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{"content":"after silence"},"finish_reason":"stop"}]})
            ),
        ];
        let server = fake::serve(vec![Script::SseSlow {
            pieces,
            pause: Duration::from_millis(350),
        }]);
        let mut remote = backend(endpoint(&server.url));
        let mut text = String::new();
        let mut request = request();
        request.messages[0]["content"] = json!("long prompt ".repeat(2000));
        let reply = remote
            .chat(
                None,
                &request,
                3600,
                Some(&mut |piece| {
                    text.push_str(piece);
                    true
                }),
            )
            .unwrap();
        assert_eq!(reply.text, "after silence");
        assert_eq!(text, "after silence");
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn transient_failures_retry_with_visible_notices_and_stop_at_the_bound() {
        for status in [408, 409, 425, 429, 500, 502, 503, 504, 529] {
            let first = Script::HttpHeaders {
                status,
                headers: vec![("Retry-After", "0".into())],
                body: b"{\"error\":\"busy\"}".to_vec(),
            };
            let server = fake::serve(vec![first, ok("recovered")]);
            let mut remote = backend(endpoint(&server.url));
            let mut notices = Vec::new();
            let mut notify = |text: &str| notices.push(text.to_owned());
            assert_eq!(
                remote
                    .chat_with_control(
                        None,
                        &request(),
                        5,
                        None,
                        &mut notify,
                        Arc::new(AtomicBool::new(false))
                    )
                    .unwrap()
                    .text,
                "recovered"
            );
            assert_eq!(notices.len(), 1);
            assert_eq!(server.requests().len(), 2);
        }
        let server = fake::serve(
            (0..10)
                .map(|_| fake::json(503, &json!({"error":"busy"})))
                .collect(),
        );
        let mut remote = backend(endpoint(&server.url));
        let error = remote.chat(None, &request(), 5, None).unwrap_err();
        assert!(
            error.to_string().contains("failed after 10 attempts")
                && error.to_string().contains("Say \"continue\" to retry"),
            "{error:#}"
        );
        assert_eq!(error.downcast_ref::<llm::HttpError>().unwrap().status, 503);
        assert_eq!(server.requests().len(), 10);
    }

    #[test]
    fn backoff_is_interruptible_and_obeys_retry_after() {
        let server = fake::serve(vec![Script::HttpHeaders {
            status: 429,
            headers: vec![("Retry-After", "4".into())],
            body: b"{}".to_vec(),
        }]);
        let mut remote = backend(endpoint(&server.url));
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut notice = String::new();
        let start = Instant::now();
        let error = remote
            .chat_with_control(
                None,
                &request(),
                5,
                None,
                &mut |text| {
                    notice = text.into();
                    cancelled.store(true, Ordering::SeqCst);
                },
                Arc::clone(&cancelled),
            )
            .unwrap_err();
        assert!(error.downcast_ref::<llm::Interrupted>().is_some());
        assert!(notice.contains("4.0s"));
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn permanent_statuses_never_retry() {
        for status in [400, 401, 403, 404, 501] {
            let server = fake::serve(vec![fake::json(status, &json!({"error":"permanent"}))]);
            let mut remote = backend(endpoint(&server.url));
            assert_eq!(
                remote
                    .chat(None, &request(), 5, None)
                    .unwrap_err()
                    .downcast_ref::<llm::HttpError>()
                    .unwrap()
                    .status,
                status
            );
            assert_eq!(server.requests().len(), 1);
        }
    }

    #[test]
    fn disconnected_streams_retry_with_fresh_replies_and_identical_requests() {
        for delta in [
            json!({"content":"partial"}),
            json!({"tool_calls":[{"index":0,"id":"call","function":{"name":"read","arguments":"{"}}]}),
        ] {
            for disconnect in [false, true] {
                let partial = format!("data: {}\n\n", json!({"choices":[{"delta":delta}]}));
                let server = fake::serve(vec![
                    if disconnect {
                        Script::SseDisconnect(partial)
                    } else {
                        Script::Sse(partial)
                    },
                    Script::Sse(format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        json!({"choices":[{"delta":{"content":"recovered"},"finish_reason":"stop"}]})
                    )),
                ]);
                let mut remote = backend(endpoint(&server.url));
                let mut notices = Vec::new();
                let reply = remote
                    .chat_with_control(
                        None,
                        &request(),
                        5,
                        Some(&mut |_| true),
                        &mut |text| notices.push(text.to_owned()),
                        Arc::new(AtomicBool::new(false)),
                    )
                    .unwrap();
                assert_eq!(reply.text, "recovered");
                assert!(reply.tool_calls.is_empty());
                assert!(
                    notices
                        .iter()
                        .any(|n| n.contains("discarding the partial reply"))
                );
                let requests = server.requests();
                assert_eq!(requests.len(), 2);
                assert_eq!(fake::body(&requests[0]), fake::body(&requests[1]));
            }
        }
    }

    #[test]
    fn disconnected_stream_retries_are_bounded_and_cancellable() {
        let server = fake::serve(
            (0..10)
                .map(|_| Script::SseDisconnect(String::new()))
                .collect(),
        );
        let error = backend(endpoint(&server.url))
            .chat(None, &request(), 5, Some(&mut |_| true))
            .unwrap_err();
        assert!(error.to_string().contains("after 10 attempts"), "{error:#}");
        assert_eq!(server.requests().len(), 10);

        let server = fake::serve(vec![Script::SseDisconnect(String::new())]);
        let cancelled = Arc::new(AtomicBool::new(false));
        let error = backend(endpoint(&server.url))
            .chat_with_control(
                None,
                &request(),
                5,
                Some(&mut |_| true),
                &mut |_| cancelled.store(true, Ordering::SeqCst),
                Arc::clone(&cancelled),
            )
            .unwrap_err();
        assert!(error.downcast_ref::<llm::Interrupted>().is_some());
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn disconnect_after_chat_finish_reason_does_not_regenerate_the_reply() {
        let body = format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":{"content":"complete"},"finish_reason":"stop"}]})
        );
        let server = fake::serve(vec![Script::SseDisconnect(body)]);
        let reply = backend(endpoint(&server.url))
            .chat(None, &request(), 5, Some(&mut |_| true))
            .unwrap();
        assert_eq!(reply.text, "complete");
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn client_errors_and_interruptions_are_not_retried() {
        let server = fake::serve(vec![fake::json(401, &json!({"error": "no"}))]);
        let mut remote = backend(endpoint(&format!("{}/v1", server.url)));
        let error = remote.chat(None, &request(), 5, None).unwrap_err();
        assert_eq!(
            error.downcast_ref::<llm::HttpError>().map(|e| e.status),
            Some(401)
        );

        let pieces = (0..20)
            .map(|i| {
                format!(
                    "data: {}\n\n",
                    json!({"choices": [{"delta": {"content": format!("w{i}")}}]})
                )
            })
            .collect();
        let server = fake::serve(vec![Script::SseSlow {
            pieces,
            pause: Duration::from_millis(50),
        }]);
        let mut remote = backend(endpoint(&format!("{}/v1", server.url)));
        let mut stop = |_: &str| false;
        let error = remote
            .chat(None, &request(), 10, Some(&mut stop))
            .unwrap_err();
        assert!(
            error.downcast_ref::<llm::Interrupted>().is_some(),
            "{error:#}"
        );
    }

    #[test]
    fn context_window_from_local_metadata_not_endpoint_name_guesses() {
        let dir = tempfile::tempdir().unwrap();
        let local = ProviderKind::Local {
            model_dir: dir.path().to_str().unwrap().into(),
        };
        assert_eq!(context_window_hint(&local), None, "missing config.json");
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"max_position_embeddings": 8192}"#,
        )
        .unwrap();
        assert_eq!(context_window_hint(&local), Some(8192));
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"text_config": {"max_position_embeddings": 32768}}"#,
        )
        .unwrap();
        assert_eq!(context_window_hint(&local), Some(32768));
        std::fs::write(dir.path().join("config.json"), "{broken").unwrap();
        assert_eq!(context_window_hint(&local), None);

        assert_eq!(
            context_window_hint(&endpoint("https://e.com")),
            None,
            "unknown model"
        );
        let known = ProviderKind::Endpoint {
            base_url: "https://openrouter.ai/api/v1".into(),
            model: "openai/gpt-4o-mini".into(),
            api_key: None,
        };
        assert_eq!(
            context_window_hint(&known),
            Some(128000),
            "bundled context metadata is grounded for catalogued remote models"
        );
    }

    #[test]
    fn release_without_a_server_is_a_no_op() {
        let mut local = backend(ProviderKind::Local {
            model_dir: "/models/x".into(),
        });
        local.release();
        local.release();
    }
    fn anthropic_sse(events: &[serde_json::Value]) -> String {
        events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect()
    }

    fn thinking_tool_events() -> Vec<serde_json::Value> {
        vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":0,"cache_read_input_tokens":11,"cache_creation_input_tokens":13}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"consider "}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"the file"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed-block"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Reading "}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"now."}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call-a","name":"read","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"a.rs\"}"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":19}}),
            json!({"type":"message_stop"}),
        ]
    }

    #[test]
    fn native_anthropic_streams_text_thinking_tools_usage_and_preserves_signed_replay() {
        let server = fake::serve(vec![Script::Sse(anthropic_sse(&thinking_tool_events()))]);
        let mut remote = backend(anthropic_endpoint(&server.url));
        let mut text = Vec::new();
        let mut events = Vec::new();
        let reply = remote
            .chat_with_control_events(
                None,
                &request(),
                crate::llm::Limits {
                    timeout_secs: 5,
                    interrupt: Arc::new(AtomicBool::new(false)),
                },
                Some(&mut |piece| {
                    text.push(piece.to_owned());
                    true
                }),
                &mut |_| {},
                &mut |event| {
                    events.push(event.clone());
                    true
                },
            )
            .unwrap();
        assert_eq!(text, ["Reading ", "now."]);
        assert_eq!(reply.text, "Reading now.");
        assert_eq!(reply.reasoning, "consider the file");
        assert_eq!(reply.tool_calls[0].arguments, json!({"path":"a.rs"}));
        assert_eq!(reply.finish_reason, "tool_calls");
        assert_eq!(
            (reply.prompt_tokens, reply.completion_tokens),
            (Some(31), Some(19))
        );
        assert_eq!(
            (reply.cached_prompt_tokens, reply.cache_creation_tokens),
            (Some(11), Some(13))
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, llm::StreamEvent::Reasoning(_)))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, llm::StreamEvent::ToolArguments { .. }))
                .count(),
            2
        );
        let body = fake::body(&server.requests()[0]);
        assert_eq!(body["stream"], true);
        let mut req = request();
        req.messages = vec![
            llm::assistant_message(&reply),
            llm::tool_message("call-a", "contents"),
        ];
        let replay = anthropic_request("claude-test", &req).unwrap();
        assert_eq!(
            replay["messages"][0]["content"][0]["signature"],
            "signed-block"
        );
        assert_eq!(replay["messages"][1]["content"][0]["tool_use_id"], "call-a");
    }

    #[test]
    fn incomplete_anthropic_stream_retries_without_returning_partial_calls() {
        let events = thinking_tool_events();
        let mut missing_reason = events.clone();
        missing_reason.remove(missing_reason.len() - 2);
        let mut open_call = events[..12].to_vec();
        open_call.push(json!({"type":"message_stop"}));
        for incomplete in [
            events[..events.len() - 1].to_vec(),
            missing_reason,
            open_call,
        ] {
            let server = fake::serve(vec![
                Script::Sse(anthropic_sse(&incomplete)),
                Script::Sse(anthropic_sse(&events)),
            ]);
            let mut remote = backend(anthropic_endpoint(&server.url));
            let mut notices = Vec::new();
            let reply = remote
                .chat_with_control(
                    None,
                    &request(),
                    5,
                    Some(&mut |_| true),
                    &mut |n| notices.push(n.to_owned()),
                    Arc::new(AtomicBool::new(false)),
                )
                .unwrap();
            let requests = server.requests();
            assert_eq!(requests.len(), 2);
            assert_eq!(reply.tool_calls.len(), 1);
            assert_eq!(reply.tool_calls[0].arguments, json!({"path":"a.rs"}));
            assert!(
                notices
                    .iter()
                    .any(|n| n.contains("discarding the partial reply"))
            );
            assert_eq!(fake::body(&requests[0]), fake::body(&requests[1]));
        }
    }

    #[test]
    fn incomplete_anthropic_stream_stops_at_existing_retry_bound() {
        let events = thinking_tool_events();
        let server = fake::serve(
            (0..10)
                .map(|_| Script::Sse(anthropic_sse(&events[..events.len() - 1])))
                .collect(),
        );
        let mut remote = backend(anthropic_endpoint(&server.url));
        let error = remote
            .chat(None, &request(), 5, Some(&mut |_| true))
            .unwrap_err();
        assert!(error.to_string().contains("after 10 attempts"), "{error:#}");
        assert_eq!(server.requests().len(), 10);
    }

    #[test]
    fn native_anthropic_stalls_and_ping_only_streams_retry() {
        Patience::with_test_idle_limit(Duration::from_millis(200));
        let events = thinking_tool_events();
        let ping = anthropic_sse(&[json!({"type":"ping"})]);
        for first in [None, Some(anthropic_sse(&events[..1]))] {
            let mut pieces = vec![first.unwrap_or_default()];
            pieces.extend(std::iter::repeat_n(ping.clone(), 100));
            let server = fake::serve(vec![
                Script::SseSlow {
                    pieces,
                    pause: Duration::from_millis(10),
                },
                Script::Sse(anthropic_sse(&events)),
            ]);
            let mut remote = backend(anthropic_endpoint(&server.url));
            let mut notices = Vec::new();
            let reply = remote
                .chat_with_control(
                    None,
                    &request(),
                    1,
                    Some(&mut |_| true),
                    &mut |n| notices.push(n.to_owned()),
                    Arc::new(AtomicBool::new(false)),
                )
                .unwrap();
            assert_eq!(reply.tool_calls.len(), 1);
            assert!(
                notices.iter().any(|n| n.contains("idle limit")),
                "{notices:?}"
            );
            assert_eq!(server.requests().len(), 2);
        }
        for script in [
            Script::Stall {
                silence: Duration::from_millis(320),
            },
            Script::StallAfter {
                first: anthropic_sse(&events[..12]),
                silence: Duration::from_millis(320),
            },
        ] {
            let server = fake::serve(vec![script, Script::Sse(anthropic_sse(&events))]);
            let mut remote = backend(anthropic_endpoint(&server.url));
            let reply = remote
                .chat(None, &request(), 1, Some(&mut |_| true))
                .unwrap();
            assert_eq!(reply.tool_calls.len(), 1);
            assert_eq!(reply.tool_calls[0].arguments, json!({"path":"a.rs"}));
            assert_eq!(server.requests().len(), 2);
        }
    }

    #[test]
    fn native_anthropic_noop_events_cannot_keep_a_failed_tool_round_waiting() {
        Patience::with_test_idle_limit(Duration::from_millis(200));
        let events = thinking_tool_events();
        // Stall after an open tool call, with valid but output-free events arriving.
        // The failed browser result is already in the request for this model round.
        for (prefix, noise) in [
            (
                12,
                json!({"type":"content_block_delta","index":2,
                "delta":{"type":"input_json_delta","partial_json":""}}),
            ),
            (
                8,
                json!({"type":"content_block_delta","index":1,
                "delta":{"type":"text_delta","text":""}}),
            ),
            (
                4,
                json!({"type":"content_block_delta","index":0,
                "delta":{"type":"thinking_delta","thinking":""}}),
            ),
            (
                4,
                json!({"type":"content_block_delta","index":0,
                "delta":{"type":"signature_delta","signature":""}}),
            ),
            (
                12,
                json!({"type":"message_delta","delta":{},"usage":{"output_tokens":0}}),
            ),
        ] {
            let mut pieces = vec![anthropic_sse(&events[..prefix])];
            pieces.extend(std::iter::repeat_n(anthropic_sse(&[noise]), 20));
            let server = fake::serve(vec![
                Script::SseSlow {
                    pieces,
                    pause: Duration::from_millis(10),
                },
                Script::Sse(anthropic_sse(&events)),
            ]);
            let mut remote = backend(anthropic_endpoint(&server.url));
            let mut req = request();
            req.messages
                .push(json!({"role":"assistant","content":null,"tool_calls":[
                    {"id":"browser-failed","type":"function","function":{
                        "name":"browser","arguments":"{}"}}
                ]}));
            req.messages.push(llm::tool_message(
                "browser-failed",
                "Connection refused (os error 61)",
            ));
            let mut notices = Vec::new();
            let reply = remote
                .chat_with_control(
                    None,
                    &req,
                    1,
                    Some(&mut |_| true),
                    &mut |n| notices.push(n.to_owned()),
                    Arc::new(AtomicBool::new(false)),
                )
                .unwrap();
            assert!(
                notices.iter().any(|n| n.contains("idle limit")),
                "{notices:?}"
            );
            assert_eq!(server.requests().len(), 2);
            assert_eq!(reply.tool_calls.len(), 1);
            assert_eq!(reply.tool_calls[0].arguments, json!({"path":"a.rs"}));
        }
    }

    #[test]
    fn native_anthropic_meaningful_deltas_extend_idle_deadline() {
        Patience::with_test_idle_limit(Duration::from_millis(200));
        let events = thinking_tool_events();
        let mut pieces = vec![anthropic_sse(&events[..12])];
        pieces.extend(std::iter::repeat_n(
            anthropic_sse(&[json!({
                "type":"content_block_delta","index":2,
                "delta":{"type":"input_json_delta","partial_json":" "}
            })]),
            20,
        ));
        pieces.push(anthropic_sse(&events[12..]));
        let server = fake::serve(vec![Script::SseSlow {
            pieces,
            pause: Duration::from_millis(50),
        }]);
        let mut remote = backend(anthropic_endpoint(&server.url));
        let mut notices = Vec::new();
        let reply = remote
            .chat_with_control(
                None,
                &request(),
                1,
                Some(&mut |_| true),
                &mut |n| notices.push(n.to_owned()),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert!(
            !notices.iter().any(|n| n.contains("idle limit")),
            "{notices:?}"
        );
        assert_eq!(server.requests().len(), 1);
        assert_eq!(reply.tool_calls[0].arguments, json!({"path":"a.rs"}));
    }

    #[test]
    fn native_anthropic_transient_stream_errors_retry_and_remain_bounded() {
        let events = thinking_tool_events();
        for kind in ["overloaded_error", "api_error", "rate_limit_error"] {
            let mut partial = events[..12].to_vec();
            partial.push(json!({"type":"error","error":{
                "type":kind,"message":"anth-secret SECRET-PROMPT"
            }}));
            let server = fake::serve(vec![
                Script::Sse(anthropic_sse(&partial)),
                Script::Sse(anthropic_sse(&events)),
            ]);
            let mut remote = backend(anthropic_endpoint(&server.url));
            let mut notices = Vec::new();
            let reply = remote
                .chat_with_control(
                    None,
                    &request(),
                    5,
                    Some(&mut |_| true),
                    &mut |n| notices.push(n.to_owned()),
                    Arc::new(AtomicBool::new(false)),
                )
                .unwrap();
            assert_eq!(reply.tool_calls.len(), 1);
            assert!(
                notices
                    .iter()
                    .any(|n| n.contains("discarding the partial reply"))
            );
            assert!(!notices.join(" ").contains("anth-secret"));
            assert!(!notices.join(" ").contains("SECRET-PROMPT"));
            let requests = server.requests();
            assert_eq!(requests.len(), 2);
            assert_eq!(fake::body(&requests[0]), fake::body(&requests[1]));
        }
        let error_event = anthropic_sse(&[json!({"type":"error","error":{
            "type":"overloaded_error","message":"anth-secret SECRET-PROMPT"
        }})]);
        let server = fake::serve((0..10).map(|_| Script::Sse(error_event.clone())).collect());
        let mut remote = backend(anthropic_endpoint(&server.url));
        let error = remote
            .chat(None, &request(), 5, Some(&mut |_| true))
            .unwrap_err();
        assert!(error.to_string().contains("after 10 attempts"));
        assert!(!error.to_string().contains("anth-secret"));
        assert!(!error.to_string().contains("SECRET-PROMPT"));
        assert_eq!(server.requests().len(), 10);
    }

    #[test]
    fn native_anthropic_post_response_failures_never_replay_and_cancel_is_typed() {
        let events = thinking_tool_events();
        for script in [
            Script::Sse("data: not-json\n\n".into()),
            Script::Sse("data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"anth-secret SECRET-PROMPT\"}}\n\n".into()),
        ] {
            let server = fake::serve(vec![script]);
            let mut remote = backend(anthropic_endpoint(&server.url));
            let error = remote.chat(None, &request(), 5, Some(&mut |_| true)).unwrap_err();
            assert!(!llm::is_transient(&error));
            assert!(!error.to_string().contains("anth-secret") && !error.to_string().contains("SECRET-PROMPT"));
            assert_eq!(server.requests().len(), 1);
        }
        let server = fake::serve(vec![Script::Sse(anthropic_sse(&events))]);
        let mut remote = backend(anthropic_endpoint(&server.url));
        let error = remote
            .chat_with_control_events(
                None,
                &request(),
                crate::llm::Limits {
                    timeout_secs: 5,
                    interrupt: Arc::new(AtomicBool::new(false)),
                },
                Some(&mut |_| true),
                &mut |_| {},
                &mut |_| false,
            )
            .unwrap_err();
        assert!(error.downcast_ref::<llm::Interrupted>().is_some());
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn request_profile_is_one_shot_and_error_restores_baseline() {
        use llm::profiles::*;
        let baseline = ModelProfile {
            provider: "endpoint".into(),
            model: "test".into(),
            capabilities: ModelCapabilities {
                temperature: true,
                top_p: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut adaptive = baseline.clone();
        adaptive.temperature = Some(0.1);
        adaptive.top_p = Some(0.9);
        let server = fake::serve(vec![
            fake::json(400, &json!({"error":"invalid request"})),
            ok("second"),
        ]);
        let mut remote = backend(endpoint(&server.url)).with_profile(baseline);
        remote.next_request_profile(Some(adaptive));
        let req = request();
        assert!(remote.chat(None, &req, 5, None).is_err());
        remote.chat(None, &req, 5, None).unwrap();
        let sent = server.requests();
        assert_eq!(fake::body(&sent[0])["temperature"], 0.1);
        assert_eq!(fake::body(&sent[0])["top_p"], 0.9);
        assert_eq!(fake::body(&sent[1])["temperature"], req.temperature);
        assert!(fake::body(&sent[1]).get("top_p").is_none());
        assert!(remote.model_profile().unwrap().temperature.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn thinking_controls_reach_only_the_owned_mlx_request_and_expire_after_use() {
        let root = tempfile::tempdir().unwrap();
        let model = root.path().join("text-model");
        std::fs::create_dir(&model).unwrap();
        let mut local = Backend::new(
            ProviderKind::Local {
                model_dir: model.to_string_lossy().into_owned(),
            },
            server::tests::fake_python(root.path()),
            30,
        );
        let guard = crate::config::types::ReasoningGuardConfig {
            enabled: false,
            max_reasoning_bytes: 4096,
            ..Default::default()
        };
        local.next_reasoning_guard(&guard);
        local
            .chat(None, &request(), 5, Some(&mut |_| true))
            .unwrap();
        local.chat(None, &request(), 5, None).unwrap();
        let info: serde_json::Value =
            serde_json::from_slice(&std::fs::read(model.join("info.json")).unwrap()).unwrap();
        let requests = info["requests"].as_array().unwrap();
        // The first request is the supervisor's one-token warmup, outside the agent gate.
        assert!(requests[0].get("dial_reasoning").is_none());
        assert_eq!(
            requests[1]["dial_reasoning"],
            json!({"max_bytes":3072,"max_seconds":0.0})
        );
        assert!(requests[2].get("dial_reasoning").is_none());
        local.release();

        let server = fake::serve(vec![ok("remote")]);
        let mut remote = backend(endpoint(&server.url));
        remote.next_reasoning_guard(&guard);
        remote.chat(None, &request(), 5, None).unwrap();
        assert!(
            fake::body(&server.requests()[0])
                .get("dial_reasoning")
                .is_none()
        );
    }

    #[test]
    fn explicit_openai_reasoning_profile_and_cached_usage_reach_wire_and_reply() {
        use llm::profiles::*;
        let profile = ModelProfile {
            provider: "endpoint".into(),
            model: "gpt-x".into(),
            capabilities: ModelCapabilities {
                reasoning: ReasoningApi::OpenaiEffort,
                reasoning_efforts: vec![ReasoningEffort::High],
                output_limit: OutputLimit::MaxCompletionTokens,
                ..Default::default()
            },
            reasoning_effort: Some(ReasoningEffort::High),
            max_tokens: Some(4096),
            ..Default::default()
        };
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"reasoning_content":"reason"},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":80},"completion_tokens_details":{"reasoning_tokens":17}}})
        );
        let server = fake::serve(vec![Script::Sse(body)]);
        let mut remote = backend(endpoint(&server.url)).with_profile(profile);
        let mut req = request();
        req.max_tokens = 4096;
        let reply = remote.chat(None, &req, 5, Some(&mut |_| true)).unwrap();
        assert_eq!(reply.reasoning, "reason");
        assert_eq!(reply.reasoning_tokens, Some(17));
        assert_eq!(reply.cached_prompt_tokens, Some(80));
        let wire = fake::body(&server.requests()[0]);
        assert_eq!(wire["reasoning_effort"], "high");
        assert_eq!(wire["max_completion_tokens"], 4096);
        assert!(wire.get("temperature").is_none() && wire.get("max_tokens").is_none());
    }

    #[test]
    fn native_anthropic_explicit_thinking_effort_and_cache_prefix_are_consumed() {
        use llm::profiles::*;
        let profile = ModelProfile {
            provider: "anthropic".into(),
            model: "claude-test".into(),
            capabilities: ModelCapabilities {
                reasoning: ReasoningApi::AnthropicBudget,
                reasoning_efforts: vec![ReasoningEffort::High],
                prompt_cache: true,
                ..Default::default()
            },
            thinking: Some(true),
            thinking_budget: Some(1024),
            reasoning_effort: Some(ReasoningEffort::High),
            max_tokens: Some(2048),
            prompt_cache: Some(true),
            ..Default::default()
        };
        let server = fake::serve(vec![fake::json(
            200,
            &json!({"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}),
        )]);
        let mut remote = backend(anthropic_endpoint(&server.url)).with_profile(profile);
        let mut req = request();
        req.messages
            .insert(0, json!({"role":"system","content":"stable prefix"}));
        req.max_tokens = 2048;
        remote.chat(None, &req, 5, None).unwrap();
        let wire = fake::body(&server.requests()[0]);
        assert_eq!(
            wire["thinking"],
            json!({"type":"enabled","budget_tokens":1024})
        );
        assert_eq!(wire["output_config"]["effort"], "high");
        assert_eq!(wire["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(wire["max_tokens"], 2048);
        assert!(wire.get("temperature").is_none());
    }

    #[test]
    fn native_anthropic_history_caching_and_transient_guidance_reach_the_wire() {
        use llm::profiles::*;
        for enabled in [None, Some(false)] {
            let response = || {
                fake::json(
                    200,
                    &json!({"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}),
                )
            };
            let server = fake::serve(vec![response(), response()]);
            let mut remote = backend(anthropic_endpoint(&server.url));
            if enabled.is_some() {
                remote = remote.with_profile(ModelProfile {
                    provider: "anthropic".into(),
                    model: "claude-test".into(),
                    prompt_cache: enabled,
                    ..Default::default()
                });
            }
            let mut req = request();
            req.messages
                .insert(0, json!({"role":"system","content":"stable"}));
            req.messages
                .push(json!({"role":"user","content":"contract one","dial_transient":true}));
            remote.chat(None, &req, 5, None).unwrap();
            req.messages.pop();
            req.messages
                .push(json!({"role":"assistant","content":"ok"}));
            req.messages
                .push(json!({"role":"user","content":"contract two","dial_transient":true}));
            remote.chat(None, &req, 5, None).unwrap();
            let seen = server.requests();
            let first = fake::body(&seen[0]);
            let next = fake::body(&seen[1]);
            assert_eq!(first["system"], next["system"]);
            assert_eq!(first["messages"][0], next["messages"][0]);
            assert_eq!(first["messages"][1]["content"], "contract one");
            assert_eq!(next["messages"][2]["content"], "contract two");
            assert!(next["messages"][2].get("dial_transient").is_none());
            if enabled == Some(false) {
                assert_eq!(next["messages"][0]["content"], "SECRET-PROMPT");
                assert_eq!(next["system"], "stable");
            } else {
                assert_eq!(
                    next["messages"][0]["content"][0]["cache_control"]["type"],
                    "ephemeral"
                );
                assert_eq!(
                    next["messages"][1]["content"][0]["cache_control"]["type"],
                    "ephemeral"
                );
            }
        }
    }

    #[test]
    fn profiled_provider_honors_increased_output_budget_on_next_request() {
        use llm::profiles::*;
        for anthropic in [false, true] {
            let response = |length: bool| {
                if anthropic {
                    fake::json(
                        200,
                        &json!({"content":[{"type":"text","text":"done"}],"stop_reason":if length { "max_tokens" } else { "end_turn" }}),
                    )
                } else {
                    fake::json(
                        200,
                        &json!({"choices":[{"finish_reason":if length { "length" } else { "stop" },"message":{"content":"done"}}]}),
                    )
                }
            };
            let server = fake::serve(vec![response(true), response(false)]);
            let profile = ModelProfile {
                provider: if anthropic { "anthropic" } else { "endpoint" }.into(),
                model: if anthropic { "claude-test" } else { "gpt-x" }.into(),
                max_tokens: Some(16),
                capabilities: ModelCapabilities {
                    output_limit: if anthropic {
                        OutputLimit::MaxTokens
                    } else {
                        OutputLimit::MaxCompletionTokens
                    },
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut remote = backend(if anthropic {
                anthropic_endpoint(&server.url)
            } else {
                endpoint(&server.url)
            })
            .with_profile(profile);
            let mut req = request();
            req.max_tokens = 16;
            assert_eq!(
                remote.chat(None, &req, 5, None).unwrap().finish_reason,
                "length"
            );
            req.max_tokens = 64;
            remote.chat(None, &req, 5, None).unwrap();
            let sent = server.requests();
            let field = if anthropic {
                "max_tokens"
            } else {
                "max_completion_tokens"
            };
            assert_eq!(fake::body(&sent[0])[field], 16);
            assert_eq!(fake::body(&sent[1])[field], 64);
        }
    }

    #[test]
    fn cache_cost_never_fabricates_missing_discount_prices() {
        let pricing = llm::catalog::TokenPricing {
            prompt: 1.0,
            completion: 2.0,
            cache_read: None,
            cache_write: None,
        };
        let reply = Reply {
            prompt_tokens: Some(100),
            completion_tokens: Some(5),
            cached_prompt_tokens: Some(80),
            ..Default::default()
        };
        assert_eq!(pricing.reply_cost(&reply), None);
        assert_eq!(
            llm::catalog::TokenPricing {
                cache_read: Some(0.1),
                ..pricing
            }
            .reply_cost(&reply),
            Some(38.0)
        );
    }

    #[test]
    fn native_anthropic_interrupt_abandons_one_live_stream() {
        let server = fake::serve(vec![Script::Drip {
            pause: Duration::from_millis(20),
            count: 100,
        }]);
        let mut remote = backend(anthropic_endpoint(&server.url));
        let interrupt = Arc::new(AtomicBool::new(false));
        let flag = interrupt.clone();
        let trigger = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            flag.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let error = remote
            .chat_with_control(
                None,
                &request(),
                3600,
                Some(&mut |_| true),
                &mut |_| {},
                interrupt,
            )
            .unwrap_err();
        trigger.join().unwrap();
        assert!(error.downcast_ref::<llm::Interrupted>().is_some());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(server.requests().len(), 1);
    }
}

#[cfg(test)]
mod real_server {
    //! Run with `cargo test --lib provider::real_server -- --ignored --nocapture`.
    use super::*;
    use serde_json::json;
    use std::time::Instant;

    fn python() -> PathBuf {
        std::env::var_os("DIAL_TEST_PYTHON")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("python/.venv/bin/python")
            })
    }

    fn snapshot(repo_dir: &str) -> Option<String> {
        let home = std::env::var("HOME").ok()?;
        let snapshots = Path::new(&home)
            .join(".cache/huggingface/hub")
            .join(repo_dir)
            .join("snapshots");
        let entry = std::fs::read_dir(snapshots).ok()?.next()?.ok()?;
        entry.path().to_str().map(str::to_string)
    }

    fn ask(content: &str, tools: Vec<serde_json::Value>, max_tokens: u32) -> Chat {
        Chat {
            messages: vec![json!({"role": "user", "content": content})],
            tools,
            temperature: 0.0,
            max_tokens,
            seed: Some(1),
        }
    }

    #[test]
    #[ignore = "loads a downloaded vision model; set DIAL_TEST_VISION_MODEL and DIAL_TEST_VISION_IMAGE (a red square)"]
    fn sees_images_and_streams_tools_from_the_real_vlm_server() {
        let model = std::env::var("DIAL_TEST_VISION_MODEL").expect("downloaded vision model path");
        let image =
            PathBuf::from(std::env::var("DIAL_TEST_VISION_IMAGE").expect("red-square PNG path"));
        let root = tempfile::tempdir().unwrap();
        let config = crate::config::types::Config {
            user_dir: root.path().join("user"),
            state_dir: root.path().join("state"),
            project_root: root.path().to_path_buf(),
            ..Default::default()
        };
        let mut backend = Backend::new(ProviderKind::Local { model_dir: model }, python(), 180)
            .with_steering(&config);
        assert!(backend.accepts_image_input());
        let mut req = ask(
            "Describe the shape and color in this image in one short sentence.",
            vec![],
            64,
        );
        req.messages[0]["dial_images"] =
            json!([llm::images::user_attachment(root.path(), &image).unwrap()]);
        let reply = backend.chat(None, &req, 90, None).unwrap();
        println!("VISION: {}", reply.text);
        assert!(
            reply.text.to_lowercase().contains("red")
                && reply.text.to_lowercase().contains("square")
        );
        req.messages[0]["content"] = json!(
            "Inspect the attached image and call describe_image with its color and shape. Use the tool instead of writing an answer."
        );
        req.tools = vec![json!({"type":"function","function":{
            "name":"describe_image", "description":"Record the color and shape seen in the image.",
            "parameters":{"type":"object","properties":{"color":{"type":"string"},"shape":{"type":"string"}},"required":["color","shape"]}
        }})];
        req.max_tokens = 256;
        let reply = backend.chat(None, &req, 90, Some(&mut |_| true)).unwrap();
        println!("VISION TOOL CALLS: {:?}", reply.tool_calls);
        let call = reply
            .tool_calls
            .iter()
            .find(|call| call.name == "describe_image")
            .expect("visual tool call");
        assert_eq!(call.arguments["color"], "red");
        assert_eq!(call.arguments["shape"], "square");
        backend.release();
    }

    #[test]
    #[ignore = "starts the real mlx_lm.server; needs the SmolLM2 snapshot"]
    fn streams_text_from_the_real_server() {
        let dir = snapshot("models--mlx-community--SmolLM2-135M-Instruct-8bit")
            .expect("SmolLM2 snapshot");
        let mut backend = Backend::new(ProviderKind::Local { model_dir: dir }, python(), 180);
        let request = ask("Say hello in five words.", vec![], 24);

        let plain = backend.chat(None, &request, 120, None).unwrap();
        let started = Instant::now();
        let mut first = None;
        let mut pieces = Vec::new();
        let mut on_text = |piece: &str| {
            first.get_or_insert_with(|| started.elapsed());
            pieces.push(piece.to_string());
            true
        };
        let streamed = backend
            .chat(None, &request, 120, Some(&mut on_text))
            .unwrap();
        println!(
            "STREAM: {} deltas, first after {:?}, total {:?}, usage {:?}/{:?}, finish {:?}, text {:?}",
            pieces.len(),
            first,
            started.elapsed(),
            streamed.prompt_tokens,
            streamed.completion_tokens,
            streamed.finish_reason,
            streamed.text
        );
        println!(
            "PLAIN:  usage {:?}/{:?}, text {:?}",
            plain.prompt_tokens, plain.completion_tokens, plain.text
        );
        assert!(!pieces.is_empty());
        assert_eq!(pieces.concat(), streamed.text);
        assert_eq!(
            streamed.text, plain.text,
            "temperature 0 + seed: stream and plain must agree"
        );
        assert!(streamed.prompt_tokens.is_some() && streamed.completion_tokens.is_some());

        // Abandoning mid-stream must leave the server usable.
        let mut stop = |_: &str| false;
        let error = backend
            .chat(
                None,
                &ask("Count to fifty.", vec![], 200),
                120,
                Some(&mut stop),
            )
            .unwrap_err();
        assert!(
            error.downcast_ref::<llm::Interrupted>().is_some(),
            "{error:#}"
        );
        let again = backend.chat(None, &request, 120, None).unwrap();
        assert_eq!(again.text, plain.text);
        backend.release();
    }

    #[test]
    #[ignore = "starts the real mlx_lm.server; needs the Qwen2.5-0.5B snapshot"]
    fn streams_tool_calls_from_the_real_server() {
        let Some(dir) = snapshot("models--mlx-community--Qwen2.5-0.5B-Instruct-4bit") else {
            println!("Qwen2.5-0.5B snapshot not present; skipping");
            return;
        };
        let mut backend = Backend::new(ProviderKind::Local { model_dir: dir }, python(), 180);
        let tool = json!({"type": "function", "function": {
            "name": "read_file",
            "description": "Read a text file from the workspace",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}
        }});
        let request = ask("Please read the file notes.txt", vec![tool], 128);
        let mut pieces = Vec::new();
        let mut on_text = |piece: &str| {
            pieces.push(piece.to_string());
            true
        };
        let started = Instant::now();
        let streamed = backend
            .chat(None, &request, 120, Some(&mut on_text))
            .unwrap();
        println!(
            "TOOL STREAM ({:?}): text deltas {pieces:?}\n  reply {streamed:?}",
            started.elapsed()
        );
        let plain = backend.chat(None, &request, 120, None).unwrap();
        println!("TOOL PLAIN: {plain:?}");
        assert_eq!(streamed.tool_calls.len(), plain.tool_calls.len());
        assert!(
            !streamed.tool_calls.is_empty(),
            "no tool call: {streamed:?}"
        );
        assert_eq!(streamed.tool_calls[0].name, "read_file");
        assert!(!streamed.tool_calls[0].malformed_arguments);
        backend.release();
    }
}

#[cfg(test)]
mod chatgpt_tests {
    use super::*;
    use crate::llm::cancel::Patience;
    use serde_json::json;
    #[test]
    fn remote_silence_limit_preserves_local_worker_allowances() {
        let make = |kind| Backend::new(kind, PathBuf::new(), 600).with_remote_idle_timeout(90);
        for kind in [
            ProviderKind::ChatGpt {
                model: "test".into(),
                user_dir: PathBuf::new(),
            },
            ProviderKind::Endpoint {
                base_url: "https://example.com/v1".into(),
                model: "test".into(),
                api_key: None,
            },
            ProviderKind::AnthropicEndpoint {
                base_url: "https://example.com/v1".into(),
                model: "test".into(),
                api_key: None,
            },
        ] {
            let backend = make(kind);
            assert_eq!(backend.response_timeout(3600), 90);
            assert_eq!(backend.response_timeout(30), 30);
        }
        for kind in [
            ProviderKind::Local {
                model_dir: "local-model".into(),
            },
            ProviderKind::Endpoint {
                base_url: "http://127.0.0.1:1234/v1".into(),
                model: "test".into(),
                api_key: None,
            },
        ] {
            assert_eq!(make(kind).response_timeout(3600), 3600);
        }
    }

    #[test]
    fn remote_default_bounds_unlimited_workers_while_local_servers_stay_unlimited() {
        let limit = crate::config::types::DEFAULT_REMOTE_IDLE_TIMEOUT_SECS;
        let remote = Backend::new(
            ProviderKind::ChatGpt {
                model: "test".into(),
                user_dir: PathBuf::new(),
            },
            PathBuf::new(),
            600,
        );
        assert_eq!(remote.response_timeout(0), limit);
        let local = Backend::new(
            ProviderKind::Local {
                model_dir: "local-model".into(),
            },
            PathBuf::new(),
            600,
        );
        assert_eq!(local.response_timeout(0), 0);
    }

    #[test]
    fn subscription_stall_is_bounded_retried_and_never_executes_partial_calls() {
        Patience::with_test_idle_limit(Duration::from_millis(200));
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let layout =
            crate::storage::paths::Layout::with_user_dir(&project, temp.path().join("home"))
                .unwrap();
        llm::chatgpt::test_account(&layout.user_dir, false);
        // This exercises the SSE stall path; the WebSocket handshake would consume a script.
        let overlay: toml::Table = toml::from_str("model='account-model'\nprovider='chatgpt'\napproval='auto'\nintegrations=[]\nremote_model_idle_timeout_secs=1\ncodex_websocket=false\n[retry]\nmax_attempts=2\n[sandbox]\nmode='off'\n[improve]\nenabled=false").unwrap();
        let config = crate::config::types::Config::load(&layout, Some(overlay)).unwrap();
        let item = json!({"type":"function_call","namespace":"dial","name":"write","id":"item1","call_id":"call1","status":"completed","arguments":"{\"path\":\"marker.txt\",\"content\":\"must not run\"}"});
        let first = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type":"response.output_item.added","output_index":0,"item":item}),
            json!({"type":"response.output_item.done","output_index":0,"item":item})
        );
        let stall = || llm::fake::Script::StallAfter {
            first: first.clone(),
            silence: Duration::from_millis(320),
        };
        let server = llm::fake::serve(vec![stall(), stall()]);
        let mut backend = crate::agent::make_backend(&config).unwrap();
        backend.chatgpt_url = Some(format!("{}/codex/responses", server.url));
        backend.retry_backoff = Duration::from_millis(1);
        let session = crate::storage::session::Session::create(
            &layout,
            crate::storage::session::SessionMeta {
                model: config.model.clone(),
            },
        )
        .unwrap();
        let mut agent = crate::agent::Agent::interactive(
            &config,
            backend,
            layout,
            session,
            None,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        struct Ui(usize);
        impl crate::agent::Ui for Ui {
            fn heartbeat(&mut self) {
                self.0 += 1;
            }
        }
        let mut ui = Ui(0);
        let started = Instant::now();
        let error = agent
            .turn("Create marker.txt", &mut ui)
            .err()
            .expect("stalled request must fail");
        // A stalled Responses stream made no semantic progress: it is retried as an incomplete
        // stream, within the configured attempts, and never releases the partial tool call.
        assert!(
            format!("{error:#}").contains("no semantic progress"),
            "{error:#}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "each attempt must be bounded by the remote idle limit, not the worker allowance"
        );
        assert!(
            ui.0 > 0,
            "the UI must keep pumping while the provider stalls"
        );
        assert!(!project.join("marker.txt").exists());
        assert_eq!(
            server.requests().len(),
            2,
            "retries stop at retry.max_attempts"
        );
    }

    #[test]
    fn codex_server_without_websocket_costs_one_handshake_per_session() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let layout =
            crate::storage::paths::Layout::with_user_dir(&project, temp.path().join("home"))
                .unwrap();
        llm::chatgpt::test_account(&layout.user_dir, false);
        let overlay: toml::Table = toml::from_str("model='account-model'\nprovider='chatgpt'\napproval='auto'\nintegrations=[]\n[sandbox]\nmode='off'\n[improve]\nenabled=false").unwrap();
        let config = crate::config::types::Config::load(&layout, Some(overlay)).unwrap();
        let text = |text: &str| {
            let out = json!({"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]});
            llm::fake::Script::Sse(format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"type":"response.output_text.delta","delta":text}),
                json!({"type":"response.completed","response":{"status":"completed","output":[out],"usage":{"input_tokens":10,"output_tokens":2}}})
            ))
        };
        // The first script answers the WebSocket upgrade with plain HTTP.
        let server = llm::fake::serve(vec![text("unused"), text("one"), text("two")]);
        let mut backend = Backend::new(
            ProviderKind::ChatGpt {
                model: config.model.clone(),
                user_dir: layout.user_dir.clone(),
            },
            config.python.clone(),
            5,
        )
        .with_codex_websocket(true);
        backend.chatgpt_url = Some(format!("{}/codex/responses", server.url));
        let session = crate::storage::session::Session::create(
            &layout,
            crate::storage::session::SessionMeta {
                model: config.model.clone(),
            },
        )
        .unwrap();
        let backend = backend.with_session_id(session.id().to_owned());
        let mut agent = crate::agent::Agent::interactive(
            &config,
            backend,
            layout,
            session,
            None,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        struct Ui;
        impl crate::agent::Ui for Ui {}
        assert_eq!(agent.turn("first", &mut Ui).unwrap().text, "one");
        assert_eq!(agent.turn("second", &mut Ui).unwrap().text, "two");
        let requests = server.requests();
        assert_eq!(
            requests.len(),
            3,
            "the second request must go straight to SSE"
        );
        assert!(String::from_utf8_lossy(&requests[0]).starts_with("GET "));
        assert!(String::from_utf8_lossy(&requests[2]).starts_with("POST "));
    }

    #[test]
    fn subscription_provider_recovers_disconnection_runs_real_tools_and_replays_results() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let layout =
            crate::storage::paths::Layout::with_user_dir(&project, temp.path().join("home"))
                .unwrap();
        llm::chatgpt::test_account(&layout.user_dir, false);
        let overlay:toml::Table=toml::from_str("model='account-model'\nprovider='chatgpt'\napproval='auto'\nintegrations=[]\n[sandbox]\nmode='off'\n[improve]\nenabled=false").unwrap();
        let config = crate::config::types::Config::load(&layout, Some(overlay)).unwrap();
        assert_eq!(
            config.endpoint.as_ref().unwrap().base_url,
            llm::chatgpt::BASE_URL
        );
        let output = json!({"type":"function_call","namespace":"dial","name":"write","id":"item1","call_id":"call1","status":"completed","arguments":"{\"path\":\"marker.txt\",\"content\":\"real output\"}"});
        let final_output = json!({"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Marker written.","annotations":[]}]});
        let sse = |out: serde_json::Value, text: &str| {
            format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"type":"response.output_text.delta","delta":text}),
                json!({"type":"response.completed","response":{"status":"completed","output":[out],"usage":{"input_tokens":100,"output_tokens":12}}})
            )
        };
        let server = llm::fake::serve(vec![
            llm::fake::Script::SseDisconnect(format!(
                "data: {}\n\ndata: {}\n\ndata: {}\n\n",
                json!({"type":"response.output_text.delta","delta":"partial answer"}),
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","namespace":"dial","name":"write","id":"partial-item","call_id":"partial-call","status":"completed","arguments":"{\"path\":\"must-not-exist.txt\",\"content\":\"partial\"}"}}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","namespace":"dial","name":"write","id":"partial-item","call_id":"partial-call","status":"completed","arguments":"{\"path\":\"must-not-exist.txt\",\"content\":\"partial\"}"}}),
            )),
            llm::fake::Script::Sse(sse(output.clone(), "")),
            // The edit's completion review is attached to this request; no extra round.
            llm::fake::Script::Sse(sse(final_output, "Marker written.")),
        ]);
        let mut backend = Backend::new(
            ProviderKind::ChatGpt {
                model: config.model.clone(),
                user_dir: layout.user_dir.clone(),
            },
            config.python.clone(),
            5,
        );
        backend.chatgpt_url = Some(format!("{}/codex/responses", server.url));
        backend.retry_backoff = Duration::from_millis(1);
        let session = crate::storage::session::Session::create(
            &layout,
            crate::storage::session::SessionMeta {
                model: config.model.clone(),
            },
        )
        .unwrap();
        let mut agent = crate::agent::Agent::interactive(
            &config,
            backend,
            layout.clone(),
            session,
            None,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        struct Ui;
        impl crate::agent::Ui for Ui {}
        let report = agent.turn("Create marker.txt", &mut Ui).unwrap();
        assert_eq!(report.outcome, crate::agent::TurnOutcome::Completed);
        assert_eq!(report.text, "Marker written.");
        assert!(
            !project.join("must-not-exist.txt").exists(),
            "a completed tool item without response.completed must not execute"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("marker.txt")).unwrap(),
            "real output"
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(llm::fake::body(&requests[0]), llm::fake::body(&requests[1]));
        let expected_auth = format!("Bearer {}", llm::chatgpt::test_access("oaiapp_test"));
        assert_eq!(
            llm::fake::header(&requests[0], "authorization").as_deref(),
            Some(expected_auth.as_str())
        );
        assert_eq!(
            llm::fake::header(&requests[0], "chatgpt-account-id").as_deref(),
            Some("oaiapp_test")
        );
        assert_eq!(
            llm::fake::header(&requests[0], "originator").as_deref(),
            Some("dial")
        );
        assert_eq!(
            llm::fake::header(&requests[0], "version").as_deref(),
            Some("0.159.0")
        );
        assert_eq!(
            llm::fake::request_line(&requests[0]),
            "POST /codex/responses HTTP/1.1"
        );
        let second = llm::fake::body(&requests[2]);
        let input = second["input"].as_array().unwrap();
        assert!(input.iter().any(|item| item["type"] == "function_call"
            && item["name"] == "write"
            && item.get("namespace").is_none()));
        assert!(
            input
                .iter()
                .any(|item| item["type"] == "function_call_output" && item["call_id"] == "call1")
        );
        assert!(
            second["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "finish")
        );
    }
}
