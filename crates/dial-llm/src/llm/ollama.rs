//! Native Ollama /api/chat. Bounded incremental NDJSON, semantic idle deadlines,
//! and done:true authorization: partial function calls never escape a failed stream.
use super::profiles::{CacheRetention, ModelProfile, OutputLimit, ReasoningApi, ReasoningEffort};
use super::{Chat, Endpoint, Interrupted, Patience, Reply, StreamEvent, ToolCall};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{ErrorKind, Read},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

pub const BASE_URL: &str = "http://127.0.0.1:11434";
const MAX_TOOL_CALLS: usize = 128;
static CALL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn call_id() -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "ollama_{stamp}_{}",
        CALL_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn request_url(base: &str) -> Result<String> {
    super::provider::endpoint_url(base, true)?;
    ensure!(
        !base.contains('#'),
        "Ollama endpoint base must not contain a fragment"
    );
    let base = base.trim_end_matches('/');
    if base.ends_with("/api/chat") {
        return Ok(base.to_owned());
    }
    if base.ends_with("/api") {
        return Ok(format!("{base}/chat"));
    }
    Ok(format!("{base}/api/chat"))
}

fn content(value: &Value, vision: bool) -> Result<(String, Vec<String>)> {
    if let Some(text) = value.as_str() {
        return Ok((text.to_owned(), Vec::new()));
    }
    if value.is_null() {
        return Ok((String::new(), Vec::new()));
    }
    let blocks = value
        .as_array()
        .context("unsupported Ollama message content")?;
    let mut text = String::new();
    let mut images = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                let piece = block["text"]
                    .as_str()
                    .context("Ollama text content is not a string")?;
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(piece);
            }
            Some("image_url") => {
                ensure!(
                    vision,
                    "this Ollama model profile declares image input unsupported"
                );
                let url = block["image_url"]["url"]
                    .as_str()
                    .context("Ollama image has no URL")?;
                let (mime, data) = url
                    .strip_prefix("data:")
                    .and_then(|url| url.split_once(";base64,"))
                    .context(
                        "Ollama supports inline base64 image data only; attach an image file",
                    )?;
                ensure!(
                    matches!(
                        mime,
                        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                    ),
                    "unsupported Ollama inline image media type"
                );
                ensure!(
                    !data.is_empty()
                        && data.len() <= (super::images::MAX_IMAGE_BYTES as usize).div_ceil(3) * 4,
                    "Ollama inline image is empty or exceeds 5 MiB"
                );
                images.push(data.to_owned());
            }
            _ => bail!("unsupported Ollama message content block"),
        }
    }
    Ok((text, images))
}

fn arguments(raw: &Value) -> Result<Value> {
    let value = if let Some(raw) = raw.as_str() {
        serde_json::from_str::<Value>(raw)
            .context("Ollama history tool arguments are not valid JSON")?
    } else {
        raw.clone()
    };
    ensure!(
        value.is_object(),
        "Ollama history tool arguments must be an object"
    );
    Ok(value)
}

