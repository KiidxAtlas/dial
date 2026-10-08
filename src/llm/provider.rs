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

/// A grounded local context window, or `None` for endpoint models.
/// Remote context limits come from discovery/config, never model-name prefix guesses.
pub fn context_window_hint(kind: &ProviderKind) -> Option<u32> {
    match kind {
        ProviderKind::ChatGpt { .. } | ProviderKind::AnthropicSubscription { .. } => None,
        ProviderKind::Local { model_dir } => local_context_window(Path::new(model_dir)),
        ProviderKind::Endpoint { .. } | ProviderKind::AnthropicEndpoint { .. } => None,
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
    steering_config: Option<crate::config::types::Config>,
    steering_runtime: serde_json::Value,
    usage_scope: Option<llm::usage::Scope>,
}

impl Backend {
    pub fn new(kind: ProviderKind, python: PathBuf, startup_timeout_secs: u64) -> Self {
        Backend {
            #[cfg(test)]
            chatgpt_url: None,
            kind,
            python,
            startup_timeout_secs,
            allow_insecure: false,
            retry_backoff: RETRY_BACKOFF,
            remote_idle_timeout_secs: 180,
            server: None,
            adapter_monitor: Default::default(),
            metadata: Default::default(),
            profile: None,
            next_profile: None,
            steering_config: None,
            steering_runtime: serde_json::json!({"state":"disabled","uses":0}),
            usage_scope: None,
        }
    }

    pub fn set_usage_scope(
        &mut self,
        scope: Option<llm::usage::Scope>,
    ) -> Option<llm::usage::Scope> {
        std::mem::replace(&mut self.usage_scope, scope)
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
            worker_timeout_secs.min(self.remote_idle_timeout_secs)
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
    pub fn model_profile(&self) -> Option<&llm::profiles::ModelProfile> {
        self.profile.as_ref()
    }
    /// Consumed once at the request boundary; transport retries keep the same profile.
    pub fn next_request_profile(&mut self, profile: Option<llm::profiles::ModelProfile>) {
        self.next_profile = profile;
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
        if !remote {
            return None;
        }
        self.metadata.pricing?.reply_cost(reply)
    }

    /// Human-readable summary; never contains the API key.
    pub fn describe(&self) -> String {
        match &self.kind {
            ProviderKind::ChatGpt { model, .. } => format!("{model} via ChatGPT subscription"),
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

    /// At most five attempts, with at most 60 seconds spent in backoff.
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
            timeout_secs,
            on_text,
            on_notice,
            interrupt,
            &mut |_| true,
        )
    }

    /// Local vision requires a vision encoder; unknown external endpoints may attempt images.
    pub fn accepts_image_input(&self) -> bool {
        self.image_input_for(self.profile.as_ref())
    }

    fn image_input_for(&self, profile: Option<&llm::profiles::ModelProfile>) -> bool {
        let supported = profile
            .and_then(|p| p.capabilities.vision)
            .or(self.metadata.vision)
            .unwrap_or(true);
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
        timeout_secs: u64,
        on_text: Option<&mut dyn FnMut(&str) -> bool>,
        on_notice: &mut dyn FnMut(&str),
        interrupt: Arc<AtomicBool>,
        on_event: &mut dyn FnMut(&llm::StreamEvent) -> bool,
    ) -> Result<Reply> {
        let scope = self.usage_scope.clone();
        if let Some(scope) = &scope {
            scope.begin()?;
        }
        let result = self.chat_with_control_events_inner(
            adapter,
            req,
            timeout_secs,
            on_text,
            on_notice,
            interrupt,
            on_event,
        );
        if let Some(scope) = scope {
            scope.finish(result.as_ref().ok());
        }
        result
    }

    fn chat_with_control_events_inner(
        &mut self,
        adapter: Option<&Path>,
        req: &Chat,
        timeout_secs: u64,
        mut on_text: Option<&mut dyn FnMut(&str) -> bool>,
        on_notice: &mut dyn FnMut(&str),
        interrupt: Arc<AtomicBool>,
        on_event: &mut dyn FnMut(&llm::StreamEvent) -> bool,
    ) -> Result<Reply> {
        let profile = self.next_profile.take().or_else(|| self.profile.clone());
        let interrupted = || interrupt.load(Ordering::SeqCst);
        let timeout_secs = self.response_timeout(timeout_secs);
        let vision = self.image_input_for(profile.as_ref());
        let prepared = llm::images::prepare(req, vision, on_notice);
        let req = prepared.as_ref().unwrap_or(req);
        let chatgpt_access = match &self.kind {
            ProviderKind::ChatGpt { user_dir, .. } => Some(llm::chatgpt::account_access(
                user_dir,
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
                let phase =
                    if self.server.as_ref().is_some_and(|server| {
                        server.ready_for(model_dir, adapter, steering.as_ref())
                    }) {
                        llm::InferencePhase::Processing
                    } else {
                        llm::InferencePhase::Loading
                    };
                if !on_event(&llm::StreamEvent::Progress(llm::ModelProgress::new(phase))) {
                    return Err(llm::Interrupted.into());
                }
                let server = match server::ensure_profile_tracked(
                    &mut self.server,
                    server::ServerProfile {
                        python: &self.python,
                        model: model_dir,
                        adapter,
                        steering: steering.as_ref(),
                        context: self.steering_config.as_ref(),
                    },
                    self.startup_timeout_secs,
                    Some(Arc::clone(&interrupt)),
                ) {
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
                        url: &llm::completions_url(server.base_url()),
                        auth: None,
                        model: server.request_model(),
                    },
                    req,
                    timeout_secs,
                    &mut on_text,
                    Arc::clone(&interrupt),
                    profile.as_ref(),
                    on_event,
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
                    endpoint_url(base_url, self.allow_insecure)?,
                    api_key.as_deref(),
                    model,
                    0,
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
                    anthropic_url(base_url, self.allow_insecure)?,
                    api_key.as_deref(),
                    model,
                    1,
                    None,
                )
            }
        };
        let mut waited = Duration::ZERO;
        for attempt in 1..=llm::retry::MAX_ATTEMPTS {
            if interrupted() {
                return Err(llm::Interrupted.into());
            }
            if attempt > 1 {
                if let Some(scope) = &self.usage_scope {
                    scope.retry();
                }
            }
            let result = if protocol == 2 {
                llm::responses::exchange_codex(
                    llm::responses::Endpoint {
                        url: &url,
                        token: auth.unwrap(),
                        account_id: Some(codex_account_id.unwrap()),
                        model,
                    },
                    req,
                    timeout_secs,
                    &mut on_text,
                    Arc::clone(&interrupt),
                    profile.as_ref(),
                    on_event,
                )
            } else if protocol == 1 {
                anthropic_exchange(
                    llm::Endpoint {
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
            } else {
                exchange(
                    llm::Endpoint {
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
            };
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
            if let Some(status @ (401 | 403)) = error
                .downcast_ref::<llm::HttpError>()
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
            let incomplete = error
                .downcast_ref::<llm::anthropic::IncompleteStream>()
                .is_some();
            if !incomplete && !llm::is_transient(&error) {
                return Err(error);
            }
            if attempt == llm::retry::MAX_ATTEMPTS {
                return Err(error.context(format!(
                    "provider request failed after {attempt} attempts; the turn is paused. Say \"continue\" to retry"
                )));
            }
            let hinted = error
                .downcast_ref::<llm::HttpError>()
                .and_then(|http| http.retry_after);
            if incomplete {
                on_notice(
                    "Anthropic stream was incomplete; discarding the partial reply and retrying. No partial tool calls will execute.",
                );
            }
            let exponential = self
                .retry_backoff
                .saturating_mul(1 << (attempt - 1))
                .min(Duration::from_secs(8));
            let jitter = exponential.mul_f64(0.75 + rand::random::<f64>() * 0.5);
            let delay = hinted.map_or(jitter, |minimum| minimum.max(jitter));
            // Never retry earlier than a server's minimum; a hint beyond the budget ends the call.
            if delay > llm::retry::MAX_WAIT.saturating_sub(waited) {
                return Err(error.context("provider retry delay exceeds the 60s backoff budget"));
            }
            let reason = if error
                .downcast_ref::<llm::HttpError>()
                .is_some_and(|http| matches!(http.status, 429 | 529))
            {
                "rate limited"
            } else {
                "transient provider failure"
            };
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
                llm::retry::MAX_ATTEMPTS
            ));
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
            waited += delay;
        }
        unreachable!("bounded retries return on the final attempt")
    }
}

fn exchange(
    endpoint: llm::Endpoint<'_>,
    req: &Chat,
    timeout_secs: u64,
    on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&llm::profiles::ModelProfile>,
    on_event: &mut dyn FnMut(&llm::StreamEvent) -> bool,
) -> Result<Reply> {
    match on_text {
        Some(callback) => llm::chat_stream_at_controlled(
            endpoint,
            req,
            timeout_secs,
            &mut **callback,
            Some(interrupt),
            profile,
            on_event,
        ),
        None => llm::chat_at_controlled(
            endpoint,
            req,
            timeout_secs,
            Some(interrupt),
            profile,
            on_event,
        ),
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
    let llm::Endpoint {
        url,
        auth: api_key,
        model,
    } = endpoint;
    ensure!(timeout_secs > 0, "chat timeout must be positive");
    if let Some(key) = api_key {
        ensure!(
            !key.is_empty() && key.bytes().all(|b| b.is_ascii_graphic()),
            "api key is empty or contains characters that are not allowed in an HTTP header"
        );
    }
    let claude_code_oauth = api_key.is_some_and(llm::anthropic::subscription_token);
    let mut body = anthropic_request_variant(model, req, claude_code_oauth)?;
    let streaming = on_text.is_some();
    body["stream"] = serde_json::json!(streaming);
    if let Some(profile) = profile {
        profile.validate_request(req, true)?;
        if profile.capabilities.temperature && profile.thinking != Some(true) {
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
        if profile.prompt_cache {
            // Stable system/tool prefixes only; never cache dynamic tool results accidentally.
            if let Some(text) = body["system"].as_str().map(str::to_owned) {
                body["system"] = serde_json::json!([{"type":"text","text":text,"cache_control":{"type":"ephemeral"}}]);
            } else if let Some(system) = body["system"].as_array_mut() {
                if system.len() > 1 {
                    system[1]["cache_control"] = serde_json::json!({"type":"ephemeral"});
                } else if let Some(block) = system.first_mut() {
                    block["cache_control"] = serde_json::json!({"type":"ephemeral"});
                } else {
                    bail!("Anthropic prompt_cache requires a stable system or tool prefix");
                }
            } else if let Some(tool) = body["tools"]
                .as_array_mut()
                .and_then(|tools| tools.last_mut())
            {
                tool["cache_control"] = serde_json::json!({"type":"ephemeral"});
            } else {
                bail!("Anthropic prompt_cache requires a stable system or tool prefix");
            }
        }
    }
    let use_proxy = !llm::host_of(url).is_some_and(|host| llm::is_local_host(&host));
    let patience = streaming.then(|| llm::Patience::for_stream(timeout_secs, !use_proxy));
    let mut request = llm::agent_with(timeout_secs, use_proxy, Some(interrupt), patience.clone())
        .post(url)
        .header("anthropic-version", "2023-06-01");
    if streaming {
        request = request.header("Accept", "text/event-stream");
    }
    if let Some(key) = api_key {
        for (name, value) in llm::anthropic::auth_headers(url, key)? {
            request = request.header(name, value);
        }
    }
    if claude_code_oauth {
        // Attest once, after all body mutations, so the fingerprint covers the wire payload.
        attest_claude_code_body(&mut body)?;
    }
    let mut response = request
        .send_json(&body)
        .map_err(|error| llm::transport_error(error, url, api_key))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let retry_after = llm::retry::server_delay(response.headers());
        let mut bytes =
            llm::read_bounded(response.body_mut().as_reader(), llm::ERROR_EXCERPT_BYTES)
                .context("reading Anthropic error response")?;
        // Auth failures need an actionable hint, not an echoed request/credential.
        if matches!(status, 401 | 403) {
            bytes = llm::Bounded {
                data: b"check ANTHROPIC_API_KEY or sign in through /providers".to_vec(),
                truncated: false,
            };
        }
        return Err(llm::http_error(status, &bytes, api_key, retry_after));
    }
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
        let patience =
            patience.unwrap_or_else(|| llm::Patience::for_stream(timeout_secs, !use_proxy));
        llm::stream::read_events(
            response.body_mut().as_reader(),
            llm::MAX_RESPONSE_BYTES,
            llm::stream::MAX_LINE_BYTES,
            patience,
            |data| accumulator.feed(data, &mut forward, on_event),
        )
        .map_err(|error| llm::response_error(error, api_key))?;
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
        match message["role"].as_str().unwrap_or_default() {
            // Responses native replay uses Dial wrappers, not Anthropic content blocks. Old
            // sessions may contain both protocols; fall through to shared text/tool calls for
            // foreign replay data, while keeping Claude's opaque signed blocks unchanged.
            "assistant" if message["dial_provider_content"].as_array().is_some_and(|blocks|
                !blocks.is_empty() && blocks.iter().all(|block| block["type"] != "dial_response_item")) => {
                let mut content=message["dial_provider_content"].clone();
                if claude_code_oauth {
                    for block in content.as_array_mut().into_iter().flatten() {
                        if block["type"]=="tool_use"
                            && let Some(name)=block["name"].as_str().filter(|name| !name.starts_with('_')) {
                                block["name"]=json!(format!("_{name}"));
                            }
                    }
                }
                messages.push(json!({"role":"assistant","content":content}));
            }
            "system" | "developer" => {
                if let Some(text) = message["content"].as_str() { system.push(text.to_owned()); }
            }
            "tool" => messages.push(json!({"role":"user","content":[{
                "type":"tool_result","tool_use_id":message["tool_call_id"],"content":message["content"]
            }]})),
            "assistant" if message["tool_calls"].is_array() => {
                let mut content=Vec::new();
                if let Some(text)=message["content"].as_str().filter(|text|!text.is_empty()) { content.push(json!({"type":"text","text":text})); }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let raw=&call["function"]["arguments"];
                    let parsed:Value=match raw { Value::String(text)=>serde_json::from_str(text).unwrap_or_else(|_|json!({})),value=>value.clone() };
                    let input=if parsed.is_object(){parsed}else{json!({})};
                    let name=call["function"]["name"].as_str().unwrap_or_default();
                    content.push(json!({"type":"tool_use","id":call["id"],"name":if claude_code_oauth&&!name.starts_with('_'){format!("_{name}")}else{name.to_owned()},"input":input}));
                }
                messages.push(json!({"role":"assistant","content":content}));
            }
            role @ ("user" | "assistant") => messages.push(json!({"role":role,"content":llm::images::anthropic_content(&message["content"])})),
            _ => {}
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
                        let name = if claude_code_oauth && !name.starts_with('_') {
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

pub(super) fn anthropic_reply(value: &serde_json::Value) -> Result<Reply> {
    use serde_json::Value;
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
            "max_tokens" => "length",
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
                .saturating_add(
                    value
                        .pointer("/usage/cache_creation_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                )
            }),
        completion_tokens: value
            .pointer("/usage/output_tokens")
            .and_then(Value::as_u64),
        reasoning,
        cached_prompt_tokens: value
            .pointer("/usage/cache_read_input_tokens")
            .and_then(Value::as_u64),
        cache_creation_tokens: value
            .pointer("/usage/cache_creation_input_tokens")
            .and_then(Value::as_u64),
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
    #[test]
    #[cfg(unix)]
    fn lora_runtime_distinguishes_loading_use_failure_and_release() {
        let dir = tempfile::tempdir().unwrap();
        let python = super::server::tests::fake_python(dir.path());
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
        let mut broken = Backend::new(
            ProviderKind::Local {
                model_dir: "bad-model".into(),
            },
            python,
            3,
        );
        assert!(broken.chat(Some(&adapter), &request(), 5, None).is_err());
        assert_eq!(
            broken.adapter_monitor().lock().phase,
            super::AdapterPhase::Failed
        );
    }
    use super::*;
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
        backend.retry_backoff = Duration::from_millis(10);
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
            prompt_cache: true,
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
        assert_eq!(body["messages"][0]["content"], "SECRET-PROMPT");
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
        let (message, warnings) =
            crate::agent::context::expand_message(root.path(), "Describe @tiny.png");
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
    #[cfg(unix)]
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
        let png = b"\x89PNG\r\n\x1a\npixels";
        std::fs::write(root.path().join("image.png"), png).unwrap();
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
                format!("data:image/png;base64,{}", STANDARD.encode(png))
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

    #[test]
    #[cfg(unix)]
    fn vision_steering_status_and_notice_respect_manual_automatic_and_disabled_settings() {
        for (enabled, automatic, strength, state, notice_count) in [
            (true, false, 0.5, "bypassed", 1),
            (false, true, 0.5, "disabled", 0),
            (true, true, 0.0, "disabled", 0),
            (false, false, 0.5, "disabled", 0),
        ] {
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
                server::tests::fake_python(root.path()),
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
        }
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
            while connections < llm::retry::MAX_ATTEMPTS && Instant::now() < deadline {
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
        assert_eq!(peer.join().unwrap(), llm::retry::MAX_ATTEMPTS, "{error:#}");
        assert_eq!(notices.len(), llm::retry::MAX_ATTEMPTS - 1);
        assert!(format!("{error:#}").contains("received fatal alert: BadRecordMac"));
        assert!(error.to_string().contains("failed after 5 attempts"));
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
            (0..5)
                .map(|_| fake::json(503, &json!({"error":"busy"})))
                .collect(),
        );
        let mut remote = backend(endpoint(&server.url));
        let error = remote.chat(None, &request(), 5, None).unwrap_err();
        assert!(
            error.to_string().contains("failed after 5 attempts")
                && error.to_string().contains("Say \"continue\" to retry"),
            "{error:#}"
        );
        assert_eq!(error.downcast_ref::<llm::HttpError>().unwrap().status, 503);
        assert_eq!(server.requests().len(), 5);
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
    fn partial_streams_and_permanent_statuses_never_retry() {
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
        for delta in [
            json!({"content":"partial"}),
            json!({"tool_calls":[{"index":0,"id":"call","function":{"name":"read","arguments":"{"}}]}),
        ] {
            let server = fake::serve(vec![Script::Sse(format!(
                "data: {}\n\n",
                json!({"choices":[{"delta":delta}]})
            ))]);
            let mut remote = backend(endpoint(&server.url));
            let mut notices = Vec::new();
            let error = remote
                .chat_with_control(
                    None,
                    &request(),
                    5,
                    Some(&mut |_| true),
                    &mut |text| notices.push(text.to_owned()),
                    Arc::new(AtomicBool::new(false)),
                )
                .unwrap_err();
            assert!(!llm::is_transient(&error));
            assert!(notices.is_empty());
            assert_eq!(server.requests().len(), 1);
        }
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
            None,
            "remote model names do not establish context capabilities"
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
                5,
                Some(&mut |piece| {
                    text.push(piece.to_owned());
                    true
                }),
                &mut |_| {},
                Arc::new(AtomicBool::new(false)),
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
            (0..llm::retry::MAX_ATTEMPTS)
                .map(|_| Script::Sse(anthropic_sse(&events[..events.len() - 1])))
                .collect(),
        );
        let mut remote = backend(anthropic_endpoint(&server.url));
        let error = remote
            .chat(None, &request(), 5, Some(&mut |_| true))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("after {} attempts", llm::retry::MAX_ATTEMPTS)),
            "{error:#}"
        );
        assert_eq!(server.requests().len(), llm::retry::MAX_ATTEMPTS as usize);
    }

    #[test]
    fn native_anthropic_post_response_failures_never_replay_and_cancel_is_typed() {
        let events = thinking_tool_events();
        for script in [
            Script::Sse("data: not-json\n\n".into()),
            Script::Sse("data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"anth-secret SECRET-PROMPT\"}}\n\n".into()),
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
                5,
                Some(&mut |_| true),
                &mut |_| {},
                Arc::new(AtomicBool::new(false)),
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
            prompt_cache: true,
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
        let home = crate::platform::home_var().ok()?;
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
    fn subscription_stall_stops_without_retry_or_executing_calls() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let layout =
            crate::storage::paths::Layout::with_user_dir(&project, temp.path().join("home"))
                .unwrap();
        llm::chatgpt::test_account(&layout.user_dir, false);
        let overlay: toml::Table = toml::from_str("model='account-model'\nprovider='chatgpt'\napproval='auto'\nintegrations=[]\nremote_model_idle_timeout_secs=1\n[sandbox]\nmode='off'\n[improve]\nenabled=false").unwrap();
        let config = crate::config::types::Config::load(&layout, Some(overlay)).unwrap();
        let item = json!({"type":"function_call","namespace":"dial","name":"write","id":"item1","call_id":"call1","status":"completed","arguments":"{\"path\":\"marker.txt\",\"content\":\"must not run\"}"});
        let first = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type":"response.output_item.added","output_index":0,"item":item}),
            json!({"type":"response.output_item.done","output_index":0,"item":item})
        );
        let server = llm::fake::serve(vec![llm::fake::Script::StallAfter {
            first,
            silence: Duration::from_secs(3),
        }]);
        let mut backend = crate::agent::make_backend(&config).unwrap();
        backend.chatgpt_url = Some(format!("{}/codex/responses", server.url));
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
        assert!(error.to_string().contains("silent"), "{error:#}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "remote request inherited the worker allowance"
        );
        assert!(
            ui.0 > 0,
            "the UI must keep pumping while the provider stalls"
        );
        assert!(!project.join("marker.txt").exists());
        assert_eq!(
            server.requests().len(),
            1,
            "a stream stall must not replay the request"
        );
    }

    #[test]
    fn subscription_provider_runs_real_agent_tools_and_replays_results() {
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
            llm::fake::Script::Sse(sse(output.clone(), "")),
            // Editing requires the final diff review before completion is accepted.
            llm::fake::Script::Sse(sse(final_output.clone(), "Marker written.")),
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
        assert_eq!(
            std::fs::read_to_string(project.join("marker.txt")).unwrap(),
            "real output"
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        assert!(
            llm::fake::body(&requests[2])["input"]
                .to_string()
                .contains("review required before completion")
        );
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
            Some("omp")
        );
        assert_eq!(
            llm::fake::header(&requests[0], "version").as_deref(),
            Some("0.159.0")
        );
        assert_eq!(
            llm::fake::request_line(&requests[0]),
            "POST /codex/responses HTTP/1.1"
        );
        let second = llm::fake::body(&requests[1]);
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
