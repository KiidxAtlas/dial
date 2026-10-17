//! Responses protocol adapter for API endpoints and ChatGPT plan usage. Full history and native
//! replay are retained; only a terminal completed response authorizes local tool execution.
use super::{Chat, Interrupted, Patience, Reply, StreamEvent, ToolCall};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicBool},
};
#[derive(Debug)]
pub(super) struct TerminalFailure {
    kind: &'static str,
    code: &'static str,
}
impl TerminalFailure {
    pub(super) fn retryable(&self) -> bool {
        self.kind != "response.incomplete"
            && matches!(
                self.code,
                "server_error"
                    | "model_error"
                    | "websocket_connection_limit_reached"
                    | "previous_response_not_found"
            )
    }
    pub(super) fn class(&self) -> super::errors::ErrorClass {
        match self.code {
            "rate_limit_exceeded" => super::errors::ErrorClass::RateLimited { retry_after: None },
            "insufficient_quota"
            | "subscription_sharing_usage_limit_exceeded"
            | "subscription_sharing_usage_unavailable" => {
                super::errors::ErrorClass::QuotaExhausted { reset_at: None }
            }
            "invalid_api_key" | "authentication_error" | "permission_denied" => {
                super::errors::ErrorClass::Auth
            }
            "server_error"
            | "model_error"
            | "websocket_connection_limit_reached"
            | "previous_response_not_found"
                if self.retryable() =>
            {
                super::errors::ErrorClass::Transient
            }
            _ => super::errors::ErrorClass::Fatal,
        }
    }
}
impl std::fmt::Display for TerminalFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if matches!(
            self.code,
            "subscription_sharing_usage_limit_exceeded" | "subscription_sharing_usage_unavailable"
        ) {
            return f.write_str("ChatGPT plan usage is unavailable or its shared allowance is exhausted; review Dial in ChatGPT Settings > Usage");
        }
        let field = if self.kind == "response.incomplete" {
            "reason"
        } else {
            "code"
        };
        write!(
            f,
            "Responses {} ({field}: {}); no tool calls were executed",
            self.kind, self.code
        )
    }
}
impl std::error::Error for TerminalFailure {}

pub(super) fn advances_reply(data: &str) -> bool {
    let Ok(event) = serde_json::from_str::<Value>(data) else {
        return false;
    };
    match event["type"].as_str() {
        Some(
            "response.output_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.function_call_arguments.delta",
        ) => event["delta"].as_str().is_some_and(|s| !s.is_empty()),
        Some(
            "response.output_item.added"
            | "response.output_item.done"
            | "response.completed"
            | "response.failed"
            | "response.incomplete"
            | "error",
        ) => true,
        _ => false,
    }
}

fn request_api(
    model: &str,
    chat: &Chat,
    profile: Option<&super::profiles::ModelProfile>,
) -> Result<Value> {
    let mut body = request_flat(model, chat, profile, false)?;
    body["max_output_tokens"] = json!(
        profile
            .and_then(|p| p.max_tokens)
            .filter(|n| *n > 0)
            .unwrap_or(chat.max_tokens)
    );
    if let Some(p) = profile {
        if p.capabilities.temperature {
            body["temperature"] = json!(p.temperature.unwrap_or(chat.temperature));
        }
        if p.capabilities.top_p
            && let Some(top_p) = p.top_p
        {
            body["top_p"] = json!(top_p);
        }
    }
    Ok(body)
}

/// Whether a Responses request carries the session's prompt-cache routing key. ChatGPT/Codex routes
/// send it whenever one exists, unless the profile sets `prompt_cache = false`. API-key routes keep
/// the declared-capability gate.
pub(super) fn sends_prompt_cache_key(
    profile: Option<&super::profiles::ModelProfile>,
    codex: bool,
) -> bool {
    if codex {
        profile.and_then(|p| p.prompt_cache) != Some(false)
    } else {
        profile.is_some_and(|p| p.capabilities.prompt_cache && p.prompt_cache != Some(false))
    }
}

/// OpenAI prompt-cache policy for API-key Responses requests. Operator opt-in only:
/// `prompt_cache = true` declares explicit breakpoints; `cache_retention = "long"` requests the
/// 24h retention on api.openai.com. ChatGPT subscription routes never reach this path.
fn apply_api_prompt_cache(
    body: &mut Value,
    profile: Option<&super::profiles::ModelProfile>,
    url: &str,
) {
    let Some(profile) = profile else { return };
    let keyed = body.get("prompt_cache_key").is_some();
    if keyed
        && profile.cache_retention == super::profiles::CacheRetention::Long
        && super::host_of(url).as_deref() == Some("api.openai.com")
    {
        body["prompt_cache_retention"] = json!("24h");
    }
    if profile.prompt_cache == Some(true) && profile.capabilities.prompt_cache {
        body["prompt_cache_options"] = json!({"mode":"explicit","ttl":"30m"});
        if let Some(input) = body["input"].as_array_mut() {
            mark_latest_stable_breakpoint(input);
        }
    }
}