/// `context_window` is the window Dial plans against. Ollama loads a model with a small default
/// context and silently truncates longer prompts, so the window is always requested explicitly
/// (`num_ctx`); a profile's own `context_window` takes precedence.
fn request(
    endpoint: &Endpoint<'_>,
    req: &Chat,
    profile: Option<&ModelProfile>,
    context_window: Option<u32>,
    scope: &str,
) -> Result<Value> {
    ensure!(
        !endpoint.model.trim().is_empty(),
        "Ollama model must not be empty"
    );
    if let Some(profile) = profile {
        profile.validate()?;
        ensure!(
            matches!(
                profile.capabilities.reasoning,
                ReasoningApi::None | ReasoningApi::OllamaThink
            ),
            "profile reasoning API does not match Ollama"
        );
        ensure!(
            profile.capabilities.output_limit == OutputLimit::MaxTokens,
            "Ollama profiles use max-tokens output limits"
        );
        if let Some(local) = &profile.local_sampling {
            local.validate_ollama()?;
        }
        ensure!(
            profile.prompt_cache != Some(true) && profile.cache_retention == CacheRetention::Auto,
            "Ollama does not support explicit prompt_cache/cache_retention request controls"
        );
        ensure!(
            profile.capabilities.tools != Some(false) || req.tools.is_empty(),
            "this Ollama model profile declares tools unsupported"
        );
        if !req.tools.is_empty() {
            ensure!(
                !profile.capabilities.strict_tools,
                "Ollama does not support Dial's strict tool-schema request option"
            );
            ensure!(
                profile.capabilities.parallel_tool_calls.is_none(),
                "Ollama does not support the parallel_tool_calls request option"
            );
        }
    }
    let vision = !profile.is_some_and(|profile| profile.capabilities.vision == Some(false));
    let mut calls = HashMap::new();
    let mut results = HashSet::new();
    let mut messages = Vec::with_capacity(req.messages.len());
    for message in &req.messages {
        let role = message["role"]
            .as_str()
            .context("Ollama history message has no role")?;
        if role == "assistant" {
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                let id = call["id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .context("Ollama history tool call has no ID")?;
                let name = call["function"]["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .context("Ollama history tool call has no name")?;
                ensure!(
                    calls.insert(id.to_owned(), name.to_owned()).is_none(),
                    "Ollama history has duplicate tool-call IDs"
                );
            }
            let native_items = message["dial_provider_content"].as_array();
            let native = native_items.filter(|native| {
                native.len() == 1
                    && native[0]["type"] == "dial_ollama_message"
                    && native[0]["model"] == endpoint.model
                    && native[0]["scope"].as_str() == Some(scope)
            });
            if let Some(native) = native {
                let native = &native[0]["message"];
                ensure!(
                    native.is_object() && native["role"] == "assistant",
                    "invalid persisted Ollama assistant message"
                );
                messages.push(native.clone());
                continue;
            }
            // Unsigned ordinary Ollama tool history is portable, but opaque thinking
            // and native state are not. Legacy envelopes without a scope do not verify.
            let native_has_calls = native_items.is_some_and(|items| {
                items.iter().any(|item| {
                    item["type"] == "dial_ollama_message"
                        && item["message"]["tool_calls"]
                            .as_array()
                            .is_some_and(|calls| !calls.is_empty())
                })
            });
            ensure!(
                !native_has_calls
                    || message["tool_calls"]
                        .as_array()
                        .is_some_and(|calls| !calls.is_empty()),
                "Ollama history has unverified native calls without portable tool history; start a clean chat before continuing"
            );
        }
        let wire_role = match role {
            "system" | "developer" => "system",
            "user" | "assistant" | "tool" => role,
            _ => bail!("unsupported Ollama message role"),
        };
        let (text, images) = content(&message["content"], vision && wire_role != "system")?;
        let mut converted = json!({"role":wire_role,"content":text});
        if !images.is_empty() {
            converted["images"] = json!(images);
        }
        if role == "tool" {
            let id = message["tool_call_id"]
                .as_str()
                .context("Ollama tool result has no call ID")?;
            let name = calls
                .get(id)
                .context("Ollama tool result has no matching preceding call")?;
            ensure!(
                results.insert(id.to_owned()),
                "Ollama history has duplicate tool results"
            );
            converted["tool_name"] = json!(name);
        } else if role == "assistant"
            && let Some(calls) = message["tool_calls"]
                .as_array()
                .filter(|calls| !calls.is_empty())
        {
            converted["tool_calls"] = Value::Array(
                calls
                    .iter()
                    .map(|call| {
                        Ok(
                            json!({"type":"function","function":{"name":call["function"]["name"],
                        "arguments":arguments(&call["function"]["arguments"])?}}),
                        )
                    })
                    .collect::<Result<Vec<_>>>()?,
            );
        }
        messages.push(converted);
    }
    ensure!(
        !messages.is_empty(),
        "Ollama requires conversation messages"
    );
    let mut options = json!({});
    // num_predict:-1 explicitly means unlimited, rather than 0 (no generation).
    options["num_predict"] = if req.max_tokens == 0 {
        json!(-1)
    } else {
        json!(req.max_tokens)
    };
    if profile.is_none_or(|profile| profile.capabilities.temperature) {
        let temperature = profile
            .and_then(|profile| profile.temperature)
            .unwrap_or(req.temperature);
        ensure!(
            temperature.is_finite() && temperature >= 0.0,
            "Ollama temperature must be finite and nonnegative"
        );
        options["temperature"] = json!(temperature);
    }
    if let Some(top_p) = profile.and_then(|profile| profile.top_p) {
        options["top_p"] = json!(top_p);
    }
    if let Some(seed) = req
        .seed
        .filter(|_| profile.is_none_or(|profile| profile.capabilities.seed))
    {
        ensure!(
            seed <= i64::MAX as u64,
            "Ollama seed must fit a nonnegative signed 64-bit integer"
        );
        options["seed"] = json!(seed);
    }
    if let Some(context) = profile
        .and_then(|profile| profile.context_window)
        .or(context_window)
        .filter(|context| *context > 0)
    {
        options["num_ctx"] = json!(context);
    }
    if let Some(local) = profile.and_then(|profile| profile.local_sampling.as_ref()) {
        local.apply_ollama(&mut options);
    }
    let mut body =
        json!({"model":endpoint.model,"messages":messages,"stream":true,"options":options});
    if let Some(keep_alive) = profile.and_then(|profile| profile.keep_alive.as_deref()) {
        body["keep_alive"] = json!(keep_alive);
    }
    if let Some(profile) =
        profile.filter(|profile| profile.capabilities.reasoning == ReasoningApi::OllamaThink)
    {
        if let Some(effort) = profile.reasoning_effort {
            body["think"] = if effort == ReasoningEffort::None {
                json!(false)
            } else {
                json!(effort.as_str())
            };
        } else if let Some(thinking) = profile.thinking {
            body["think"] = json!(thinking);
        }
    }
    if let Some(profile) =
        profile.filter(|p| p.local_server == Some(super::local_hints::LocalServer::Ollama))
    {
        let thinking_decided = profile.thinking.is_some() || profile.reasoning_effort.is_some();
        let mut options = body["options"].take();
        profile
            .effective_local_hints(thinking_decided)
            .apply_ollama(&mut body, &mut options);
        body["options"] = options;
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|tool| {
                    ensure!(
                        tool["type"] == "function",
                        "Ollama supports function tools only"
                    );
                    let function = &tool["function"];
                    let name = function["name"]
                        .as_str()
                        .filter(|name| !name.is_empty())
                        .context("Ollama tool has no name")?;
                    ensure!(
                        function["strict"].is_null() || function["strict"] == false,
                        "Ollama does not support strict function schemas"
                    );
                    ensure!(
                        function["parameters"].is_object(),
                        "Ollama tool parameters must be a JSON Schema object"
                    );
                    let mut converted = json!({"name":name,"parameters":function["parameters"]});
                    if let Some(description) = function["description"].as_str() {
                        converted["description"] = json!(description);
                    }
                    Ok(json!({"type":"function","function":converted}))
                })
                .collect::<Result<Vec<_>>>()?,
        );
    }
    if profile.is_some_and(|profile| {
        profile.tool_repair
            && profile.structured_output() == super::profiles::StructuredOutput::OllamaFormat
    }) && let Ok(schema) = super::tool_repair::schema_for_tools(&req.tools)
    {
        body["format"] = schema;
        body.as_object_mut()
            .expect("request body object")
            .remove("tools");
    }
    Ok(body)
}

