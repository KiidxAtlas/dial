//! Responses protocol adapter for ChatGPT plan usage. HTTP history is supplied in full and every
//! function is namespaced; only a terminal completed response can authorize local tool execution.
use super::{Chat, Interrupted, Patience, Reply, StreamEvent, ToolCall};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicBool},
};

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
        Some("text") => out.push(json!({"type":if assistant { "output_text" } else { "input_text" },"text":block["text"].as_str().unwrap_or("")})),
        Some("image_url") if !assistant => {
            let url=block["image_url"]["url"].as_str().ok_or_else(||anyhow::anyhow!("invalid Responses image"))?;
            let mut image=json!({"type":"input_image","image_url":url});
            if let Some(detail)=block["image_url"]["detail"].as_str() { image["detail"]=json!(detail); }
            out.push(image);
        },
        _=>bail!("unsupported Responses content block"),
    }
    }
    Ok(json!(out))
}
pub(super) fn request(
    model: &str,
    chat: &Chat,
    profile: Option<&super::profiles::ModelProfile>,
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
    let mut body = json!({"model":model,"input":input,"store":false,"stream":true,"include":["reasoning.encrypted_content"]});
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
                    crate::reliability::strict_parameters(&function["parameters"])
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
        profile.validate_request(chat, false)?;
        ensure!(
            profile.temperature.is_none()
                && profile.top_p.is_none()
                && profile.max_tokens.is_none(),
            "ChatGPT plan usage does not support sampling or output-token limit overrides; remove them from this model profile"
        );
        if let Some(effort) = profile.reasoning_effort.or_else(|| {
            (profile.thinking == Some(false)).then_some(super::profiles::ReasoningEffort::None)
        }) {
            body["reasoning"] = json!({"effort":effort.as_str()});
        }
    }
    Ok(body)
}
fn request_codex(
    model: &str,
    chat: &Chat,
    profile: Option<&super::profiles::ModelProfile>,
) -> Result<Value> {
    let mut body = request(model, chat, profile)?;
    if let Some(input) = body["input"].as_array_mut() {
        for item in input {
            if item["type"] == "function_call" {
                item.as_object_mut().unwrap().remove("namespace");
            }
        }
    }
    if let Some(tools) = body["tools"]
        .as_array()
        .and_then(|tools| tools.first())
        .filter(|tool| tool["type"] == "namespace")
    {
        body["tools"] = tools["tools"].clone();
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
                        && item.get("status").is_none_or(|s| s == "completed"),
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
struct Accumulator {
    calls: BTreeMap<u64, (String, String)>,
    added: std::collections::HashSet<u64>,
    finished: BTreeMap<u64, Value>,
    result: Option<Reply>,
}
impl Accumulator {
    fn feed(
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
                    .or_else(|| event.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if matches!(
                    code,
                    "subscription_sharing_usage_limit_exceeded"
                        | "subscription_sharing_usage_unavailable"
                ) {
                    bail!(
                        "ChatGPT plan usage is unavailable or its shared allowance is exhausted; review Dial in ChatGPT Settings > Usage"
                    );
                }
                // Only surface protocol identifiers. Server messages can contain credentials or prompt content.
                let reason = event
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let detail = if !code.is_empty()
                    && code
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    format!(" (code: {})", &code[..code.len().min(80)])
                } else if !reason.is_empty()
                    && reason
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    format!(" (reason: {})", &reason[..reason.len().min(80)])
                } else {
                    String::new()
                };
                bail!("ChatGPT Responses {kind} event; no tool calls were executed{detail}");
            }
            Some(_) => {}
            None => bail!("Responses stream event has no type"),
        }
        Ok(false)
    }
    fn finish(self) -> Result<Reply> {
        self.result.ok_or_else(|| {
            anyhow::anyhow!(
                "Responses stream ended before response.completed; no tool calls were executed"
            )
        })
    }
}
#[cfg(test)]
pub(super) fn exchange(
    url: &str,
    token: &str,
    model: &str,
    chat: &Chat,
    timeout: u64,
    on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&super::profiles::ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    exchange_inner(
        url, token, None, model, chat, timeout, on_text, interrupt, profile, on_event,
    )
}
pub(super) fn exchange_codex(
    url: &str,
    token: &str,
    account_id: &str,
    model: &str,
    chat: &Chat,
    timeout: u64,
    on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&super::profiles::ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    exchange_inner(
        url,
        token,
        Some(account_id),
        model,
        chat,
        timeout,
        on_text,
        interrupt,
        profile,
        on_event,
    )
}
fn exchange_inner(
    url: &str,
    token: &str,
    codex_account_id: Option<&str>,
    model: &str,
    chat: &Chat,
    timeout: u64,
    on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&super::profiles::ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    ensure!(timeout > 0, "Responses timeout must be positive");
    let body = if codex_account_id.is_some() {
        request_codex(model, chat, profile)?
    } else {
        request(model, chat, profile)?
    };
    let patience = Patience::for_stream(
        timeout,
        super::host_of(url).is_some_and(|h| super::is_local_host(&h)),
    );
    let routing_hint = format!("model={model}");
    let headers = codex_account_id
        .map(|id| {
            vec![
                ("chatgpt-account-id", id),
                ("OpenAI-Beta", "responses=experimental"),
                ("originator", "omp"),
                ("version", "0.159.0"),
                ("x-codex-routing-hint", routing_hint.as_str()),
            ]
        })
        .unwrap_or_default();
    let mut response = super::send_with_headers(
        url,
        Some(token),
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
        if matches!(status, 401 | 403) {
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
    super::stream::read_events(
        response.body_mut().as_reader(),
        super::MAX_RESPONSE_BYTES,
        super::stream::MAX_LINE_BYTES,
        patience,
        |data| {
            accumulator.feed(
                data,
                &mut |s| on_text.as_deref_mut().is_none_or(|cb| cb(s)),
                on_event,
            )
        },
    )
    .map_err(|e| super::response_error(e, Some(token)))?;
    accumulator.finish()
}

#[cfg(test)]
mod tests {
    use super::super::fake::{self, Script};
    use super::*;
    fn chat() -> Chat {
        Chat {
            messages: vec![
                json!({"role":"system","content":"system guidance"}),
                json!({"role":"user","content":"write marker"}),
            ],
            tools: vec![
                json!({"type":"function","function":{"name":"write","description":"write a file","parameters":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}}),
            ],
            temperature: 0.2,
            max_tokens: 2048,
            seed: Some(42),
        }
    }
    fn call() -> Value {
        json!({"type":"function_call","id":"item_write","status":"completed","namespace":"dial","call_id":"call_write","name":"write","arguments":"{\"path\":\"marker.txt\",\"content\":\"ok\"}"})
    }
    fn response(output: Vec<Value>) -> Value {
        json!({"status":"completed","output":output,"usage":{"input_tokens":50,"output_tokens":12,"input_tokens_details":{"cached_tokens":20},"output_tokens_details":{"reasoning_tokens":3}}})
    }
    fn sse(events: Vec<Value>) -> String {
        events
            .into_iter()
            .map(|v| format!("data: {v}\n\n"))
            .collect()
    }
    #[test]
    fn responses_send_only_advertised_max_and_ultra_reasoning_levels() {
        use super::super::profiles::{
            ModelCapabilities, ModelProfile, ReasoningApi, ReasoningEffort,
        };
        for effort in [
            ReasoningEffort::High,
            ReasoningEffort::Max,
            ReasoningEffort::Ultra,
        ] {
            let mut profile = ModelProfile {
                provider: "chatgpt".into(),
                model: "account-model".into(),
                reasoning_effort: Some(effort),
                capabilities: ModelCapabilities {
                    reasoning: ReasoningApi::OpenaiResponses,
                    reasoning_efforts: vec![effort],
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(
                request("account-model", &chat(), Some(&profile)).unwrap()["reasoning"]["effort"],
                effort.as_str()
            );
            profile.capabilities.reasoning_efforts.clear();
            assert!(request("account-model", &chat(), Some(&profile)).is_err());
        }
    }
    #[test]
    fn requests_use_namespaces_full_history_and_only_supported_parameters() {
        let body = request("account-model", &chat(), None).unwrap();
        assert_eq!(body["model"], "account-model");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["tools"][0]["type"], "namespace");
        assert_eq!(body["tools"][0]["name"], "dial");
        assert_eq!(body["tools"][0]["tools"][0]["name"], "write");
        assert_eq!(body["tools"][0]["tools"][0]["strict"], false);
        for field in [
            "temperature",
            "max_output_tokens",
            "max_tokens",
            "seed",
            "previous_response_id",
            "metadata",
            "conversation",
            "background",
        ] {
            assert!(body.get(field).is_none(), "unexpected {field}");
        }
    }
    #[test]
    fn tool_results_and_encrypted_reasoning_roundtrip_after_session_resume() {
        let reasoning = json!({"type":"reasoning","id":"reasoning_id","summary":[],"encrypted_content":"opaque-encrypted-evidence"});
        let reply = completed(&response(vec![reasoning.clone(), call()])).unwrap();
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout =
            crate::storage::paths::Layout::with_user_dir(project.path(), home.path().to_owned())
                .unwrap();
        let mut session = crate::storage::session::Session::create(
            &layout,
            crate::storage::session::SessionMeta {
                model: "account-model".into(),
            },
        )
        .unwrap();
        session
            .append(super::super::assistant_message(&reply))
            .unwrap();
        session
            .append(json!({"role":"tool","tool_call_id":"call_write","content":"wrote marker"}))
            .unwrap();
        let path = session.path().unwrap().to_owned();
        drop(session);
        let session = crate::storage::session::Session::open(&path).unwrap();
        let mut c = chat();
        c.messages.extend(session.messages());
        let body = request("account-model", &c, None).unwrap();
        assert_eq!(body["input"][2], reasoning);
        assert_eq!(body["input"][3], call());
        assert_eq!(
            body["input"][4],
            json!({"type":"function_call_output","call_id":"call_write","output":"wrote marker"})
        );
        assert_eq!(reply.tool_calls[0].name, "write");
        assert_eq!(reply.prompt_tokens, Some(50));
        assert_eq!(reply.cached_prompt_tokens, Some(20));
        assert_eq!(reply.reasoning_tokens, Some(3));
    }
    #[test]
    fn images_convert_to_responses_input_without_sending_bookkeeping() {
        let mut c = chat();
        c.messages[1] = json!({"role":"user","content":[{"type":"text","text":"look"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA==","detail":"low"}}],"dial_evidence":{"secret":"never sent"}});
        let body = request("account-model", &c, None).unwrap();
        assert_eq!(body["input"][1]["content"][1]["type"], "input_image");
        assert_eq!(body["input"][1]["content"][1]["detail"], "low");
        assert!(!body.to_string().contains("never sent"));
    }
    #[test]
    fn empty_terminal_output_replays_finished_items_only_after_successful_completion() {
        let message = json!({"type":"message","id":"message_id","role":"assistant","status":"completed","content":[{"type":"output_text","text":"OK","annotations":[]}]});
        let events = vec![
            json!({"type":"response.output_item.added","output_index":0,"item":message}),
            json!({"type":"response.output_item.done","output_index":0,"item":message}),
            json!({"type":"response.output_item.added","output_index":1,"item":call()}),
            json!({"type":"response.output_item.done","output_index":1,"item":call()}),
        ];
        let mut accumulator = Accumulator::default();
        for event in &events {
            accumulator
                .feed(&event.to_string(), &mut |_| true, &mut |_| true)
                .unwrap();
        }
        assert!(
            accumulator.finish().is_err(),
            "done items alone cannot release tools"
        );
        let mut full = events.clone();
        full.push(json!({"type":"response.completed","response":response(vec![])}));
        let server = fake::serve(vec![Script::SseNoContentType(sse(full))]);
        let reply = exchange(
            &server.url,
            "fixture-token",
            "account-model",
            &chat(),
            5,
            &mut None,
            Arc::new(AtomicBool::new(false)),
            None,
            &mut |_| true,
        )
        .unwrap();
        assert_eq!(reply.text, "OK");
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.provider_content.len(), 2);
        server.requests();
        let mut failed = events;
        failed.push(json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_unavailable"}}}));
        let server = fake::serve(vec![Script::SseNoContentType(sse(failed))]);
        assert!(
            exchange(
                &server.url,
                "fixture-token",
                "account-model",
                &chat(),
                5,
                &mut None,
                Arc::new(AtomicBool::new(false)),
                None,
                &mut |_| true
            )
            .is_err()
        );
        server.requests();
        let server = fake::serve(vec![Script::Sse(sse(vec![
            json!({"type":"response.output_item.added","output_index":0,"item":call()}),
            json!({"type":"response.completed","response":response(vec![])}),
        ]))]);
        assert!(
            exchange(
                &server.url,
                "fixture-token",
                "account-model",
                &chat(),
                5,
                &mut None,
                Arc::new(AtomicBool::new(false)),
                None,
                &mut |_| true
            )
            .is_err()
        );
        server.requests();
    }
    #[test]
    fn valid_responses_streams_can_omit_content_type_but_json_cannot_release_tools() {
        let payload = sse(vec![
            json!({"type":"response.completed","response":response(vec![call()])}),
        ]);
        let server = fake::serve(vec![Script::SseNoContentType(payload)]);
        let reply = exchange(
            &server.url,
            "fixture-token",
            "account-model",
            &chat(),
            5,
            &mut None,
            Arc::new(AtomicBool::new(false)),
            None,
            &mut |_| true,
        )
        .unwrap();
        assert_eq!(reply.tool_calls.len(), 1);
        server.requests();
        let server = fake::serve(vec![Script::SseNoContentType(
            response(vec![call()]).to_string(),
        )]);
        assert!(
            exchange(
                &server.url,
                "fixture-token",
                "account-model",
                &chat(),
                5,
                &mut None,
                Arc::new(AtomicBool::new(false)),
                None,
                &mut |_| true
            )
            .is_err()
        );
        server.requests();
    }
    #[test]
    fn completed_stream_yields_tools_and_partial_stream_never_does() {
        let events = vec![
            json!({"type":"response.output_item.added","output_index":0,"item":call()}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":"}),
        ];
        let mut accumulator = Accumulator::default();
        let mut seen = Vec::new();
        for event in &events {
            accumulator
                .feed(&event.to_string(), &mut |_| true, &mut |e| {
                    seen.push(e.clone());
                    true
                })
                .unwrap();
        }
        assert!(accumulator.finish().is_err());
        assert!(matches!(&seen[0],StreamEvent::ToolArguments { name,.. } if name=="write"));
        let mut events = events;
        events.push(json!({"type":"response.completed","response":response(vec![call()])}));
        let server = fake::serve(vec![Script::Sse(sse(events))]);
        let reply = exchange(
            &server.url,
            "test-access",
            "account-model",
            &chat(),
            5,
            &mut None,
            Arc::new(AtomicBool::new(false)),
            None,
            &mut |_| true,
        )
        .unwrap();
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].arguments["path"], "marker.txt");
        let requests = server.requests();
        assert_eq!(
            fake::header(&requests[0], "authorization").as_deref(),
            Some("Bearer test-access")
        );
        let body = fake::body(&requests[0]);
        assert_eq!(body["store"], false);
    }
    #[test]
    fn late_usage_limit_failures_and_incomplete_responses_do_not_release_calls() {
        for terminal in [
            json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"secret prompt"}}}),
            json!({"type":"response.incomplete","response":{"status":"incomplete","output":[call()]}}),
        ] {
            let server = fake::serve(vec![Script::Sse(sse(vec![
                json!({"type":"response.output_item.added","output_index":0,"item":call()}),
                terminal,
            ]))]);
            let err = exchange(
                &server.url,
                "test-access",
                "account-model",
                &chat(),
                5,
                &mut None,
                Arc::new(AtomicBool::new(false)),
                None,
                &mut |_| true,
            )
            .unwrap_err();
            assert!(!err.to_string().contains("secret prompt"));
            server.requests();
        }
    }
    #[test]
    fn failed_events_expose_only_safe_diagnostic_identifiers() {
        for (terminal, expected) in [
            (
                json!({"type":"response.failed","response":{"error":{"code":"model_not_found","message":"private prompt"}}}),
                "code: model_not_found",
            ),
            (
                json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"error":{"message":"private prompt"}}}),
                "reason: max_output_tokens",
            ),
        ] {
            let mut accumulator = Accumulator::default();
            let err = accumulator
                .feed(&terminal.to_string(), &mut |_| true, &mut |_| true)
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{err}");
            assert!(!err.contains("private prompt"), "{err}");
        }
    }

    #[test]
    fn stream_callbacks_and_transport_are_cancellable() {
        let server = fake::serve(vec![Script::Sse(sse(vec![
            json!({"type":"response.output_text.delta","delta":"hello"}),
            json!({"type":"response.completed","response":response(vec![])}),
        ]))]);
        let mut callback = |_: &str| false;
        assert!(
            exchange(
                &server.url,
                "test-access",
                "account-model",
                &chat(),
                5,
                &mut Some(&mut callback),
                Arc::new(AtomicBool::new(false)),
                None,
                &mut |_| true
            )
            .unwrap_err()
            .downcast_ref::<Interrupted>()
            .is_some()
        );
        server.requests();
        let server = fake::serve(vec![Script::Stall {
            silence: std::time::Duration::from_millis(300),
        }]);
        let interrupt = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&interrupt);
        let start = std::time::Instant::now();
        let job = std::thread::spawn(move || {
            exchange(
                &server.url,
                "test-access",
                "account-model",
                &chat(),
                5,
                &mut None,
                interrupt,
                None,
                &mut |_| true,
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(job.join().unwrap().is_err());
        assert!(start.elapsed() < std::time::Duration::from_millis(250));
    }
    #[test]
    fn malformed_arguments_reach_the_existing_repair_path_but_unfinished_calls_fail() {
        let mut invalid = call();
        invalid["arguments"] = json!("{broken");
        assert!(
            completed(&response(vec![invalid.clone()]))
                .unwrap()
                .tool_calls[0]
                .malformed_arguments
        );
        invalid["status"] = json!("in_progress");
        assert!(completed(&response(vec![invalid])).is_err());
        let mut foreign = call();
        foreign["namespace"] = json!("other-app");
        assert!(completed(&response(vec![foreign])).is_err());
        assert!(completed(&response(vec![call(), call()])).is_err());
    }
    #[test]
    fn auth_errors_and_redirects_cannot_leak_tokens_to_a_new_origin() {
        let server = fake::serve(vec![
            fake::json(401, &json!({"detail":"test-access"})),
            Script::HttpHeaders {
                status: 307,
                headers: vec![("Location", "https://evil.example".into())],
                body: vec![],
            },
        ]);
        for _ in 0..2 {
            let err = exchange(
                &server.url,
                "test-access",
                "account-model",
                &chat(),
                5,
                &mut None,
                Arc::new(AtomicBool::new(false)),
                None,
                &mut |_| true,
            )
            .unwrap_err();
            assert!(!err.to_string().contains("test-access"));
        }
        assert_eq!(server.requests().len(), 2);
    }
}