/// Marks the newest stable cache breakpoint for requests without a stateful baseline:
/// marks the newest cacheable item before the latest user/developer turn. Plain-string
/// instructions and user text are wrapped as `input_text` so the marker can sit on them;
/// assistant output is never a breakpoint target.
fn mark_latest_stable_breakpoint(input: &mut [Value]) -> bool {
    let Some(latest) = input
        .iter()
        .rposition(|item| matches!(item["role"].as_str(), Some("user" | "developer")))
    else {
        return false;
    };
    if latest == 0 {
        return false;
    }
    for item in input[..latest].iter_mut().rev() {
        let wraps_text = match item["role"].as_str() {
            Some("developer" | "system" | "user") => true,
            Some("assistant") => false,
            _ => continue,
        };
        if let Some(text) = item["content"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
        {
            if wraps_text {
                item["content"] = json!([{
                    "type": "input_text",
                    "text": text,
                    "prompt_cache_breakpoint": {"mode": "explicit"}
                }]);
                return true;
            }
            continue;
        }
        if let Some(blocks) = item["content"].as_array_mut()
            && blocks.iter().all(cacheable_block)
            && let Some(last) = blocks.last_mut()
        {
            last["prompt_cache_breakpoint"] = json!({"mode": "explicit"});
            return true;
        }
    }
    false
}

fn cacheable_block(block: &Value) -> bool {
    matches!(
        block["type"].as_str(),
        Some("input_text" | "input_image" | "input_file")
    )
}

pub(super) fn exchange_api(
    endpoint: super::Endpoint<'_>,
    chat: &Chat,
    timeout: u64,
    on_text: &mut OnText<'_>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&super::profiles::ModelProfile>,
    cache_key: Option<&str>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    exchange_inner(
        Endpoint {
            headers: endpoint.headers,
            url: endpoint.url,
            token: endpoint.auth.unwrap_or(""),
            account_id: None,
            model: endpoint.model,
        },
        chat,
        timeout,
        on_text,
        interrupt,
        profile,
        on_event,
        cache_key,
        None,
    )
}

fn content(value: &Value, assistant: bool) -> Result<Value> {
    if value.is_string() {
        return Ok(value.clone());
    }
    if value.is_null() {
        return Ok(json!(""));
    }
    let blocks = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("unsupported Responses message content"))?;
    let mut out = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
        Some("text") if assistant => out.push(json!({"type":"output_text","text":block["text"].as_str().unwrap_or(""),"annotations":[]})),
        Some("text") => out.push(json!({"type":"input_text","text":block["text"].as_str().unwrap_or("")})),
        Some("image_url") if !assistant => {
            let url=block["image_url"]["url"].as_str().ok_or_else(||anyhow::anyhow!("invalid Responses image"))?;
            let mut image=json!({"type":"input_image","image_url":url});
            image["detail"]=json!(block["image_url"]["detail"].as_str().unwrap_or("auto"));
            out.push(image);
        },
        _=>bail!("unsupported Responses content block"),
    }
    }
    Ok(json!(out))
}
#[cfg(any(test, feature = "test-support"))]
pub fn request(
    model: &str,
    chat: &Chat,
    profile: Option<&super::profiles::ModelProfile>,
) -> Result<Value> {
    request_inner(model, chat, profile, true)
}
fn request_inner(
    model: &str,
    chat: &Chat,
    profile: Option<&super::profiles::ModelProfile>,
    subscription: bool,
) -> Result<Value> {
    let mut input = Vec::new();
    for message in &chat.messages {
        match message["role"].as_str() {
            Some("system" | "developer") => input.push(json!({"role":"developer","content":content(&message["content"],false)?})),
            Some("user") => input.push(json!({"role":"user","content":content(&message["content"],false)?})),
            Some("tool") => input.push(json!({"type":"function_call_output","call_id":message["tool_call_id"],"output":message["content"].as_str().unwrap_or("")})),
            Some("assistant") => {
                let native=message["dial_provider_content"].as_array().filter(|items| !items.is_empty() && items.iter().all(|item| item["type"]=="dial_response_item"));
                if let Some(native)=native { input.extend(native.iter().map(|item|item["item"].clone())); continue; }
                if !message["content"].is_null() { input.push(json!({"role":"assistant","content":content(&message["content"],true)?})); }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    input.push(json!({"type":"function_call","call_id":call["id"],"name":call["function"]["name"],"namespace":"dial","arguments":call["function"]["arguments"]}));
                }
            },
            _=>bail!("unsupported Responses message role"),
        }
    }
    let mut body = json!({"model":model,"input":input,"tools":[],"store":false,"stream":true,"include":["reasoning.encrypted_content"]});
    if !chat.tools.is_empty() {
        let mut functions = Vec::new();
        for tool in &chat.tools {
            ensure!(
                tool["type"] == "function" && tool["function"]["name"].is_string(),
                "Responses supports Dial function tools only"
            );
            let mut function = tool["function"].clone();
            function["type"] = json!("function");
            function["strict"] = json!(false);
            if profile.is_some_and(|p| p.capabilities.strict_tools)
                && let Some(parameters) =
                    crate::llm::reliability::strict_parameters(&function["parameters"])
            {
                function["parameters"] = parameters;
                function["strict"] = json!(true);
            }
            functions.push(function);
        }
        body["tools"] = json!([{"type":"namespace","name":"dial","description":"Tools executed locally by Dial.","tools":functions}]);
    }
    if let Some(profile) = profile {
        if !chat.tools.is_empty()
            && let Some(parallel) = profile.capabilities.parallel_tool_calls
        {
            body["parallel_tool_calls"] = json!(parallel);
        }
        profile.validate_responses_request(chat)?;
        ensure!(
            !subscription
                || (profile.temperature.is_none()
                    && profile.top_p.is_none()
                    && !profile.max_tokens.is_some_and(|n| n > 0)),
            "ChatGPT plan usage does not support sampling or output-token limit overrides; remove them from this model profile"
        );
        ensure!(
            !subscription || profile.cache_retention == super::profiles::CacheRetention::Auto,
            "ChatGPT plan usage does not support cache_retention overrides; remove them from this model profile"
        );
        if let Some(effort) = profile.reasoning_effort.or_else(|| {
            (profile.thinking == Some(false)).then_some(super::profiles::ReasoningEffort::None)
        }) {
            body["reasoning"] = json!({"effort":effort.as_str()});
        }
        if let Some(tier) = profile.request_tiers.service_tier_for(&profile.provider) {
            body["service_tier"] = json!(tier);
        }
    }
    Ok(body)
}
pub(super) fn request_codex(
    model: &str,
    chat: &Chat,
    profile: Option<&super::profiles::ModelProfile>,
) -> Result<Value> {
    request_flat(model, chat, profile, true)
}

