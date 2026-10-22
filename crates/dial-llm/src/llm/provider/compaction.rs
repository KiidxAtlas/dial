//! Native compaction transport. Native success requires real provider state, never a text fallback.
use super::{Backend, ProviderKind, anthropic_url, attest_claude_code_body, endpoint_url};
use crate::{
    config::compaction::NativeCompactionCapability,
    llm::{self, Chat, Reply},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub struct NativeCompactionReply {
    pub summary: String,
    pub prefix: Vec<Value>,
    pub reply: Reply,
    pub retained_messages: usize,
    pub notices: Vec<String>,
}
impl Backend {
    /// Preserve the controls selected for the actual projected request during native maintenance.
    pub fn compaction_profile(&self) -> Option<&llm::profiles::ModelProfile> {
        self.next_profile.as_ref().or(self.profile.as_ref())
    }
    /// Apply the summary budget only where the transport accepts output-token overrides.
    /// Keep explicit profile settings intact so unsupported user overrides still fail validation.
    pub fn compaction_summary_profile(
        &self,
        max_tokens: u32,
    ) -> Option<llm::profiles::ModelProfile> {
        self.bounded_request_profile(None, max_tokens)
    }

    /// Internal auxiliary budgets must not become unsupported subscription overrides.
    /// Explicit profile controls remain intact so transport validation still reports them.
    pub fn bounded_request_profile(
        &self,
        selected: Option<&llm::profiles::ModelProfile>,
        max_tokens: u32,
    ) -> Option<llm::profiles::ModelProfile> {
        let mut profile = selected.or(self.compaction_profile()).cloned()?;
        if max_tokens > 0 && !matches!(self.kind, ProviderKind::ChatGpt { .. }) {
            profile.max_tokens = Some(
                profile
                    .max_tokens
                    .filter(|cap| *cap > 0)
                    .map_or(max_tokens, |cap| cap.min(max_tokens)),
            );
        }
        Some(profile)
    }
    /// Expand exactly the same verified image packets as ordinary generation, before selecting a cut.
    pub fn prepare_compaction_request(
        &self,
        request: &Chat,
        notice: &mut dyn FnMut(&str),
    ) -> Option<Chat> {
        llm::images::prepare(
            request,
            self.image_input_for(self.compaction_profile()),
            &self.image_notices,
            notice,
        )
    }
    pub fn compaction_anthropic_family(&self) -> bool {
        match &self.kind {
            ProviderKind::AnthropicSubscription { .. } => true,
            ProviderKind::AnthropicEndpoint { .. } | ProviderKind::Endpoint { .. } => {
                self.protocol == Some(llm::model_config::ApiProtocol::Anthropic)
                    || (self.protocol.is_none()
                        && matches!(self.kind, ProviderKind::AnthropicEndpoint { .. }))
            }
            _ => false,
        }
    }
    /// Resolve actual transport identity without sending a model request. Cache routing is not authority.
    pub fn validate_compaction_request(
        &self,
        request: &Chat,
        interrupt: Arc<AtomicBool>,
    ) -> Result<()> {
        if !request.messages.iter().any(|m| {
            !m["dial_compaction_model"].is_null()
                || !m["dial_anthropic_compaction"].is_null()
                || m["dial_provider_content"].as_array().is_some_and(|items| {
                    items
                        .iter()
                        .any(|item| item["type"] == "dial_response_item")
                })
        }) {
            return Ok(());
        }
        self.require_session_identity()?;
        let codex = match &self.kind {
            ProviderKind::ChatGpt { user_dir, .. } => Some(llm::chatgpt::scoped_account_access(
                user_dir,
                self.account_id
                    .as_deref()
                    .context("ChatGPT backend has no pinned account")?,
                interrupt.clone(),
            )?),
            _ => None,
        };
        let oauth = match &self.kind {
            ProviderKind::AnthropicSubscription { user_dir, .. } => {
                Some(llm::anthropic_oauth::access_token(user_dir, interrupt)?)
            }
            _ => None,
        };
        let headers = llm::model_config::resolve_validated_headers(&self.headers)?;
        let pairs: Vec<_> = headers.pairs().collect();
        let endpoint_key = self.endpoint_api_key()?;
        let (url, model, token, account, anthropic) = match &self.kind {
            ProviderKind::ChatGpt { model, .. } => (
                format!("{}/codex/responses", llm::chatgpt::BASE_URL),
                model.as_str(),
                codex.as_ref().map(|a| a.1.as_str()),
                codex.as_ref().map(|a| a.0.as_str()),
                false,
            ),
            ProviderKind::AnthropicSubscription { model, .. } => (
                format!("{}/messages", llm::anthropic::BASE_URL),
                model.as_str(),
                oauth.as_ref().map(llm::auth::SecretToken::as_str),
                None,
                true,
            ),
            ProviderKind::Endpoint {
                base_url, model, ..
            }
            | ProviderKind::AnthropicEndpoint {
                base_url, model, ..
            } => {
                let protocol = self.protocol.unwrap_or(
                    if matches!(self.kind, ProviderKind::AnthropicEndpoint { .. }) {
                        llm::model_config::ApiProtocol::Anthropic
                    } else {
                        llm::model_config::ApiProtocol::ChatCompletions
                    },
                );
                ensure!(
                    matches!(
                        protocol,
                        llm::model_config::ApiProtocol::Responses
                            | llm::model_config::ApiProtocol::Anthropic
                    ),
                    "native history cannot be replayed through this protocol; compact the immutable archive to portable history"
                );
                let anthropic = protocol == llm::model_config::ApiProtocol::Anthropic;
                let url = if anthropic {
                    anthropic_url(base_url, self.allow_insecure)?
                } else {
                    endpoint_url(base_url, self.allow_insecure)?
                        .trim_end_matches("/chat/completions")
                        .to_owned()
                        + "/responses"
                };
                (
                    url,
                    model.as_str(),
                    endpoint_key.as_deref(),
                    None,
                    anthropic,
                )
            }
            ProviderKind::Local { .. } | ProviderKind::Apple => {
                bail!("native history cannot be replayed through a local provider")
            }
        };
        if anthropic {
            let scope = llm::model_config::native_replay_scope(
                "anthropic-compaction",
                &url,
                Some(model),
                None,
                token,
                &pairs,
            );
            return super::validate_anthropic_compaction_scope(request, &scope, model)
                .map_err(|error| headers.redact(error));
        }
        let scope = llm::model_config::native_replay_scope(
            "responses",
            &url,
            None,
            account,
            Some(token.unwrap_or("")),
            &pairs,
        );
        for message in &request.messages {
            ensure!(
                message["dial_anthropic_compaction"].is_null(),
                "Anthropic native state cannot be replayed through Responses"
            );
            ensure!(
                message["dial_compaction_model"]
                    .as_str()
                    .is_none_or(|owner| owner == model),
                "native compaction belongs to another model"
            );
            for item in message["dial_provider_content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "dial_response_item")
            {
                ensure!(
                    item["scope"].as_str() == Some(scope.as_str()),
                    "native history belongs to another live credential, account, endpoint, or header scope; no compaction committed"
                );
            }
        }
        Ok(())
    }
    /// Metadata/explicit-route capability, not a speculative request to discover support.
    pub fn supports_native_compaction(
        &self,
        capability: Option<NativeCompactionCapability>,
        streaming_v2: bool,
    ) -> bool {
        use NativeCompactionCapability::*;
        match &self.kind {
            ProviderKind::ChatGpt { .. } => {
                streaming_v2 && matches!(capability, Some(ResponsesV2 | ResponsesV1V2))
            }
            ProviderKind::AnthropicSubscription { .. } => capability == Some(Anthropic),
            ProviderKind::Endpoint { .. } | ProviderKind::AnthropicEndpoint { .. } => {
                let protocol = self.protocol.unwrap_or(
                    if matches!(self.kind, ProviderKind::AnthropicEndpoint { .. }) {
                        llm::model_config::ApiProtocol::Anthropic
                    } else {
                        llm::model_config::ApiProtocol::ChatCompletions
                    },
                );
                match protocol {
                    llm::model_config::ApiProtocol::Responses => {
                        matches!(capability, Some(ResponsesV1 | ResponsesV1V2))
                            || (streaming_v2 && capability == Some(ResponsesV2))
                    }
                    llm::model_config::ApiProtocol::Anthropic => capability == Some(Anthropic),
                    _ => false,
                }
            }
            ProviderKind::Local { .. } | ProviderKind::Apple => false,
        }
    }
    pub fn compact_native(
        &mut self,
        request: &Chat,
        capability: Option<NativeCompactionCapability>,
        streaming_v2: bool,
        retained_user_budget: u32,
        first_kept: usize,
        instructions: &str,
        timeout_secs: u64,
        interrupt: Arc<AtomicBool>,
    ) -> Result<NativeCompactionReply> {
        self.require_session_identity()?;
        ensure!(
            self.supports_native_compaction(capability, streaming_v2),
            "the active model/route has no declared native compaction support"
        );
        let billing = if matches!(
            self.kind,
            ProviderKind::ChatGpt { .. } | ProviderKind::AnthropicSubscription { .. }
        ) {
            llm::usage::Billing::Subscription
        } else {
            llm::usage::Billing::Api
        };
        let scope = self
            .usage_scope
            .as_ref()
            .map(|scope| scope.worker().for_purpose(llm::usage::Purpose::Compaction));
        if let Some(scope) = &scope {
            scope.begin(billing)?;
        }
        let transport = llm::usage::Transport::enter(scope.clone());
        let result = self
            .compact_native_inner(
                request,
                capability,
                streaming_v2,
                retained_user_budget,
                first_kept,
                instructions,
                timeout_secs,
                interrupt,
            )
            .map_err(|cause| {
                if transport.attempts() > 0 && cause.downcast_ref::<llm::Interrupted>().is_none() {
                    NativeCompactionFailure { cause }.into()
                } else {
                    cause
                }
            });
        transport.settle(result.is_ok())?;
        if let Some(scope) = scope {
            scope.finish(
                result.as_ref().ok().map(|r| &r.reply),
                result.as_ref().ok().and_then(|r| self.reply_cost(&r.reply)),
                billing,
                transport.attempts(),
            )?;
        }
        result
    }
    fn compact_native_inner(
        &mut self,
        request: &Chat,
        capability: Option<NativeCompactionCapability>,
        streaming_v2: bool,
        retained_user_budget: u32,
        first_kept: usize,
        instructions: &str,
        timeout_secs: u64,
        interrupt: Arc<AtomicBool>,
    ) -> Result<NativeCompactionReply> {
        use NativeCompactionCapability::*;
        let timeout = self.response_timeout(timeout_secs);
        let timeout = if timeout == 0 { 300 } else { timeout.min(300) };
        let codex = match &self.kind {
            ProviderKind::ChatGpt { user_dir, .. } => Some(llm::chatgpt::scoped_account_access(
                user_dir,
                self.account_id
                    .as_deref()
                    .context("ChatGPT backend has no pinned account")?,
                interrupt.clone(),
            )?),
            _ => None,
        };
        let oauth = match &self.kind {
            ProviderKind::AnthropicSubscription { user_dir, .. } => Some(
                llm::anthropic_oauth::access_token(user_dir, interrupt.clone())?,
            ),
            _ => None,
        };
        let headers = llm::model_config::resolve_validated_headers(&self.headers)?;
        let mut notices = Vec::new();
        let prepared = llm::images::prepare(
            request,
            self.image_input_for(self.compaction_profile()),
            &self.image_notices,
            &mut |text| notices.push(text.to_owned()),
        );
        let request = prepared.as_ref().unwrap_or(request);
        let pairs: Vec<_> = headers.pairs().collect();
        let endpoint_key = self.endpoint_api_key()?;
        let (url, model, token, account, anthropic) = match &self.kind {
            ProviderKind::ChatGpt { model, .. } => (
                format!("{}/codex/responses", llm::chatgpt::BASE_URL),
                model.as_str(),
                codex.as_ref().map(|a| a.1.as_str()),
                codex.as_ref().map(|a| a.0.as_str()),
                false,
            ),
            ProviderKind::AnthropicSubscription { model, .. } => (
                format!("{}/messages", llm::anthropic::BASE_URL),
                model.as_str(),
                oauth.as_ref().map(|a| a.as_str()),
                None,
                true,
            ),
            ProviderKind::Endpoint {
                base_url, model, ..
            }
            | ProviderKind::AnthropicEndpoint {
                base_url, model, ..
            } => {
                let anthropic = self.protocol == Some(llm::model_config::ApiProtocol::Anthropic)
                    || (self.protocol.is_none()
                        && matches!(self.kind, ProviderKind::AnthropicEndpoint { .. }));
                let url = if anthropic {
                    anthropic_url(base_url, self.allow_insecure)?
                } else {
                    endpoint_url(base_url, self.allow_insecure)?
                        .trim_end_matches("/chat/completions")
                        .to_owned()
                        + "/responses"
                };
                (
                    url,
                    model.as_str(),
                    endpoint_key.as_deref(),
                    None,
                    anthropic,
                )
            }
            ProviderKind::Local { .. } | ProviderKind::Apple => {
                bail!("local providers have no native compaction route")
            }
        };
        if anthropic {
            let mut reply = anthropic_compact(
                llm::Endpoint {
                    url: &url,
                    auth: token,
                    model,
                    headers: &pairs,
                },
                request,
                first_kept,
                instructions,
                timeout,
                interrupt,
                self.compaction_profile(),
            )
            .map_err(|error| headers.redact(error))?;
            reply.notices = notices;
            return Ok(reply);
        }
        let endpoint = llm::responses::Endpoint {
            url: &url,
            token: token.unwrap_or(""),
            model,
            account_id: account,
            headers: &pairs,
        };
        let v2 = streaming_v2 && matches!(capability, Some(ResponsesV2 | ResponsesV1V2));
        let v1 = account.is_none() && matches!(capability, Some(ResponsesV1 | ResponsesV1V2));
        if v2 {
            match llm::responses::compact(
                endpoint,
                request,
                instructions,
                true,
                retained_user_budget,
                timeout,
                interrupt.clone(),
                self.compaction_profile(),
                self.prompt_cache_key.as_deref(),
            ) {
                Ok(mut reply) => {
                    reply.notices = notices;
                    return Ok(reply);
                }
                Err(error) => {
                    if interrupt.load(Ordering::SeqCst) || !v1 {
                        return Err(headers.redact(error));
                    }
                }
            }
        }
        ensure!(v1, "no declared V1 fallback exists for this native route");
        let mut reply = llm::responses::compact(
            endpoint,
            request,
            instructions,
            false,
            retained_user_budget,
            timeout,
            interrupt,
            self.compaction_profile(),
            self.prompt_cache_key.as_deref(),
        )
        .map_err(|error| headers.redact(error))?;
        reply.notices = notices;
        Ok(reply)
    }
    /// Custom summary endpoints are authenticated against their OWN URL, never the origin model.
    pub fn compact_remote_summary(
        &mut self,
        endpoint: &str,
        user_dir: &Path,
        request: &Chat,
        timeout_secs: u64,
        interrupt: Arc<AtomicBool>,
    ) -> Result<Reply> {
        let uri = url::Url::parse(endpoint).context("invalid remote compaction endpoint")?;
        endpoint_url(endpoint, self.allow_insecure)?;
        let token = llm::catalog::resolve_credential(user_dir, endpoint, None)?;
        let system = request
            .messages
            .iter()
            .filter(|m| m["role"] == "system")
            .filter_map(|m| m["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let prompt = request
            .messages
            .iter()
            .filter(|m| m["role"] != "system")
            .filter_map(|m| m["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let chat = uri
            .path()
            .trim_end_matches('/')
            .ends_with("/chat/completions");
        let body = if chat {
            json!({"model":self.model_label(),"messages":request.messages,"stream":false,"max_tokens":request.max_tokens})
        } else {
            json!({"systemPrompt":system,"prompt":prompt,"maxTokens":request.max_tokens})
        };
        let billing = if llm::host_of(endpoint).is_some_and(|h| llm::is_local_host(&h)) {
            llm::usage::Billing::Local
        } else {
            llm::usage::Billing::Api
        };
        let scope = self.usage_scope.as_ref().map(|scope| {
            scope
                .worker()
                .for_purpose(llm::usage::Purpose::Compaction)
                .for_model(format!("remote-compaction/{}", llm::display_url(endpoint)))
        });
        if let Some(scope) = &scope {
            scope.begin(billing)?;
        }
        let transport = llm::usage::Transport::enter(scope.clone());
        let result = (|| -> Result<Reply> {
            let timeout = if timeout_secs == 0 {
                300
            } else {
                timeout_secs.min(300)
            };
            let data = post_json(endpoint, token.as_deref(), &body, timeout, interrupt, &[])?;
            let text = if chat {
                &data["choices"][0]["message"]["content"]
            } else {
                &data["summary"]
            };
            let summary = text
                .as_str()
                .map(str::to_owned)
                .or_else(|| {
                    text.as_array().map(|blocks| {
                        blocks
                            .iter()
                            .filter_map(|b| b["text"].as_str())
                            .collect::<String>()
                    })
                })
                .context("remote compaction response has no summary text")?;
            ensure!(
                !summary.trim().is_empty(),
                "remote compaction response has an empty summary"
            );
            ensure!(
                !chat || data["choices"][0]["finish_reason"] != "length",
                "remote compaction summary was truncated"
            );
            ensure!(
                !chat
                    || data["choices"][0]["message"]["tool_calls"]
                        .as_array()
                        .is_none_or(Vec::is_empty),
                "remote compaction returned tool calls instead of a complete summary"
            );
            Ok(Reply {
                text: summary,
                finish_reason: "stop".into(),
                prompt_tokens: data["usage"]["prompt_tokens"].as_u64(),
                completion_tokens: data["usage"]["completion_tokens"].as_u64(),
                ..Reply::default()
            })
        })();
        if let Some(scope) = scope {
            scope.finish(result.as_ref().ok(), None, billing, transport.attempts())?;
        }
        result
    }
}
pub(in crate::llm) fn post_json(
    url: &str,
    token: Option<&str>,
    body: &Value,
    timeout: u64,
    interrupt: Arc<AtomicBool>,
    headers: &[(&str, &str)],
) -> Result<Value> {
    let mut response = llm::send_with_headers(
        url,
        token,
        body,
        timeout,
        None,
        Some(interrupt.clone()),
        headers,
    )?;
    let status = response.status().as_u16();
    let bytes = llm::read_bounded(
        response.body_mut().as_reader(),
        if (200..300).contains(&status) {
            llm::MAX_RESPONSE_BYTES
        } else {
            llm::ERROR_EXCERPT_BYTES
        },
    )?;
    if !(200..300).contains(&status) {
        return Err(llm::http_error(
            status,
            &bytes,
            token,
            llm::retry::server_delay(response.headers()),
        ));
    }
    ensure!(
        !bytes.truncated,
        "compaction response exceeds the response byte limit"
    );
    if interrupt.load(Ordering::SeqCst) {
        return Err(llm::Interrupted.into());
    }
    serde_json::from_slice(&bytes.data).context("invalid compaction response JSON")
}
fn anthropic_compact(
    endpoint: llm::Endpoint<'_>,
    request: &Chat,
    first_kept: usize,
    instructions: &str,
    timeout: u64,
    interrupt: Arc<AtomicBool>,
    profile: Option<&llm::profiles::ModelProfile>,
) -> Result<NativeCompactionReply> {
    let oauth = endpoint
        .auth
        .is_some_and(llm::anthropic::subscription_token);
    let scope = llm::model_config::native_replay_scope(
        "anthropic-compaction",
        endpoint.url,
        Some(endpoint.model),
        None,
        endpoint.auth,
        endpoint.headers,
    );
    super::validate_anthropic_compaction_scope(request, &scope, endpoint.model)?;
    let mut prefix = request.clone();
    prefix.messages = request.messages[..first_kept]
        .iter()
        .filter(|m| m["dial_transient"] != true && m["dial_portable_rules"] != true)
        .cloned()
        .collect();
    // Summarize the exact wire prefix, retaining the same controls and unchanged wire tail.
    let mut body = super::anthropic_request_body(endpoint.model, &prefix, oauth, profile)?;
    let mut snapshot = request.clone();
    snapshot
        .messages
        .retain(|m| m["dial_transient"] != true && m["dial_portable_rules"] != true);
    let full = super::anthropic_request_body(endpoint.model, &snapshot, oauth, profile)?;
    let wire = full["messages"]
        .as_array()
        .context("Anthropic compaction request has no wire history")?;
    let prefix_wire = body["messages"]
        .as_array()
        .context("Anthropic compaction request has no history")?;
    let mut prefix_len = prefix_wire.len();
    ensure!(
        prefix_len <= wire.len(),
        "Anthropic compaction wire prefix is inconsistent"
    );
    // Consecutive source users/tool-result users merge on the wire: never silently drop a merged tail.
    if prefix_len > 0 && prefix_wire.last() != wire.get(prefix_len - 1) {
        prefix_len -= 1;
    }
    while prefix_len > 0
        && prefix_len < wire.len()
        && (wire[prefix_len]["role"] != "user"
            || wire[prefix_len]["content"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part["type"] == "tool_result")))
    {
        prefix_len -= 1;
    }
    ensure!(
        prefix_len > 0,
        "Anthropic native compaction cannot preserve a complete retained wire group"
    );
    let tail = wire[prefix_len..].to_vec();
    body["messages"] = json!(&wire[..prefix_len]);
    let retained_messages = tail.len();
    body["compaction"] = json!({"type":"summarize","instructions":instructions});
    body["stream"] = json!(false);
    body.as_object_mut().unwrap().remove("temperature");
    body.as_object_mut().unwrap().remove("top_p");
    if oauth {
        attest_claude_code_body(&mut body)?;
    }
    let mut headers = vec![("anthropic-version", "2023-06-01".to_owned())];
    if let Some(token) = endpoint.auth {
        for (name, value) in llm::anthropic::auth_headers(endpoint.url, token)? {
            headers.push((name, value));
        }
    }
    let mut beta = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    if !beta.is_empty() {
        beta.push(',');
    }
    beta.push_str("compact-2026-09-04");
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("anthropic-beta"));
    headers.push(("anthropic-beta", beta));
    headers.extend(
        endpoint
            .headers
            .iter()
            .map(|(name, value)| (*name, (*value).to_owned())),
    );
    let pairs = headers
        .iter()
        .map(|(k, v)| (*k, v.as_str()))
        .collect::<Vec<_>>();
    let data = post_json(endpoint.url, None, &body, timeout, interrupt, &pairs)
        .map_err(|error| llm::response_error(error, endpoint.auth))?;
    ensure!(
        data["stop_reason"] == "compaction",
        "Anthropic native compaction did not return compaction stop reason"
    );
    let blocks = data["content"]
        .as_array()
        .context("Anthropic compaction response has no content")?;
    ensure!(
        blocks.len() == 1 && blocks[0]["type"] == "compaction",
        "Anthropic native compaction returned non-compaction output"
    );
    let summary = blocks[0]["content"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .context("Anthropic compaction returned an empty summary")?
        .to_owned();
    ensure!(
        blocks[0]["signature"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "Anthropic compaction returned no signed summary"
    );
    let mut history = vec![json!({"role":"assistant","content":blocks})];
    history.extend(tail);
    let (cache_writes, cache_writes_5m, cache_writes_1h) =
        crate::llm::provider::anthropic_cache_writes(&data["usage"]);
    let reply = Reply {
        text: summary.clone(),
        finish_reason: "compaction".into(),
        prompt_tokens: data["usage"]["input_tokens"].as_u64().map(|n| {
            n.saturating_add(
                data["usage"]["cache_read_input_tokens"]
                    .as_u64()
                    .unwrap_or(0),
            )
            .saturating_add(cache_writes.unwrap_or(0))
        }),
        completion_tokens: data["usage"]["output_tokens"].as_u64(),
        cached_prompt_tokens: data["usage"]["cache_read_input_tokens"].as_u64(),
        cache_creation_tokens: cache_writes,
        cache_creation_5m_tokens: cache_writes_5m,
        cache_creation_1h_tokens: cache_writes_1h,
        ..Reply::default()
    };
    Ok(NativeCompactionReply {
        summary: summary.clone(),
        prefix: vec![
            json!({"role":"assistant","content":summary,"dial_anthropic_compaction":{"scope":scope,"model":endpoint.model,"history":history}}),
        ],
        reply,
        retained_messages,
        notices: Vec::new(),
    })
}

/// Only an attempted native transport can disable a native route; local preparation errors cannot.
#[derive(Debug)]
pub struct NativeCompactionFailure {
    pub cause: anyhow::Error,
}
impl std::fmt::Display for NativeCompactionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native compaction failed: {}", self.cause)
    }
}
impl std::error::Error for NativeCompactionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

#[cfg(test)]
mod tests;
