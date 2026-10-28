//! Native Gemini GenerateContent SSE. Preserve every returned Part, including empty
//! signature-bearing parts, in durable native replay; only a benign finish commits calls.
use super::profiles::{CacheRetention, ModelProfile, OutputLimit, ReasoningApi, ReasoningEffort};
use super::{Chat, Endpoint, Interrupted, Patience, Reply, StreamEvent, ToolCall};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(crate) const BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const MAX_TOOL_CALLS: usize = 128;
const MAX_PARTS: usize = 65_536;
const POST_FINISH_GRACE: Duration = Duration::from_millis(2500);
static CALL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn call_id() -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "gemini_{stamp}_{}",
        CALL_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

/// Receives a validated base, never a URL with a credential/query appended.
fn request_url(base: &str, model: &str) -> Result<String> {
    // The provider has already enforced the operator's insecure-HTTP policy.
    // Reuse its structural validation even for direct internal callers.
    super::provider::endpoint_url(base, true)?;
    ensure!(
        !base.contains('#'),
        "Gemini endpoint base must not contain a fragment"
    );
    let uri: ureq::http::Uri = base.parse().context("invalid Gemini endpoint base")?;
    let mut base = base.trim_end_matches('/').to_owned();
    if uri.path().is_empty() || uri.path() == "/" {
        base.push_str("/v1beta");
    }
    let model = model.strip_prefix("models/").unwrap_or(model);
    ensure!(!model.trim().is_empty(), "Gemini model must not be empty");
    let mut encoded = String::with_capacity(model.len());
    for byte in model.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(encoded, "%{byte:02X}").expect("writing into a String");
        }
    }
    Ok(format!(
        "{base}/models/{encoded}:streamGenerateContent?alt=sse"
    ))
}

fn content(value: &Value, vision: bool) -> Result<Vec<Value>> {
    if let Some(text) = value.as_str() {
        return Ok(if text.is_empty() {
            Vec::new()
        } else {
            vec![json!({"text":text})]
        });
    }
    if value.is_null() {
        return Ok(Vec::new());
    }
    let blocks = value
        .as_array()
        .context("unsupported Gemini message content")?;
    let mut parts = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                let text = block["text"]
                    .as_str()
                    .context("Gemini text content is not a string")?;
                if !text.is_empty() {
                    parts.push(json!({"text":text}));
                }
            }
            Some("image_url") => {
                ensure!(
                    vision,
                    "this Gemini model profile declares image input unsupported"
                );
                let url = block["image_url"]["url"]
                    .as_str()
                    .context("Gemini image has no URL")?;
                let (mime, data) = url
                    .strip_prefix("data:")
                    .and_then(|url| url.split_once(";base64,"))
                    .context(
                        "Gemini supports inline base64 image data only; attach an image file",
                    )?;
                ensure!(
                    matches!(
                        mime,
                        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                    ),
                    "unsupported Gemini inline image media type"
                );
                ensure!(
                    !data.is_empty()
                        && data.len() <= (super::images::MAX_IMAGE_BYTES as usize).div_ceil(3) * 4,
                    "Gemini inline image is empty or exceeds 5 MiB"
                );
                parts.push(json!({"inlineData":{"mimeType":mime,"data":data}}));
            }
            _ => bail!("unsupported Gemini message content block"),
        }
    }
    Ok(parts)
}

#[derive(Clone)]
struct CallContext {
    name: String,
    wire_id: Option<String>,
}