/// Convert already-prepared Dial messages to the native Codex Responses input format.
pub fn input_items(messages: &[Value]) -> Result<Value> {
    let chat = Chat {
        messages: messages.to_vec(),
        tools: Vec::new(),
        temperature: 0.0,
        max_tokens: 1,
        seed: None,
    };
    Ok(request_codex("codex", &chat, None)?["input"].clone())
}
fn request_flat(
    model: &str,
    chat: &Chat,
    profile: Option<&super::profiles::ModelProfile>,
    subscription: bool,
) -> Result<Value> {
    let mut body = request_inner(model, chat, profile, subscription)?;
    if let Some(input) = body["input"].as_array_mut() {
        for item in input {
            if item["type"] == "function_call" {
                item.as_object_mut().unwrap().remove("namespace");
            }
        }
    }
    if let Some(tools) = body["tools"]
        .as_array_mut()
        .and_then(|tools| tools.first_mut())
        .filter(|tool| tool["type"] == "namespace")
    {
        let functions = tools["tools"].take();
        body["tools"] = functions;
    }
    Ok(body)
}
fn tool_name(item: &Value) -> Result<String> {
    ensure!(
        item.get("namespace").is_none_or(|n| n == "dial"),
        "Responses returned an unexpected tool namespace"
    );
    let name = item["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Responses function call has no name"))?;
    Ok(name.strip_prefix("dial.").unwrap_or(name).into())
}
fn completed(response: &Value) -> Result<Reply> {
    ensure!(
        response["status"] == "completed",
        "Responses did not complete successfully"
    );
    reply_from_output(response, false)
}
/// Builds the reply from a response's output items. `partial` admits the in-progress message of a
/// response that stopped at its token limit; unfinished tool calls always fail.
fn reply_from_output(response: &Value, partial: bool) -> Result<Reply> {
    let items = response["output"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Responses completion has no output array"))?;
    let mut reply = Reply {
        finish_reason: "stop".into(),
        ..Default::default()
    };
    let mut ids = std::collections::HashSet::new();
    for item in items {
        match item["type"].as_str() {
            Some("message") => {
                ensure!(
                    item["role"] == "assistant"
                        && item
                            .get("status")
                            .is_none_or(|s| { s == "completed" || (partial && s == "incomplete") }),
                    "Responses message is unfinished"
                );
                for block in item["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("output_text") => {
                            reply.text.push_str(block["text"].as_str().unwrap_or(""))
                        }
                        Some("refusal") => {
                            reply.text.push_str(block["refusal"].as_str().unwrap_or(""))
                        }
                        _ => {}
                    }
                }
            }
            Some("function_call") => {
                ensure!(
                    reply.tool_calls.len() < 128,
                    "too many Responses function calls"
                );
                ensure!(
                    item.get("status").is_none_or(|s| s == "completed"),
                    "Responses function call is unfinished"
                );
                let id = item["call_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Responses function call has no call ID"))?;
                ensure!(ids.insert(id), "duplicate Responses function call ID");
                let raw = item["arguments"].as_str().ok_or_else(|| {
                    anyhow::anyhow!("Responses function call has no argument string")
                })?;
                let arguments = super::json_repair::parse_arguments(raw).filter(Value::is_object);
                let malformed = arguments.is_none();
                reply.tool_calls.push(ToolCall {
                    id: id.into(),
                    name: tool_name(item)?,
                    arguments: arguments.unwrap_or(Value::Null),
                    malformed_arguments: malformed,
                });
                reply.finish_reason = "tool_calls".into();
            }
            Some("reasoning") => {
                for part in item["summary"].as_array().into_iter().flatten() {
                    if let Some(text) = part["text"].as_str() {
                        reply.reasoning.push_str(text);
                    }
                }
            }
            _ => bail!("Responses returned an unsupported output item"),
        }
        // Preserve native IDs, call namespaces and encrypted reasoning for the next HTTP request.
        reply
            .provider_content
            .push(json!({"type":"dial_response_item","item":item}));
    }
    let usage = &response["usage"];
    reply.prompt_tokens = usage["input_tokens"].as_u64();
    reply.completion_tokens = usage["output_tokens"].as_u64();
    reply.cached_prompt_tokens = usage["input_tokens_details"]["cached_tokens"].as_u64();
    reply.reasoning_tokens = usage["output_tokens_details"]["reasoning_tokens"].as_u64();
    Ok(reply)
}
#[derive(Default)]
pub(super) struct Accumulator {
    calls: BTreeMap<u64, (String, String)>,
    added: std::collections::HashSet<u64>,
    finished: BTreeMap<u64, Value>,
    result: Option<Reply>,
}
impl Accumulator {
    pub(super) fn feed(
        &mut self,
        data: &str,
        on_text: &mut dyn FnMut(&str) -> bool,
        on_event: &mut dyn FnMut(&StreamEvent) -> bool,
    ) -> Result<bool> {
        let event: Value = serde_json::from_str(data)
            .map_err(|_| anyhow::anyhow!("invalid Responses stream event"))?;
        match event["type"].as_str() {
            Some("response.output_text.delta" | "response.refusal.delta") => {
                let delta = event["delta"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid Responses text delta"))?;
                if !on_text(delta) {
                    return Err(Interrupted.into());
                }
            }
            Some("response.reasoning_summary_text.delta") => {
                if let Some(delta) = event["delta"].as_str()
                    && !on_event(&StreamEvent::Reasoning(delta.into()))
                {
                    return Err(Interrupted.into());
                }
            }
            Some("response.output_item.added") => {
                let index = event["output_index"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("Responses call has no output index"))?;
                ensure!(
                    self.added.len() < 1024 && self.added.insert(index),
                    "too many or duplicate Responses output items"
                );
                if event["item"]["type"] == "function_call" {
                    ensure!(self.calls.len() < 128, "too many streamed Responses calls");
                    let id = event["item"]["call_id"].as_str().unwrap_or("").to_owned();
                    let name = tool_name(&event["item"])?;
                    ensure!(
                        self.calls.insert(index, (id, name)).is_none(),
                        "duplicate streamed Responses call index"
                    );
                }
            }
            Some("response.output_item.done") => {
                let index = event["output_index"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("Responses item has no output index"))?;
                ensure!(
                    self.added.contains(&index) && event["item"].is_object(),
                    "Responses finished an unknown output item"
                );
                ensure!(
                    self.finished.insert(index, event["item"].clone()).is_none(),
                    "duplicate finished Responses output item"
                );
            }
            Some("response.function_call_arguments.delta") => {
                let index = event["output_index"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("Responses call has no output index"))?;
                let (id, name) = self.calls.get(&index).ok_or_else(|| {
                    anyhow::anyhow!("Responses arguments preceded their function call")
                })?;
                let delta = event["delta"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Responses arguments delta is invalid"))?;
                if !on_event(&StreamEvent::ToolArguments {
                    index,
                    id: id.clone(),
                    name: name.clone(),
                    delta: delta.into(),
                }) {
                    return Err(Interrupted.into());
                }
            }
            Some("response.completed") => {
                let mut response = event["response"].clone();
                // Subscription routing may put complete output only in output_item.done, with an
                // empty terminal output array. Retain those items, but release them only after
                // response.completed and reject any unfinished streamed item.
                if response["output"].as_array().is_some_and(Vec::is_empty)
                    && !self.added.is_empty()
                {
                    ensure!(
                        self.finished.len() == self.added.len(),
                        "Responses completed with unfinished streamed output; no tool calls were executed"
                    );
                    response["output"] = json!(self.finished.values().collect::<Vec<_>>());
                }
                self.result = Some(completed(&response)?);
                return Ok(true);
            }
            Some(kind @ ("response.failed" | "response.incomplete" | "error")) => {
                let code = event
                    .pointer("/response/error/code")
                    .and_then(Value::as_str)
                    .or_else(|| event.pointer("/error/code").and_then(Value::as_str))
                    .or_else(|| event.get("code").and_then(Value::as_str))
                    .unwrap_or("");
                let reason = event
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if kind == "response.incomplete"
                    && reason == "max_output_tokens"
                    && code.is_empty()
                    && let Ok(reply) = self.truncated_reply(&event["response"])
                {
                    self.result = Some(reply);
                    return Ok(true);
                }
                let code = match code {
                    "server_error" => "server_error",
                    "model_error" => "model_error",
                    "websocket_connection_limit_reached" => "websocket_connection_limit_reached",
                    "previous_response_not_found" => "previous_response_not_found",
                    "rate_limit_exceeded" => "rate_limit_exceeded",
                    "insufficient_quota" => "insufficient_quota",
                    "invalid_api_key" => "invalid_api_key",
                    "model_not_found" => "model_not_found",
                    "authentication_error" => "authentication_error",
                    "permission_denied" => "permission_denied",
                    "subscription_sharing_usage_limit_exceeded" => {
                        "subscription_sharing_usage_limit_exceeded"
                    }
                    "subscription_sharing_usage_unavailable" => {
                        "subscription_sharing_usage_unavailable"
                    }
                    _ => match reason {
                        "max_output_tokens" => "max_output_tokens",
                        "content_filter" => "content_filter",
                        _ => "unclassified_failure",
                    },
                };
                let kind = match kind {
                    "response.failed" => "response.failed",
                    "response.incomplete" => "response.incomplete",
                    _ => "error",
                };
                return Err(TerminalFailure { kind, code }.into());
            }
            Some(_) => {}
            None => bail!("Responses stream event has no type"),
        }
        Ok(false)
    }

    /// A response that stopped at its token limit keeps its completed output as a `length` reply.
    /// Any tool call makes it a terminal failure: a truncated turn never executes tools.
    fn truncated_reply(&self, response: &Value) -> Result<Reply> {
        let mut response = response.clone();
        if response["output"].as_array().is_some_and(Vec::is_empty) && !self.finished.is_empty() {
            response["output"] = json!(self.finished.values().collect::<Vec<_>>());
        }
        let mut reply = reply_from_output(&response, true)?;
        ensure!(
            reply.tool_calls.is_empty(),
            "Responses output was truncated with tool calls; no tool calls were executed"
        );
        reply.finish_reason = "length".into();
        Ok(reply)
    }

    pub(super) fn finish(self) -> Result<Reply> {
        self.result.ok_or_else(|| {
            super::stream::IncompleteStream(
                "Responses stream ended before response.completed; no tool calls were executed",
            )
            .into()
        })
    }
}
/// Optional visible-text callback; returning `false` interrupts the stream.
type OnText<'a> = Option<&'a mut dyn FnMut(&str) -> bool>;

/// Responses authentication and routing travel together; account IDs are only sent for Codex.
#[derive(Clone, Copy)]
pub(super) struct Endpoint<'a> {
    pub url: &'a str,
    pub token: &'a str,
    pub account_id: Option<&'a str>,
    pub model: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
}

/// Both transports must verify and stamp the same native-history ownership.
pub(super) fn validate_history(endpoint: Endpoint<'_>, chat: &Chat) -> Result<String> {
    let replay_scope = super::model_config::native_replay_scope(
        "responses",
        endpoint.url,
        None,
        endpoint.account_id,
        Some(endpoint.token),
        endpoint.headers,
    );
    for message in &chat.messages {
        ensure!(
            message["dial_anthropic_compaction"].is_null(),
            "Anthropic compaction state cannot be replayed through Responses; start a clean chat or explicitly compact to a portable summary"
        );
        if let Some(compacted_model) = message["dial_compaction_model"].as_str() {
            ensure!(
                compacted_model == endpoint.model,
                "native compaction state belongs to a different model; start a clean chat"
            );
        }
    }
    for item in chat
        .messages
        .iter()
        .flat_map(|m| m["dial_provider_content"].as_array().into_iter().flatten())
        .filter(|item| item["type"] == "dial_response_item")
    {
        ensure!(
            item["scope"].as_str() == Some(replay_scope.as_str()),
            "Responses native history belongs to an unverified or different account/endpoint; start a clean chat"
        );
    }
    Ok(replay_scope)
}

pub(super) fn stamp_history(reply: &mut Reply, replay_scope: &str) {
    for item in &mut reply.provider_content {
        if item["type"] == "dial_response_item" {
            item["scope"] = json!(replay_scope);
        }
    }
}

#[cfg(test)]
pub(super) fn exchange(
    endpoint: Endpoint<'_>,
    chat: &Chat,
    timeout: u64,
    on_text: &mut OnText<'_>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&super::profiles::ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    exchange_inner(
        endpoint, chat, timeout, on_text, interrupt, profile, on_event, None, None,
    )
}
pub(super) fn exchange_codex(
    endpoint: Endpoint<'_>,
    chat: &Chat,
    timeout: u64,
    on_text: &mut OnText<'_>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&super::profiles::ModelProfile>,
    cache_key: Option<&str>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
    session_key: Option<&str>,
) -> Result<Reply> {
    exchange_inner(
        endpoint,
        chat,
        timeout,
        on_text,
        interrupt,
        profile,
        on_event,
        cache_key,
        session_key,
    )
}
fn exchange_inner(
    endpoint: Endpoint<'_>,
    chat: &Chat,
    timeout: u64,
    on_text: &mut OnText<'_>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&super::profiles::ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
    cache_key: Option<&str>,
    session_key: Option<&str>,
) -> Result<Reply> {
    let Endpoint {
        url,
        token,
        account_id: codex_account_id,
        model,
        headers: custom_headers,
    } = endpoint;
    let replay_scope = validate_history(endpoint, chat)?;
    let mut body = if codex_account_id.is_some() {
        request_codex(model, chat, profile)?
    } else {
        request_api(model, chat, profile)?
    };
    if sends_prompt_cache_key(profile, codex_account_id.is_some())
        && let Some(key) = cache_key
    {
        body["prompt_cache_key"] = json!(key);
    }
    if codex_account_id.is_none() {
        apply_api_prompt_cache(&mut body, profile, url);
    }
    let patience = Patience::for_stream(
        timeout,
        super::host_of(url).is_some_and(|h| super::is_local_host(&h)),
    );
    let routing_hint = format!("model={model}");
    ensure!(
        codex_account_id.is_none() || custom_headers.is_empty(),
        "ChatGPT subscription does not permit custom request headers"
    );
    let mut headers = codex_account_id
        .map(|id| {
            vec![
                ("chatgpt-account-id", id),
                ("OpenAI-Beta", "responses=experimental"),
                ("originator", "dial"),
                ("version", "0.159.0"),
                ("x-codex-routing-hint", routing_hint.as_str()),
            ]
        })
        .unwrap_or_default();
    // The backend derives prompt-cache affinity from the session headers, not `prompt_cache_key`;
    // without them a WebSocket-to-SSE fallback lands on arbitrary replicas and rarely hits cache.
    if let (Some(_), Some(session)) = (codex_account_id, session_key) {
        headers.extend([("session_id", session), ("conversation_id", session)]);
    }
    headers.extend_from_slice(custom_headers);
    let mut response = super::send_with_headers(
        url,
        (!token.is_empty()).then_some(token),
        &body,
        timeout,
        Some(patience.clone()),
        Some(interrupt),
        &headers,
    )?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        // Authorization and plan-usage failures never fall back to separately billed API credentials.
        let bytes = super::read_bounded(response.body_mut().as_reader(), 512)?;
        let delay = super::retry::server_delay(response.headers());
        let error_code = serde_json::from_slice::<Value>(&bytes.data)
            .ok()
            .and_then(|v| {
                v.pointer("/error/code")
                    .or_else(|| v.pointer("/error/type"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        let permanent = match error_code.as_deref() {
            Some("insufficient_quota") => Some("insufficient_quota"),
            Some("billing_hard_limit_reached") => Some("billing_hard_limit_reached"),
            Some("invalid_api_key") => Some("invalid_api_key"),
            Some("permission_denied") => Some("permission_denied"),
            _ => None,
        };
        if let Some(code) = permanent {
            return Err(TerminalFailure {
                kind: "error",
                code,
            }
            .into());
        }
        if serde_json::from_slice::<Value>(&bytes.data)
            .ok()
            .and_then(|body| {
                body.pointer("/error/code")
                    .or_else(|| body.get("code"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .is_some_and(|code| {
                matches!(
                    code.as_str(),
                    "subscription_sharing_usage_limit_exceeded"
                        | "subscription_sharing_usage_unavailable"
                )
            })
        {
            bail!(
                "ChatGPT plan usage is unavailable or its shared allowance is exhausted; review Dial in ChatGPT Settings > Usage"
            );
        }
        if codex_account_id.is_some() && matches!(status, 401 | 403) {
            bail!(
                "ChatGPT sign-in or plan permission expired; Continue with ChatGPT in /providers"
            );
        }
        return Err(super::http_error(status, &bytes, Some(token), delay));
    }
    // Some subscription responses omit Content-Type. The event parser and response.completed
    // remain authoritative; an explicitly different media type is still rejected.
    ensure!(
        response
            .headers()
            .get("content-type")
            .is_none_or(|value| value.to_str().ok().is_some_and(|mime| mime
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("text/event-stream"))),
        "ChatGPT Responses requires an SSE stream"
    );
    let mut accumulator = Accumulator::default();
    super::stream::read_events_with_progress(
        response.body_mut().as_reader(),
        super::MAX_STREAM_BYTES,
        super::stream::MAX_LINE_BYTES,
        patience,
        advances_reply,
        |data| {
            accumulator.feed(
                data,
                &mut |s| on_text.as_deref_mut().is_none_or(|cb| cb(s)),
                on_event,
            )
        },
    )
    .map_err(|e| {
        if e.downcast_ref::<super::stream::StalledStream>().is_some() {
            super::stream::IncompleteStream("Responses made no semantic progress before the idle limit; no tool calls were executed").into()
        } else {
            super::response_error(e, Some(token))
        }
    })?;
    let mut reply = accumulator.finish()?;
    stamp_history(&mut reply, &replay_scope);
    Ok(reply)
}

fn authored_user_content(content: &Value) -> bool {
    let text = content
        .as_str()
        .or_else(|| {
            content
                .as_array()
                .and_then(|parts| parts.iter().find_map(|part| part["text"].as_str()))
        })
        .unwrap_or("")
        .trim_start();
    ![
        "<summary>",
        "<compaction-notes>",
        "[Task focus from dial",
        "[Current task evidence packet;",
        "<environment_context>",
        "<user_instructions>",
        "<additional_context>",
        "<skills",
        "<token_budget>",
        "<model_switch>",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

/// V1 uses the compact endpoint; V2 requires a terminal stream containing exactly one native item.
pub(super) fn compact(
    endpoint: Endpoint<'_>,
    chat: &Chat,
    instructions: &str,
    v2: bool,
    retained_budget: u32,
    timeout: u64,
    interrupt: Arc<AtomicBool>,
    profile: Option<&super::profiles::ModelProfile>,
    cache_key: Option<&str>,
) -> Result<super::provider::NativeCompactionReply> {
    use std::sync::atomic::Ordering;
    let scope = super::model_config::native_replay_scope(
        "responses",
        endpoint.url,
        None,
        endpoint.account_id,
        Some(endpoint.token),
        endpoint.headers,
    );
    for message in &chat.messages {
        ensure!(
            message["dial_anthropic_compaction"].is_null(),
            "Anthropic compaction state cannot be sent to Responses"
        );
        ensure!(
            message["dial_compaction_model"]
                .as_str()
                .is_none_or(|model| model == endpoint.model),
            "Responses compaction state belongs to another model"
        );
        for item in message["dial_provider_content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == "dial_response_item")
        {
            ensure!(
                item["scope"].as_str() == Some(scope.as_str()),
                "Responses history belongs to another account/endpoint; start a clean chat"
            );
        }
    }
    let mut snapshot = chat.clone();
    snapshot
        .messages
        .retain(|m| m["dial_transient"] != true && m["dial_portable_rules"] != true);
    let chat = &snapshot;
    let mut body = if endpoint.account_id.is_some() {
        request_codex(endpoint.model, chat, profile)?
    } else {
        request_api(endpoint.model, chat, profile)?
    };
    if endpoint.account_id.is_none() {
        body["max_output_tokens"] = json!(chat.max_tokens);
    }
    if sends_prompt_cache_key(profile, endpoint.account_id.is_some())
        && let Some(key) = cache_key
    {
        body["prompt_cache_key"] = json!(key);
    }
    let original_instructions = body["instructions"].as_str().unwrap_or("");
    body["instructions"] = json!(if original_instructions.is_empty() {
        instructions.to_owned()
    } else {
        format!("{original_instructions}\n\n{instructions}")
    });
    let retained = if v2 {
        let mut candidates = Vec::new();
        for message in &chat.messages {
            if !message["dial_compaction_model"].is_null() {
                candidates.extend(
                    message["dial_provider_content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|item| {
                            item["type"] == "dial_response_item"
                                && item["item"]["type"] == "message"
                                && item["item"]["role"] == "user"
                                && authored_user_content(&item["item"]["content"])
                        })
                        .map(|item| item["item"].clone()),
                );
            } else if message["role"] == "user"
                && message["dial_transient"] != true
                && message["dial_portable_rules"] != true
                && message["dial_compaction_summary"] != true
                && message["dial_compaction_notes"] != true
                && message["dial_snapcompact"].is_null()
                && message["dial_generated_user"] != true
                && authored_user_content(&message["content"])
            {
                let single = Chat {
                    messages: vec![message.clone()],
                    tools: Vec::new(),
                    temperature: 0.0,
                    max_tokens: chat.max_tokens,
                    seed: None,
                };
                let serialized = request_flat(endpoint.model, &single, None, false)?;
                candidates.extend(
                    serialized["input"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .cloned(),
                );
            }
        }
        let mut remaining = u64::from(retained_budget.min(64_000));
        let mut kept = Vec::new();
        for candidate in candidates.into_iter().rev() {
            let cost = crate::llm::wire::estimate(&candidate);
            if cost > remaining {
                break;
            }
            remaining -= cost;
            kept.push(candidate);
        }
        kept.reverse();
        kept
    } else {
        Vec::new()
    };
    let mut headers = Vec::new();
    let routing = format!("model={}", endpoint.model);
    if let Some(account) = endpoint.account_id {
        ensure!(
            endpoint.headers.is_empty(),
            "ChatGPT compaction does not permit custom headers"
        );
        headers.extend([
            ("chatgpt-account-id", account),
            ("OpenAI-Beta", "responses=experimental"),
            ("originator", "dial"),
            ("version", "0.159.0"),
            ("x-codex-routing-hint", routing.as_str()),
        ]);
        if v2 {
            headers.push(("x-codex-beta-features", "remote_compaction_v2"));
        }
    }
    headers.extend_from_slice(endpoint.headers);
    let output;
    let usage;
    if v2 {
        body["input"]
            .as_array_mut()
            .context("Responses compact input is not an array")?
            .push(json!({"type":"compaction_trigger"}));
        body["stream"] = json!(true);
        body["store"] = json!(false);
        let patience = Patience::for_stream(timeout, false);
        let mut response = super::send_with_headers(
            endpoint.url,
            (!endpoint.token.is_empty()).then_some(endpoint.token),
            &body,
            timeout,
            Some(patience.clone()),
            Some(interrupt.clone()),
            &headers,
        )?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let bytes =
                super::read_bounded(response.body_mut().as_reader(), super::ERROR_EXCERPT_BYTES)?;
            return Err(super::http_error(
                status,
                &bytes,
                Some(endpoint.token),
                super::retry::server_delay(response.headers()),
            ));
        }
        let mut completed = None;
        let mut done = Vec::new();
        super::stream::read_named_events_with_progress(
            response.body_mut().as_reader(),
            super::MAX_STREAM_BYTES,
            super::stream::MAX_LINE_BYTES,
            patience,
            |data, name| {
                advances_reply(data)
                    || matches!(
                        name,
                        Some("response.output_item.done" | "response.completed" | "response.done")
                    )
                    || serde_json::from_str::<Value>(data)
                        .is_ok_and(|event| event["type"] == "response.done")
            },
            |data, name| {
                if data.trim() == "[DONE]" {
                    return Ok(false);
                }
                let event: Value =
                    serde_json::from_str(data).context("invalid V2 compaction stream JSON")?;
                let kind = event["type"].as_str().or(name);
                match kind {
                    Some("response.output_item.done") => {
                        ensure!(
                            event["item"]["type"] == "compaction"
                                || event["item"]["type"] == "compaction_summary",
                            "V2 compaction returned non-compaction output"
                        );
                        done.push(event["item"].clone());
                        ensure!(
                            done.len() <= 1,
                            "V2 compaction returned more than one output item"
                        );
                    }
                    Some("response.completed" | "response.done") => {
                        ensure!(
                            completed.is_none()
                                && (event["response"]["status"] == "completed"
                                    || (kind == Some("response.done")
                                        && event["response"]["status"].is_null())),
                            "V2 compaction did not complete exactly once"
                        );
                        ensure!(
                            done.len() == 1,
                            "V2 compaction completed without exactly one streamed native output item"
                        );
                        completed = Some(event["response"].clone());
                        if interrupt.load(Ordering::SeqCst) {
                            return Err(super::Interrupted.into());
                        }
                        return Ok(true);
                    }
                    Some(kind @ ("response.failed" | "response.incomplete" | "error")) => {
                        let code = match event
                            .pointer("/response/error/code")
                            .and_then(Value::as_str)
                            .or_else(|| event.pointer("/error/code").and_then(Value::as_str))
                            .or_else(|| event.get("code").and_then(Value::as_str))
                            .unwrap_or("")
                        {
                            "server_error" => "server_error",
                            "model_error" => "model_error",
                            "rate_limit_exceeded" => "rate_limit_exceeded",
                            "insufficient_quota" => "insufficient_quota",
                            "invalid_api_key" => "invalid_api_key",
                            "model_not_found" => "model_not_found",
                            "authentication_error" => "authentication_error",
                            "permission_denied" => "permission_denied",
                            _ => "unclassified_failure",
                        };
                        let kind = match kind {
                            "response.failed" => "response.failed",
                            "response.incomplete" => "response.incomplete",
                            _ => "error",
                        };
                        return Err(TerminalFailure { kind, code }.into());
                    }
                    _ => {}
                }
                if interrupt.load(Ordering::SeqCst) {
                    return Err(super::Interrupted.into());
                }
                Ok(false)
            },
        )?;
        let response = completed.ok_or({
            super::stream::IncompleteStream("V2 compaction stream ended before response.completed")
        })?;
        output = done;
        ensure!(
            output.len() == 1,
            "V2 compaction requires exactly one output item"
        );
        usage = response["usage"].clone();
    } else {
        ensure!(
            endpoint.account_id.is_none(),
            "ChatGPT has no automatic V1 compact endpoint"
        );
        let input = body["input"].take();
        let body =
            json!({"model":endpoint.model,"input":input,"instructions":body["instructions"]});
        let url = format!("{}/compact", endpoint.url.trim_end_matches('/'));
        let response = super::provider::compaction::post_json(
            &url,
            (!endpoint.token.is_empty()).then_some(endpoint.token),
            &body,
            timeout,
            interrupt.clone(),
            &headers,
        )?;
        output = response["output"]
            .as_array()
            .cloned()
            .context("V1 compaction response has no output")?;
        usage = response["usage"].clone();
    }
    if interrupt.load(Ordering::SeqCst) {
        return Err(super::Interrupted.into());
    }
    let mut items = if v2 { retained } else { Vec::new() };
    let mut native = 0usize;
    let mut summary = None;
    for mut item in output {
        match item["type"].as_str() {
            Some("compaction") => {
                ensure!(
                    item["encrypted_content"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty()),
                    "Responses compaction has no encrypted state"
                );
                native += 1;
            }
            Some("compaction_summary") => {
                ensure!(
                    item["summary"]
                        .as_str()
                        .or_else(|| item["content"].as_str())
                        .is_some_and(|s| !s.is_empty()),
                    "Responses compaction_summary has no summary state"
                );
                native += 1;
            }
            Some("message") if !v2 => {
                ensure!(
                    matches!(item["role"].as_str(), Some("user" | "assistant")),
                    "V1 compaction returned an invalid message role"
                );
            }
            _ => bail!("Responses compaction returned unsupported output; no tools were executed"),
        }
        if let Some(text) = item["summary"]
            .as_str()
            .or_else(|| item["content"].as_str())
            && !text.trim().is_empty()
        {
            summary = Some(text.to_owned());
        }
        if let Some(object) = item.as_object_mut() {
            object.remove("status");
        }
        items.push(item);
    }
    ensure!(
        native == 1,
        "Responses compaction requires exactly one native compaction item"
    );
    let retained_messages = items
        .iter()
        .filter(|item| item["type"] == "message")
        .count();
    let summary = summary.unwrap_or_else(|| "Provider-native encrypted compaction state; the full immutable conversation remains in the session archive.".to_owned());
    let reply = Reply {
        text: summary.clone(),
        finish_reason: "compaction".into(),
        prompt_tokens: usage["input_tokens"].as_u64(),
        completion_tokens: usage["output_tokens"].as_u64(),
        cached_prompt_tokens: usage["input_tokens_details"]["cached_tokens"].as_u64(),
        reasoning_tokens: usage["output_tokens_details"]["reasoning_tokens"].as_u64(),
        ..Reply::default()
    };
    let wrappers = items
        .into_iter()
        .map(|item| json!({"type":"dial_response_item","item":item,"scope":scope}))
        .collect::<Vec<_>>();
    Ok(super::provider::NativeCompactionReply {
        summary: summary.clone(),
        prefix: vec![
            json!({"role":"assistant","content":summary,"dial_compaction_model":endpoint.model,"dial_provider_content":wrappers}),
        ],
        reply,
        retained_messages,
        notices: Vec::new(),
    })
}

#[cfg(test)]
mod tests;