struct PartialCall {
    id: String,
    name: String,
    json: String,
    complete: Option<Value>,
}

#[derive(Default)]
struct Accumulator {
    reply: Reply,
    calls: BTreeMap<u64, PartialCall>,
    next_index: u64,
    done: bool,
}

impl Accumulator {
    fn feed(
        &mut self,
        line: &[u8],
        patience: &Patience,
        on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
        on_event: &mut dyn FnMut(&StreamEvent) -> bool,
    ) -> Result<bool> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            return Ok(false);
        }
        let mut event: Value =
            serde_json::from_slice(line).context("Ollama stream line is not valid JSON")?;
        ensure!(event.is_object(), "Ollama stream event is not an object");
        ensure!(!self.done, "Ollama returned data after done");
        if event["done"] == true {
            self.reply.prompt_tokens = event["prompt_eval_count"].as_u64();
            self.reply.cached_prompt_tokens = event["prompt_eval_cached_count"].as_u64();
            self.reply.completion_tokens = event["eval_count"].as_u64();
            super::usage::capture_reply(&self.reply);
        }
        if event.get("error").is_some_and(|error| !error.is_null()) {
            // Native Ollama error strings are not a reliable retry classification.
            // Keep them nonretryable rather than replaying an unknown failure.
            bail!("Ollama stream reported a provider error; no tool calls were accepted");
        }
        let mut advanced = false;
        if let Some(message) = event.get_mut("message") {
            ensure!(
                message.is_object(),
                "Ollama response message is not an object"
            );
            ensure!(
                message["role"].is_null() || message["role"] == "assistant",
                "Ollama returned a non-assistant response message"
            );
            for (field, reasoning) in [("content", false), ("thinking", true)] {
                if let Some(piece) = message.get(field).filter(|v| !v.is_null()) {
                    let piece = piece
                        .as_str()
                        .context("Ollama response text/thinking is not a string")?;
                    if !piece.is_empty() {
                        advanced = true;
                        if reasoning {
                            self.reply.reasoning.push_str(piece);
                            if !on_event(&StreamEvent::Reasoning(piece.to_owned())) {
                                return Err(Interrupted.into());
                            }
                        } else {
                            self.reply.text.push_str(piece);
                            if !on_text
                                .as_deref_mut()
                                .is_none_or(|callback| callback(piece))
                            {
                                return Err(Interrupted.into());
                            }
                        }
                    }
                }
            }
            ensure!(
                message.get("images").is_none_or(
                    |images| images.is_null() || images.as_array().is_some_and(Vec::is_empty)
                ),
                "Ollama returned generated images unsupported by this text/tool client"
            );
            if let Some(calls) = message.get_mut("tool_calls").filter(|c| !c.is_null()) {
                let calls = calls
                    .as_array_mut()
                    .context("Ollama tool_calls is not an array")?;
                for raw in calls {
                    ensure!(
                        raw["type"].is_null() || raw["type"] == "function",
                        "Ollama returned a non-function tool call"
                    );
                    let function = raw
                        .get_mut("function")
                        .filter(|function| function.is_object())
                        .context("Ollama tool call has no function object")?;
                    let index = if let Some(index) = function.get("index") {
                        index
                            .as_u64()
                            .context("Ollama function index is not an unsigned integer")?
                    } else {
                        while self.calls.contains_key(&self.next_index) {
                            self.next_index = self
                                .next_index
                                .checked_add(1)
                                .context("Ollama function index overflow")?;
                        }
                        self.next_index
                    };
                    let new = !self.calls.contains_key(&index);
                    if new {
                        ensure!(
                            self.calls.len() < MAX_TOOL_CALLS,
                            "Ollama reply exceeds the tool-call limit"
                        );
                        let name = function["name"]
                            .as_str()
                            .filter(|name| !name.is_empty())
                            .context("Ollama function call has no name")?;
                        self.calls.insert(
                            index,
                            PartialCall {
                                id: call_id(),
                                name: name.to_owned(),
                                json: String::new(),
                                complete: None,
                            },
                        );
                    }
                    let call = self.calls.get_mut(&index).expect("inserted call");
                    ensure!(
                        call.complete.is_none(),
                        "Ollama repeated a completed tool-call index"
                    );
                    if let Some(name) = function.get("name") {
                        let name = name
                            .as_str()
                            .context("Ollama function name is not a string")?;
                        ensure!(
                            name.is_empty() || name == call.name,
                            "Ollama changed the function name within a call"
                        );
                    }
                    let raw = function
                        .get_mut("arguments")
                        .map(Value::take)
                        .unwrap_or_else(|| json!({}));
                    let delta = if let Value::String(piece) = raw {
                        call.json.push_str(&piece);
                        ensure!(
                            call.json.len() <= super::stream::MAX_LINE_BYTES,
                            "Ollama tool arguments exceed the byte limit"
                        );
                        piece
                    } else {
                        ensure!(
                            call.json.is_empty(),
                            "Ollama mixed partial string and object tool arguments"
                        );
                        let delta = raw.to_string();
                        call.complete = Some(raw);
                        delta
                    };
                    if new || !delta.is_empty() {
                        advanced = true;
                    }
                    if !on_event(&StreamEvent::ToolArguments {
                        index,
                        id: call.id.clone(),
                        name: call.name.clone(),
                        delta,
                    }) {
                        return Err(Interrupted.into());
                    }
                }
            }
        }
        if let Some(done) = event.get("done") {
            let done = done
                .as_bool()
                .context("Ollama done marker is not boolean")?;
            if done {
                let reason = event
                    .get("done_reason")
                    .map(|reason| {
                        reason
                            .as_str()
                            .context("Ollama done_reason is not a string")
                    })
                    .transpose()?;
                self.reply.finish_reason = match reason {
                    None | Some("stop") => if self.calls.is_empty() { "stop" } else { "tool_calls" },
                    Some("tool_calls") => "tool_calls",
                    Some("length") => {
                        ensure!(self.calls.is_empty(),
                            "Ollama output limit interrupted a tool-call turn; increase num_predict before retrying");
                        "length"
                    }
                    Some("load" | "unload") => bail!("Ollama only loaded/unloaded the model instead of generating a reply; include a user turn"),
                    _ => bail!("Ollama terminated with an unsupported done reason; no calls were accepted"),
                }.to_owned();
                // Durations are nanoseconds (Ollama API reference); a zero duration has no rate.
                let rate = |count: Option<u64>, nanos: Option<u64>| {
                    count
                        .zip(nanos.filter(|n| *n > 0))
                        .map(|(count, nanos)| count as f64 * 1e9 / nanos as f64)
                };
                self.reply.load_secs = event["load_duration"].as_u64().map(|n| n as f64 / 1e9);
                self.reply.prefill_tokens_per_second = rate(
                    event["prompt_eval_count"].as_u64(),
                    event["prompt_eval_duration"].as_u64(),
                );
                self.reply.decode_tokens_per_second = rate(
                    event["eval_count"].as_u64(),
                    event["eval_duration"].as_u64(),
                );
                let mut progress = super::ModelProgress::new(super::InferencePhase::Generating);
                progress.prompt_tokens = self.reply.prompt_tokens;
                progress.output_tokens = self.reply.completion_tokens;
                progress.tokens_per_second = event["eval_duration"]
                    .as_u64()
                    .filter(|duration| *duration > 0)
                    .and_then(|duration| {
                        self.reply
                            .completion_tokens
                            .map(|tokens| tokens as f64 * 1_000_000_000.0 / duration as f64)
                    });
                progress.normalize();
                if !on_event(&StreamEvent::Progress(progress)) {
                    return Err(Interrupted.into());
                }
                self.done = true;
                advanced = true;
            }
        }
        if advanced {
            patience.mark_event();
        }
        Ok(self.done)
    }

    fn finish(mut self, endpoint: &Endpoint<'_>, scope: String) -> Result<Reply> {
        if !self.done {
            return Err(
                super::stream::IncompleteStream("Ollama stream ended before done:true").into(),
            );
        }
        let mut native_calls = Vec::with_capacity(self.calls.len());
        for (index, call) in self.calls {
            let value = call
                .complete
                .or_else(|| serde_json::from_str::<Value>(&call.json).ok())
                .unwrap_or(Value::Null);
            let malformed_arguments = !value.is_object();
            native_calls.push(
                json!({"type":"function","function":{"index":index,"name":call.name,
                "arguments":if malformed_arguments { json!({}) } else { value.clone() }}}),
            );
            self.reply.tool_calls.push(ToolCall {
                id: call.id,
                name: call.name,
                arguments: if malformed_arguments {
                    Value::Null
                } else {
                    value
                },
                malformed_arguments,
            });
        }
        let mut native = json!({"role":"assistant","content":self.reply.text});
        if !self.reply.reasoning.is_empty() {
            native["thinking"] = json!(self.reply.reasoning);
        }
        if !native_calls.is_empty() {
            native["tool_calls"] = json!(native_calls);
        }
        self.reply.provider_content =
            vec![json!({"type":"dial_ollama_message","model":endpoint.model,
            "scope":scope,"message":native})];
        Ok(self.reply)
    }
}