fn record_calls(
    message: &Value,
    native: Option<&Value>,
    calls: &mut HashMap<String, CallContext>,
) -> Result<()> {
    for call in message["tool_calls"].as_array().into_iter().flatten() {
        let id = call["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .context("Gemini history tool call has no ID")?;
        let name = call["function"]["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .context("Gemini history tool call has no name")?;
        let wire_id = if let Some(native) = native {
            let context = native["calls"]
                .as_array()
                .and_then(|contexts| contexts.iter().find(|context| context["call_id"] == id))
                .context("persisted Gemini tool call has no native context")?;
            let index = context["part_index"]
                .as_u64()
                .and_then(|index| usize::try_from(index).ok())
                .context("invalid persisted Gemini tool Part index")?;
            let function = &native["parts"]
                .as_array()
                .and_then(|parts| parts.get(index))
                .context("persisted Gemini tool Part is missing")?["functionCall"];
            ensure!(
                function["name"] == name,
                "persisted Gemini tool name conflicts with history"
            );
            function["id"].as_str().map(str::to_owned)
        } else {
            None
        };
        ensure!(
            calls
                .insert(
                    id.to_owned(),
                    CallContext {
                        name: name.to_owned(),
                        wire_id
                    }
                )
                .is_none(),
            "Gemini history has duplicate tool call IDs"
        );
    }
    Ok(())
}

fn request(
    endpoint: &Endpoint<'_>,
    req: &Chat,
    profile: Option<&ModelProfile>,
    scope: &str,
) -> Result<Value> {
    if let Some(profile) = profile {
        profile.validate()?;
        ensure!(
            matches!(
                profile.capabilities.reasoning,
                ReasoningApi::None | ReasoningApi::GeminiBudget | ReasoningApi::GeminiLevel
            ),
            "profile reasoning API does not match Gemini"
        );
        ensure!(
            profile.capabilities.output_limit == OutputLimit::MaxTokens,
            "Gemini profiles use max-tokens output limits"
        );
        ensure!(
            profile.local_sampling.is_none(),
            "Gemini does not accept local sampling controls"
        );
        ensure!(
            profile.prompt_cache != Some(true) && profile.cache_retention == CacheRetention::Auto,
            "Gemini does not implement caller-owned context-cache resources; remove prompt_cache/cache_retention overrides"
        );
        ensure!(
            profile.capabilities.tools != Some(false) || req.tools.is_empty(),
            "this Gemini model profile declares tools unsupported"
        );
        if !req.tools.is_empty() {
            ensure!(
                !profile.capabilities.strict_tools,
                "Gemini does not support Dial's strict tool-schema request option"
            );
            ensure!(
                profile.capabilities.parallel_tool_calls.is_none(),
                "Gemini does not support the parallel_tool_calls request option"
            );
        }
    }
    let vision = !profile.is_some_and(|profile| profile.capabilities.vision == Some(false));
    let mut system = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    let mut calls = HashMap::new();
    let mut results = HashSet::new();
    // Tool-result images are emitted after the complete contiguous functionResponse
    // group, which works without guessing whether this exact model supports nested media.
    let mut pending_images = Vec::new();
    for message in &req.messages {
        if message["role"] != "tool" && !pending_images.is_empty() {
            contents.push(json!({"role":"user","parts":std::mem::take(&mut pending_images)}));
        }
        match message["role"].as_str() {
            Some("system" | "developer") => {
                let parts = content(&message["content"], false)?;
                ensure!(
                    parts.iter().all(|part| part["text"].is_string()),
                    "Gemini system instructions must contain text only"
                );
                system.extend(parts);
            }
            Some("user") => {
                let parts = content(&message["content"], vision)?;
                if !parts.is_empty() {
                    contents.push(json!({"role":"user","parts":parts}));
                }
            }
            Some("assistant") => {
                let native_items = message["dial_provider_content"].as_array();
                let native = native_items
                    .filter(|items| {
                        items.len() == 1
                            && items[0]["type"] == "dial_gemini_content"
                            && items[0]["model"] == endpoint.model
                            && items[0]["scope"].as_str() == Some(scope)
                    })
                    .map(|items| &items[0]);
                let has_calls = message["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty())
                    || native_items.is_some_and(|items| {
                        items.iter().any(|item| {
                            item["type"] == "dial_gemini_content"
                                && item["parts"].as_array().is_some_and(|parts| {
                                    parts.iter().any(|part| part.get("functionCall").is_some())
                                })
                        })
                    });
                // Function-call signatures cannot be reconstructed from ordinary
                // history, and unknown models must not get invented bypass signatures.
                // Legacy envelopes without a scope are deliberately unverified.
                ensure!(
                    native.is_some() || !has_calls,
                    "Gemini tool history has no verified replay scope for this endpoint, model, credential and routing headers; start a clean chat before continuing"
                );
                record_calls(message, native, &mut calls)?;
                let parts = if let Some(native) = native {
                    let mut parts = native["parts"]
                        .as_array()
                        .context("invalid persisted Gemini Parts")?
                        .clone();
                    for part in &mut parts {
                        ensure!(part.is_object(), "invalid persisted Gemini Part");
                        // Match shared malformed-call replay, retaining all opaque
                        // context/signatures while presenting the model a repairable object.
                        if let Some(function) = part.get_mut("functionCall") {
                            ensure!(
                                function.is_object(),
                                "invalid persisted Gemini function call"
                            );
                            if function.get("args").is_some_and(|args| !args.is_object()) {
                                function["args"] = json!({});
                            }
                        }
                    }
                    parts
                } else {
                    content(&message["content"], vision)?
                };
                if !parts.is_empty() {
                    contents.push(json!({"role":"model","parts":parts}));
                }
            }
            Some("tool") => {
                let id = message["tool_call_id"]
                    .as_str()
                    .context("Gemini tool result has no call ID")?;
                let context = calls
                    .get(id)
                    .context("Gemini tool result has no matching preceding call")?;
                ensure!(
                    results.insert(id.to_owned()),
                    "Gemini history has duplicate tool results"
                );
                let parts = content(&message["content"], vision)?;
                let mut text = String::new();
                for part in parts {
                    if let Some(piece) = part["text"].as_str() {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(piece);
                    } else {
                        pending_images.push(part);
                    }
                }
                let mut response = json!({"name":context.name,"response":{"output":text}});
                if message["is_error"] == true {
                    response["response"] = json!({"error":text});
                }
                if let Some(id) = &context.wire_id {
                    response["id"] = json!(id);
                }
                let part = json!({"functionResponse":response});
                if let Some(last) = contents.last_mut().filter(|last| {
                    last["role"] == "user"
                        && last["parts"].as_array().is_some_and(|parts| {
                            parts
                                .iter()
                                .all(|part| part["functionResponse"].is_object())
                        })
                }) {
                    last["parts"]
                        .as_array_mut()
                        .expect("checked parts")
                        .push(part);
                } else {
                    contents.push(json!({"role":"user","parts":[part]}));
                }
            }
            _ => bail!("unsupported Gemini message role"),
        }
    }
    if !pending_images.is_empty() {
        contents.push(json!({"role":"user","parts":pending_images}));
    }
    ensure!(!contents.is_empty(), "Gemini requires conversation content");
    let mut generation = json!({"candidateCount":1});
    if req.max_tokens > 0 {
        generation["maxOutputTokens"] = json!(req.max_tokens);
    }
    if profile.is_none_or(|profile| profile.capabilities.temperature) {
        let temperature = profile
            .and_then(|profile| profile.temperature)
            .unwrap_or(req.temperature);
        ensure!(
            temperature.is_finite() && temperature >= 0.0,
            "Gemini temperature must be finite and nonnegative"
        );
        generation["temperature"] = json!(temperature);
    }
    if let Some(top_p) = profile.and_then(|profile| profile.top_p) {
        generation["topP"] = json!(top_p);
    }
    if let Some(seed) = req
        .seed
        .filter(|_| profile.is_none_or(|profile| profile.capabilities.seed))
    {
        ensure!(
            seed <= i32::MAX as u64,
            "Gemini seed must fit a nonnegative 32-bit integer"
        );
        generation["seed"] = json!(seed);
    }
    if let Some(profile) = profile {
        match profile.capabilities.reasoning {
            ReasoningApi::GeminiBudget
                if profile.thinking.is_some() || profile.thinking_budget.is_some() =>
            {
                let mut thinking = json!({"includeThoughts":profile.thinking != Some(false)});
                if profile.thinking == Some(false) {
                    thinking["thinkingBudget"] = json!(0);
                } else if let Some(budget) = profile.thinking_budget {
                    ensure!(
                        budget <= i32::MAX as u32,
                        "Gemini thinking budget must fit a 32-bit integer"
                    );
                    thinking["thinkingBudget"] = json!(budget);
                }
                generation["thinkingConfig"] = thinking;
            }
            ReasoningApi::GeminiLevel
                if profile.thinking.is_some() || profile.reasoning_effort.is_some() =>
            {
                let effort = profile
                    .reasoning_effort
                    .or_else(|| (profile.thinking == Some(false)).then_some(ReasoningEffort::None));
                let mut thinking = json!({"includeThoughts":profile.thinking != Some(false)});
                if let Some(effort) = effort {
                    if effort == ReasoningEffort::None {
                        thinking["thinkingBudget"] = json!(0);
                    } else {
                        thinking["thinkingLevel"] =
                            json!(profile.reasoning_effort_wire(effort).to_ascii_uppercase());
                    }
                }
                generation["thinkingConfig"] = thinking;
            }
            _ => {}
        }
    }
    let mut body = json!({"contents":contents,"generationConfig":generation});
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts":system});
    }
    if let Some(profile) = profile
        && let Some(tier) = profile.request_tiers.service_tier_for(&profile.provider)
    {
        body["serviceTier"] = json!(tier);
    }
    if !req.tools.is_empty() {
        let declarations = req
            .tools
            .iter()
            .map(|tool| {
                ensure!(
                    tool["type"] == "function",
                    "Gemini supports function tools only"
                );
                let function = &tool["function"];
                let name = function["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .context("Gemini tool has no name")?;
                ensure!(
                    function["strict"].is_null() || function["strict"] == false,
                    "Gemini does not support strict function schemas"
                );
                ensure!(
                    function["parameters"].is_object(),
                    "Gemini tool parameters must be a JSON Schema object"
                );
                let mut declaration =
                    json!({"name":name,"parametersJsonSchema":function["parameters"]});
                if let Some(description) = function["description"].as_str() {
                    declaration["description"] = json!(description);
                }
                Ok(declaration)
            })
            .collect::<Result<Vec<_>>>()?;
        body["tools"] = json!([{"functionDeclarations":declarations}]);
    }
    Ok(body)
}

pub(super) fn request_for_proxy(
    endpoint: &Endpoint<'_>,
    chat: &Chat,
    profile: Option<&ModelProfile>,
    scope: &str,
) -> Result<Value> {
    request(endpoint, chat, profile, scope)
}

#[derive(Default)]
pub(super) struct Accumulator {
    reply: Reply,
    native: Vec<Value>,
    native_calls: Vec<Value>,
    candidate_tokens: Option<u64>,
    ids: HashSet<String>,
    terminal: bool,
    execution_id: Option<String>,
}
impl Accumulator {
    pub(super) fn is_terminal(&self) -> bool {
        self.terminal
    }
    #[cfg(test)]
    pub(super) fn execution_id(&self) -> Option<&str> {
        self.execution_id.as_deref()
    }
}
impl Accumulator {
    pub(super) fn feed(
        &mut self,
        data: &str,
        patience: &Patience,
        on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
        on_event: &mut dyn FnMut(&StreamEvent) -> bool,
    ) -> Result<bool> {
        let mut envelope: Value =
            serde_json::from_str(data).context("Gemini stream event is not valid JSON")?;
        ensure!(envelope.is_object(), "Gemini stream event is not an object");
        if let Some(error) = envelope.get("error").filter(|error| !error.is_null()) {
            let status = error["status"].as_str();
            let code = error["code"].as_u64();
            if matches!(
                status,
                Some("INTERNAL" | "UNAVAILABLE" | "DEADLINE_EXCEEDED")
            ) || (status.is_none() && matches!(code, Some(500 | 502 | 503 | 504)))
            {
                return Err(super::stream::IncompleteStream(
                    "Gemini reported a transient server stream failure",
                )
                .into());
            }
            if status == Some("RESOURCE_EXHAUSTED")
                && let Some(delay) = super::retry::body_delay(data.as_bytes())
                && super::errors::rolling_throttle(data, Some(delay))
            {
                let message = error["message"]
                    .as_str()
                    .map(str::trim)
                    .filter(|message| !message.is_empty())
                    .unwrap_or("Gemini rate limit exceeded");
                return Err(super::errors::Classified::new(
                    super::errors::ErrorClass::RateLimited {
                        retry_after: Some(delay),
                    },
                    message.to_owned(),
                )
                .into());
            }
            let status = status.unwrap_or("unknown status");
            let message = error["message"].as_str().map(str::trim).unwrap_or_default();
            bail!("Gemini stream request failed ({status}): {message}");
        }
        let envelope_response_id = envelope
            .get("responseId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut event = match envelope.get_mut("response") {
            Some(response) => {
                self.execution_id = response
                    .get("responseId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(envelope_response_id)
                    .or_else(|| self.execution_id.take());
                std::mem::take(response)
            }
            None => {
                self.execution_id = envelope_response_id.or_else(|| self.execution_id.take());
                envelope
            }
        };
        if let Some(usage) = event.get("usageMetadata") {
            ensure!(usage.is_object(), "invalid Gemini usage metadata");
            self.reply.prompt_tokens = usage["promptTokenCount"]
                .as_u64()
                .or(self.reply.prompt_tokens);
            self.reply.cached_prompt_tokens = usage["cachedContentTokenCount"]
                .as_u64()
                .or(self.reply.cached_prompt_tokens);
            self.reply.reasoning_tokens = usage["thoughtsTokenCount"]
                .as_u64()
                .or(self.reply.reasoning_tokens);
            self.candidate_tokens = usage["candidatesTokenCount"]
                .as_u64()
                .or(self.candidate_tokens);
            if let Some(output) = self.candidate_tokens {
                self.reply.completion_tokens = Some(
                    output
                        .checked_add(self.reply.reasoning_tokens.unwrap_or(0))
                        .context("Gemini output usage overflow")?,
                );
            } else if let (Some(total), Some(prompt)) =
                (usage["totalTokenCount"].as_u64(), self.reply.prompt_tokens)
            {
                self.reply.completion_tokens = total.checked_sub(prompt);
            }
            if self.reply.prompt_tokens.is_none() {
                self.reply.prompt_tokens = usage["totalTokenCount"].as_u64().and_then(|total| {
                    self.reply
                        .completion_tokens
                        .and_then(|output| total.checked_sub(output))
                });
            }
            super::usage::capture_reply(&self.reply);
        }
        if event
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
            .is_some()
        {
            bail!(
                "Gemini blocked the request; review its content and the configured model permissions"
            );
        }
        let Some(candidates) = event.get_mut("candidates") else {
            return Ok(false);
        };
        let candidates = candidates
            .as_array_mut()
            .context("Gemini candidates are not an array")?;
        ensure!(
            candidates.len() <= 1,
            "Gemini returned more than the requested single candidate"
        );
        let Some(candidate) = candidates.first_mut() else {
            return Ok(false);
        };
        ensure!(
            candidate["index"].is_null() || candidate["index"].as_u64() == Some(0),
            "Gemini returned an unexpected candidate index"
        );
        ensure!(
            candidate
                .pointer("/content/role")
                .is_none_or(|role| role == "model"),
            "Gemini returned a non-model candidate"
        );
        let parts = candidate.pointer_mut("/content/parts").map(Value::take);
        if let Some(parts) = parts {
            let Value::Array(parts) = parts else {
                bail!("Gemini response Parts are not an array");
            };
            ensure!(
                !self.terminal || parts.is_empty(),
                "Gemini returned content after its finish reason"
            );
            for part in parts {
                ensure!(part.is_object(), "Gemini response Part is not an object");
                ensure!(
                    self.native.len() < MAX_PARTS,
                    "Gemini reply exceeds the native Part limit"
                );
                ensure!(
                    part.get("inlineData").is_none()
                        && part.get("fileData").is_none()
                        && part.get("executableCode").is_none()
                        && part.get("codeExecutionResult").is_none(),
                    "Gemini returned media or server-executed code unsupported by this text/tool client"
                );
                let mut progress = false;
                if let Some(text) = part.get("text") {
                    let text = text
                        .as_str()
                        .context("Gemini response text is not a string")?;
                    if !text.is_empty() {
                        progress = true;
                        if part["thought"] == true {
                            self.reply.reasoning.push_str(text);
                            if !on_event(&StreamEvent::Reasoning(text.to_owned())) {
                                return Err(Interrupted.into());
                            }
                        } else {
                            self.reply.text.push_str(text);
                            if !on_text.as_deref_mut().is_none_or(|callback| callback(text)) {
                                return Err(Interrupted.into());
                            }
                        }
                    }
                }
                if let Some(function) = part.get("functionCall") {
                    ensure!(
                        self.reply.tool_calls.len() < MAX_TOOL_CALLS,
                        "Gemini reply exceeds the tool-call limit"
                    );
                    let name = function["name"]
                        .as_str()
                        .filter(|name| !name.is_empty())
                        .context("Gemini function call has no name")?
                        .to_owned();
                    let id = if let Some(id) = function.get("id") {
                        id.as_str()
                            .filter(|id| !id.is_empty())
                            .context("invalid Gemini function-call ID")?
                            .to_owned()
                    } else {
                        call_id()
                    };
                    ensure!(
                        self.ids.insert(id.clone()),
                        "Gemini returned duplicate function-call IDs"
                    );
                    let raw = function.get("args").cloned().unwrap_or_else(|| json!({}));
                    let malformed_arguments = !raw.is_object();
                    if !on_event(&StreamEvent::ToolArguments {
                        index: self.reply.tool_calls.len() as u64,
                        id: id.clone(),
                        name: name.clone(),
                        delta: raw.to_string(),
                    }) {
                        return Err(Interrupted.into());
                    }
                    let arguments = if malformed_arguments {
                        Value::Null
                    } else {
                        raw
                    };
                    self.native_calls
                        .push(json!({"part_index":self.native.len(),"call_id":id}));
                    self.reply.tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments,
                        malformed_arguments,
                    });
                    progress = true;
                }
                if part["thoughtSignature"]
                    .as_str()
                    .is_some_and(|signature| !signature.is_empty())
                {
                    progress = true;
                }
                self.native.push(part);
                if progress {
                    patience.mark_event();
                }
            }
        }
        if let Some(reason) = candidate.get("finishReason") {
            let reason = reason.as_str().context("invalid Gemini finish reason")?;
            ensure!(!self.terminal, "Gemini returned duplicate finish reasons");
            self.reply.finish_reason = match reason {
                "STOP" if !self.reply.tool_calls.is_empty() => "tool_calls",
                "STOP" => "stop",
                "MAX_TOKENS" => {
                    ensure!(self.reply.tool_calls.is_empty(),
                        "Gemini output limit interrupted a tool-call turn; increase the output budget before retrying");
                    "length"
                }
                "MALFORMED_FUNCTION_CALL" if self.reply.tool_calls.is_empty() => "tool_calls",
                "MALFORMED_FUNCTION_CALL" => bail!("Gemini terminated with a malformed function call after accepting other calls; the turn was rejected"),
                "UNEXPECTED_TOOL_CALL" => bail!("Gemini called a tool that was not offered in this request; no calls were accepted"),
                "OTHER" => bail!("Gemini terminated with an unspecified error (OTHER); no calls were accepted"),
                "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "RECITATION" | "IMAGE_SAFETY"
                    | "IMAGE_PROHIBITED_CONTENT" | "IMAGE_RECITATION" => bail!("Gemini blocked generated content; no calls were accepted"),
                _ => bail!("Gemini terminated with an unsupported or unsuccessful finish reason; no calls were accepted"),
            }.to_owned();
            self.terminal = true;
            patience.mark_event();
            patience.finish_within(POST_FINISH_GRACE);
        }
        Ok(false)
    }

    pub(super) fn finish(mut self, endpoint: &Endpoint<'_>, scope: String) -> Result<Reply> {
        if !self.terminal {
            return Err(super::stream::IncompleteStream(
                "Gemini stream ended before a candidate finish reason",
            )
            .into());
        }
        let mut carrier = json!({"type":"dial_gemini_content","model":endpoint.model,
            "scope":scope,"parts":self.native,"calls":self.native_calls});
        if let Some(execution_id) = self.execution_id.take() {
            carrier["execution_id"] = json!(execution_id);
        }
        self.reply.provider_content = vec![carrier];
        Ok(self.reply)
    }
}

pub(super) fn exchange(
    endpoint: Endpoint<'_>,
    req: &Chat,
    timeout_secs: u64,
    on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    if interrupt.load(Ordering::SeqCst) {
        return Err(Interrupted.into());
    }
    let url = request_url(endpoint.url, endpoint.model)?;
    exchange_at(
        Endpoint {
            url: &url,
            ..endpoint
        },
        req,
        super::Limits {
            timeout_secs,
            interrupt,
        },
        on_text,
        profile,
        on_event,
    )
}

/// Gemini reports per-minute throttles as 429 `RESOURCE_EXHAUSTED` with a RetryInfo delay. Their
/// text matches the generic usage-limit wording, which would classify them as an account quota and
/// stop the turn instead of backing off. Without a delay this override does not apply, and
/// `errors::rolling_throttle` still classifies per-minute wording as a throttle.
pub(super) fn throttle_error(
    bytes: &super::Bounded,
    hints: super::retry::ServerHints,
) -> Option<anyhow::Error> {
    if bytes.truncated {
        return None;
    }
    let envelope: Value = serde_json::from_slice(&bytes.data).ok()?;
    let error = envelope.get("error")?;
    if error["status"].as_str() != Some("RESOURCE_EXHAUSTED") {
        return None;
    }
    let delay = hints
        .delay
        .or_else(|| super::retry::body_delay(&bytes.data))?;
    if !super::errors::rolling_throttle(&String::from_utf8_lossy(&bytes.data), Some(delay)) {
        return None;
    }
    let message = error["message"]
        .as_str()
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .unwrap_or("Gemini rate limit exceeded");
    Some(
        super::errors::Classified::new(
            super::errors::ErrorClass::RateLimited {
                retry_after: Some(delay),
            },
            message.to_owned(),
        )
        .into(),
    )
}

/// GenerateContent stream against an explicit provider URL and headers (e.g. Vertex ADC).
pub(super) fn exchange_at(
    endpoint: Endpoint<'_>,
    req: &Chat,
    limits: super::Limits,
    on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
    profile: Option<&ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    let super::Limits {
        timeout_secs,
        interrupt,
    } = limits;
    let url = endpoint.url;
    if interrupt.load(Ordering::SeqCst) {
        return Err(Interrupted.into());
    }
    let scope = super::model_config::native_replay_scope(
        "gemini",
        url,
        Some(endpoint.model),
        None,
        endpoint.auth,
        endpoint.headers,
    );
    let body = request(&endpoint, req, profile, &scope)?;
    let patience = Patience::for_stream(
        timeout_secs,
        super::host_of(url).is_some_and(|host| super::is_local_host(&host)),
    );
    let mut headers = endpoint.headers.to_vec();
    if let Some(key) = endpoint.auth {
        headers.push(("x-goog-api-key", key));
    }
    if let Some(key) = endpoint.auth {
        ensure!(
            !key.is_empty() && key.bytes().all(|byte| byte.is_ascii_graphic()),
            "invalid Gemini API key"
        );
    }
    let mut response = super::send_with_headers(
        url,
        None,
        &body,
        timeout_secs,
        Some(patience.clone()),
        Some(Arc::clone(&interrupt)),
        &headers,
    )
    .map_err(|error| {
        if interrupt.load(Ordering::SeqCst) {
            return anyhow::Error::new(Interrupted);
        }
        // This request uses header authentication instead of send's bearer parameter.
        // Keep typed transport retryability while redacting the Google key explicitly.
        match error.downcast::<super::TransportError>() {
            Ok(mut error) => {
                error.message = super::redact(&error.message, endpoint.auth);
                error.into()
            }
            Err(error) => super::response_error(error, endpoint.auth),
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
        if status == 429
            && let Some(error) = throttle_error(&bytes, delay)
        {
            return Err(error);
        }
        return Err(super::http_error(status, &bytes, endpoint.auth, delay));
    }
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
        "Gemini requires a GenerateContent SSE stream"
    );
    let mut accumulator = Accumulator::default();
    let read = super::stream::read_events_with_progress(
        response.body_mut().as_reader(),
        super::MAX_STREAM_BYTES,
        super::stream::MAX_LINE_BYTES,
        patience.clone(),
        |_| false,
        |data| {
            if interrupt.load(Ordering::SeqCst) {
                return Err(Interrupted.into());
            }
            accumulator.feed(data, &patience, on_text, on_event)
        },
    );
    if interrupt.load(Ordering::SeqCst) {
        return Err(Interrupted.into());
    }
    if let Err(error) = read {
        if error.downcast_ref::<super::errors::Classified>().is_some() {
            return Err(error);
        }
        let stalled = error
            .downcast_ref::<super::stream::StalledStream>()
            .is_some();
        let incomplete = error
            .downcast_ref::<super::stream::IncompleteStream>()
            .is_some();
        if !accumulator.terminal || !(stalled || incomplete) {
            if stalled {
                return Err(super::stream::IncompleteStream(
                    "Gemini stream stopped advancing until the idle limit",
                )
                .into());
            }
            return Err(super::response_error(error, endpoint.auth));
        }
    }
    accumulator.finish(&endpoint, scope)
}
#[cfg(test)]
mod tests;