/// Read byte chunks rather than lines from BufRead: both total bytes and one
/// unfinished line stay bounded even if a peer never emits a newline.
fn read_ndjson(
    mut reader: impl Read,
    patience: &Patience,
    interrupt: &AtomicBool,
    mut on_line: impl FnMut(&[u8]) -> Result<bool>,
) -> Result<()> {
    let mut chunk = [0u8; 8192];
    let mut line = Vec::new();
    let mut total = 0usize;
    loop {
        if interrupt.load(Ordering::SeqCst) {
            return Err(Interrupted.into());
        }
        let count = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => count,
            Err(_) if interrupt.load(Ordering::SeqCst) => return Err(Interrupted.into()),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                if patience
                    .remaining()
                    .is_some_and(|remaining| remaining.is_zero())
                    || error.kind() == ErrorKind::TimedOut
                    || error.to_string().to_ascii_lowercase().contains("timeout")
                {
                    return Err(super::stream::IncompleteStream(
                        "Ollama stream stopped advancing until the idle limit",
                    )
                    .into());
                }
                if matches!(
                    super::stream::disconnect_kind(&error),
                    ErrorKind::UnexpectedEof
                        | ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe
                ) {
                    return Err(super::stream::IncompleteStream(
                        "Ollama connection disconnected before done:true",
                    )
                    .into());
                }
                return Err(error).context("reading Ollama stream");
            }
        };
        total += count;
        if total > super::MAX_STREAM_BYTES {
            return Err(super::stream::IncompleteStream(
                "Ollama stream exceeds the response byte limit",
            )
            .into());
        }
        for &byte in &chunk[..count] {
            if byte == b'\n' {
                if interrupt.load(Ordering::SeqCst) {
                    return Err(Interrupted.into());
                }
                if on_line(&line)? {
                    return Ok(());
                }
                line.clear();
            } else {
                ensure!(
                    line.len() < super::stream::MAX_LINE_BYTES,
                    "Ollama stream line exceeds the byte limit"
                );
                line.push(byte);
            }
        }
        if patience
            .remaining()
            .is_some_and(|remaining| remaining.is_zero())
        {
            return Err(super::stream::IncompleteStream(
                "Ollama stream stopped advancing until the idle limit",
            )
            .into());
        }
    }
    if interrupt.load(Ordering::SeqCst) {
        return Err(Interrupted.into());
    }
    if !line.is_empty() {
        on_line(&line)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn exchange(
    endpoint: Endpoint<'_>,
    req: &Chat,
    timeout_secs: u64,
    on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&ModelProfile>,
    context_window: Option<u32>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    if interrupt.load(Ordering::SeqCst) {
        return Err(Interrupted.into());
    }
    let url = request_url(endpoint.url)?;
    let scope = super::model_config::native_replay_scope(
        "ollama",
        &url,
        Some(endpoint.model),
        None,
        endpoint.auth,
        endpoint.headers,
    );
    let body = request(&endpoint, req, profile, context_window, &scope)?;
    let patience = Patience::for_stream(
        timeout_secs,
        super::host_of(&url).is_some_and(|host| super::is_local_host(&host)),
    );
    let mut headers = endpoint.headers.to_vec();
    headers.push(("Accept", "application/x-ndjson"));
    let mut response = super::send_with_headers(
        &url,
        endpoint.auth,
        &body,
        timeout_secs,
        Some(patience.clone()),
        Some(Arc::clone(&interrupt)),
        &headers,
    )
    .map_err(|error| {
        if interrupt.load(Ordering::SeqCst) {
            Interrupted.into()
        } else {
            error
        }
    })?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let delay = super::retry::server_delay(response.headers());
        let bytes =
            super::read_bounded(response.body_mut().as_reader(), super::ERROR_EXCERPT_BYTES)
                .map_err(|error| {
                    if interrupt.load(Ordering::SeqCst) {
                        Interrupted.into()
                    } else {
                        super::response_error(error, endpoint.auth)
                    }
                })?;
        return Err(super::http_error(status, &bytes, endpoint.auth, delay));
    }
    ensure!(
        response
            .headers()
            .get("content-type")
            .is_none_or(|value| value.to_str().ok().is_some_and(|mime| matches!(
                mime.split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str(),
                "application/x-ndjson"
                    | "application/json"
                    | "application/jsonl"
                    | "application/ndjson"
            ))),
        "Ollama requires a native JSON/NDJSON chat stream"
    );
    let mut accumulator = Accumulator::default();
    read_ndjson(
        response.body_mut().as_reader(),
        &patience,
        &interrupt,
        |line| accumulator.feed(line, &patience, on_text, on_event),
    )
    .map_err(|error| {
        if interrupt.load(Ordering::SeqCst) {
            Interrupted.into()
        } else {
            super::response_error(error, endpoint.auth)
        }
    })?;
    if interrupt.load(Ordering::SeqCst) {
        return Err(Interrupted.into());
    }
    accumulator.finish(&endpoint, scope)
}

#[cfg(test)]
mod tests;
