//! Amazon Bedrock Converse streaming, using the AWS credential chain and SigV4 signer.
use super::aws_credentials::Credentials;
use super::profiles::{CacheRetention, ModelProfile, ReasoningApi};
use super::transports::{OnText, Route};
use super::{Chat, Interrupted, Reply, StreamEvent, ToolCall};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_HEADERS: usize = 128 * 1024;
const MAX_STREAM: usize = 64 * 1024 * 1024;
const MAX_BLOCKS: u64 = 4096;

fn encode(value: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            write!(out, "%{byte:02X}").expect("string write");
        }
    }
    out
}
fn endpoint(route: &Route<'_>) -> Result<(String, String)> {
    super::provider::endpoint_url(route.base_url, route.allow_insecure)?;
    let uri: ureq::http::Uri = route.base_url.parse().context("invalid Bedrock endpoint")?;
    let host = uri.host().context("Bedrock endpoint has no host")?;
    let region = host
        .split('.')
        .find(|part| {
            part.starts_with("us-")
                || part.starts_with("eu-")
                || part.starts_with("ap-")
                || part.starts_with("ca-")
                || part.starts_with("sa-")
                || part.starts_with("me-")
                || part.starts_with("af-")
                || part.starts_with("il-")
                || part.starts_with("mx-")
        })
        .map(str::to_owned)
        .or_else(|| std::env::var("AWS_REGION").ok())
        .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
        .context("Bedrock region must be in endpoint hostname or AWS_REGION")?;
    Ok((
        format!(
            "{}/model/{}/converse-stream",
            route.base_url.trim_end_matches('/'),
            encode(route.model)
        ),
        region,
    ))
}
fn scope(route: &Route<'_>, credentials: Option<&Credentials>) -> String {
    let identity = route
        .account_id
        .or_else(|| credentials.and_then(|c| c.account_id()))
        .or_else(|| credentials.map(|c| c.access_key_id()))
        .or(route.auth)
        .unwrap_or("");
    let mut hash = Sha256::new();
    for part in [route.base_url.trim_end_matches('/'), route.model, identity] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    format!("{:x}", hash.finalize())
}
fn tool_id(id: &str) -> String {
    if !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        id.to_owned()
    } else {
        format!("{:x}", Sha256::digest(id.as_bytes()))
    }
}
fn content_blocks(content: &Value) -> Result<Vec<Value>> {
    if let Some(text) = content.as_str() {
        return Ok(if text.is_empty() {
            vec![]
        } else {
            vec![json!({"text":text})]
        });
    }
    if content.is_null() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for block in content
        .as_array()
        .context("Bedrock content must be text or a block array")?
    {
        match block["type"].as_str() {
            Some("text" | "input_text" | "output_text") => {
                let text = block["text"]
                    .as_str()
                    .context("Bedrock text block missing text")?;
                if !text.is_empty() {
                    out.push(json!({"text":text}));
                }
            }
            Some("image_url" | "input_image") => {
                let url = block
                    .pointer("/image_url/url")
                    .and_then(Value::as_str)
                    .or_else(|| block["image_url"].as_str())
                    .context("Bedrock image missing URL")?;
                let (media, data) = url
                    .strip_prefix("data:")
                    .and_then(|v| v.split_once(","))
                    .context("Bedrock requires an inline base64 image")?;
                let format = match media {
                    "image/jpeg;base64" => "jpeg",
                    "image/png;base64" => "png",
                    "image/gif;base64" => "gif",
                    "image/webp;base64" => "webp",
                    _ => bail!("unsupported Bedrock image media type"),
                };
                ensure!(!data.is_empty(), "Bedrock image is empty");
                out.push(json!({"image":{"format":format,"source":{"bytes":data}}}));
            }
            Some("file" | "input_file") => {
                let file = block.get("file").unwrap_or(block);
                let data = file["file_data"]
                    .as_str()
                    .context("Bedrock document missing file_data")?;
                let (media, bytes) = data
                    .strip_prefix("data:")
                    .and_then(|v| v.split_once(","))
                    .context("Bedrock document requires inline base64 data")?;
                let format = match media {
                    "application/pdf;base64" => "pdf",
                    "text/plain;base64" => "txt",
                    "text/html;base64" => "html",
                    "text/csv;base64" => "csv",
                    "text/markdown;base64" => "md",
                    "application/msword;base64" => "doc",
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document;base64" => {
                        "docx"
                    }
                    "application/vnd.ms-excel;base64" => "xls",
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet;base64" => {
                        "xlsx"
                    }
                    _ => bail!("unsupported Bedrock document media type"),
                };
                let mut name: String = file["filename"]
                    .as_str()
                    .unwrap_or("document")
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric()
                            || matches!(c, ' ' | '-' | '(' | ')' | '[' | ']')
                        {
                            c
                        } else {
                            '-'
                        }
                    })
                    .take(200)
                    .collect();
                if name.trim().is_empty() {
                    name = "document".into();
                }
                out.push(
                    json!({"document":{"format":format,"name":name,"source":{"bytes":bytes}}}),
                );
            }
            other => bail!("unsupported Bedrock content block: {other:?}"),
        }
    }
    Ok(out)
}
fn request(
    model: &str,
    chat: &Chat,
    profile: Option<&ModelProfile>,
    replay_scope: &str,
) -> Result<Value> {
    let mut system = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    for message in &chat.messages {
        let role = message["role"]
            .as_str()
            .context("Bedrock message role missing")?;
        let mut blocks = content_blocks(&message["content"])?;
        if matches!(role, "system" | "developer") {
            ensure!(
                blocks.iter().all(|b| b.get("text").is_some()),
                "Bedrock system content must be text"
            );
            system.extend(blocks);
            continue;
        }
        let wire_role = match role {
            "user" | "tool" => "user",
            "assistant" => "assistant",
            _ => bail!("unsupported Bedrock message role: {role}"),
        };
        if role == "tool" {
            if blocks.is_empty() {
                blocks.push(json!({"json":""}));
            }
            let failed = message["is_error"] == true
                || message["dial_evidence"]["is_error"] == true
                || message["dial_child_tool_result"]["is_error"] == true;
            blocks = vec![
                json!({"toolResult":{"toolUseId":tool_id(message["tool_call_id"].as_str().context("Bedrock tool result missing call ID")?),"content":blocks,"status":if failed { "error" } else { "success" }}}),
            ];
        } else if role == "assistant" {
            let native = message["dial_provider_content"]
                .as_array()
                .and_then(|items| {
                    items
                        .iter()
                        .find(|item| item["type"] == "dial_bedrock_content")
                });
            if let Some(native) = native {
                ensure!(
                    native["scope"].as_str() == Some(replay_scope),
                    "Bedrock native content belongs to another model/account/endpoint; start a clean chat"
                );
                blocks = native["blocks"]
                    .as_array()
                    .context("invalid Bedrock replay blocks")?
                    .clone();
                for block in &mut blocks {
                    if let Some(reasoning) = block.pointer("/reasoningContent/reasoningText")
                        && reasoning["signature"].as_str().is_none_or(str::is_empty)
                    {
                        *block = json!({"text":reasoning["text"]});
                    }
                }
                blocks.retain(|block| {
                    !block["text"]
                        .as_str()
                        .is_some_and(|text| text.trim().is_empty())
                });
            } else if let Some(calls) = message["tool_calls"].as_array() {
                for call in calls {
                    // Bedrock requires an object; a malformed past call must not poison the whole session.
                    let input = match &call["function"]["arguments"] {
                        Value::String(s) => serde_json::from_str::<Value>(s)
                            .ok()
                            .filter(Value::is_object)
                            .unwrap_or_else(|| json!({})),
                        value if value.is_object() => value.clone(),
                        _ => json!({}),
                    };
                    blocks.push(json!({"toolUse":{"toolUseId":tool_id(call["id"].as_str().context("Bedrock history tool call missing ID")?),"name":call["function"]["name"],"input":input}}));
                }
            }
        }
        if blocks.is_empty() {
            continue;
        }
        if let Some(previous) = messages.last_mut().filter(|m| m["role"] == wire_role) {
            previous["content"]
                .as_array_mut()
                .expect("constructed content")
                .extend(blocks);
        } else {
            messages.push(json!({"role":wire_role,"content":blocks}));
        }
    }
    let max_tokens = profile
        .and_then(|p| p.max_tokens)
        .unwrap_or(chat.max_tokens);
    let mut body = json!({"messages":messages,"inferenceConfig":{"maxTokens":max_tokens}});
    // Claude thinks by default when the profile leaves `thinking` unset; see the native builder.
    let default_thinking = profile
        .is_none_or(|p| p.thinking.is_none() && p.top_p.is_none())
        .then(|| super::profiles::default_anthropic_thinking(model))
        .flatten()
        .and_then(|api| match api {
            ReasoningApi::AnthropicAdaptive => Some(json!({"type":"adaptive"})),
            _ => super::profiles::default_thinking_budget(max_tokens)
                .map(|budget| json!({"type":"enabled","budget_tokens":budget})),
        });
    if let Some(thinking) = &default_thinking {
        body["additionalModelRequestFields"] = json!({"thinking":thinking});
    }
    if default_thinking.is_none()
        && profile.is_none_or(|p| p.capabilities.temperature && p.thinking != Some(true))
    {
        body["inferenceConfig"]["temperature"] = json!(
            profile
                .and_then(|p| p.temperature)
                .unwrap_or(chat.temperature)
        );
    }
    if let Some(p) = profile {
        p.validate()?;
        if let Some(top_p) = p.top_p {
            body["inferenceConfig"]["topP"] = json!(top_p);
        }
        if let Some(thinking) = p.thinking {
            let anthropic = matches!(
                p.capabilities.reasoning,
                ReasoningApi::AnthropicBudget | ReasoningApi::AnthropicAdaptive
            );
            ensure!(
                !thinking || anthropic,
                "Bedrock thinking requires a declared Anthropic reasoning API"
            );
            if anthropic {
                body["additionalModelRequestFields"] = json!({"thinking":if !thinking { json!({"type":"disabled"}) } else if p.capabilities.reasoning == ReasoningApi::AnthropicAdaptive { json!({"type":"adaptive"}) } else { json!({"type":"enabled","budget_tokens":p.thinking_budget}) }});
            }
        }
        if let Some(effort) = p.reasoning_effort {
            body["additionalModelRequestFields"]["output_config"] =
                json!({"effort":p.reasoning_effort_wire(effort)});
        }
    }
    if !chat.tools.is_empty() {
        let mut tools = Vec::with_capacity(chat.tools.len());
        for tool in &chat.tools {
            let function = &tool["function"];
            let mut spec =
                json!({"name":function["name"],"inputSchema":{"json":function["parameters"]}});
            if let Some(description) = function["description"].as_str() {
                spec["description"] = json!(description);
            }
            tools.push(json!({"toolSpec":spec}));
        }
        body["toolConfig"] = json!({"tools":tools});
    }
    if let Some(p) =
        profile.filter(|p| p.capabilities.prompt_cache && p.prompt_cache != Some(false))
    {
        let checkpoint = json!({"cachePoint":{"type":"default","ttl":if p.cache_retention == CacheRetention::Long { "1h" } else { "5m" }}});
        if !system.is_empty() {
            system.push(checkpoint.clone());
        }
        if let Some(last) = body["messages"]
            .as_array_mut()
            .and_then(|m| m.last_mut())
            .filter(|m| m["role"] == "user")
        {
            last["content"]
                .as_array_mut()
                .expect("constructed content")
                .push(checkpoint);
        }
    }
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    Ok(body)
}

#[derive(Default)]
struct Headers<'a> {
    kind: Option<&'a str>,
    event: Option<&'a str>,
    exception: Option<&'a str>,
    error: Option<&'a str>,
}
fn parse_headers(mut bytes: &[u8]) -> Result<Headers<'_>> {
    fn take<'a>(bytes: &mut &'a [u8], count: usize) -> Result<&'a [u8]> {
        ensure!(bytes.len() >= count, "truncated Bedrock event header");
        let (part, rest) = bytes.split_at(count);
        *bytes = rest;
        Ok(part)
    }
    let mut out = Headers::default();
    while !bytes.is_empty() {
        let size = take(&mut bytes, 1)?[0] as usize;
        ensure!(size > 0, "empty Bedrock header name");
        let name = std::str::from_utf8(take(&mut bytes, size)?)?;
        let kind = take(&mut bytes, 1)?[0];
        let value = match kind {
            0 | 1 => &[][..],
            2 => take(&mut bytes, 1)?,
            3 => take(&mut bytes, 2)?,
            4 => take(&mut bytes, 4)?,
            5 | 8 => take(&mut bytes, 8)?,
            9 => take(&mut bytes, 16)?,
            6 | 7 => {
                let n = u16::from_be_bytes(take(&mut bytes, 2)?.try_into()?) as usize;
                take(&mut bytes, n)?
            }
            _ => bail!("invalid Bedrock event header type"),
        };
        let slot = match name {
            ":message-type" => Some(&mut out.kind),
            ":event-type" => Some(&mut out.event),
            ":exception-type" => Some(&mut out.exception),
            ":error-code" => Some(&mut out.error),
            _ => None,
        };
        if let Some(slot) = slot {
            ensure!(
                kind == 7 && slot.is_none(),
                "invalid or duplicate Bedrock event header"
            );
            *slot = Some(std::str::from_utf8(value)?);
        }
    }
    Ok(out)
}
fn frame(reader: &mut impl Read, consumed: &mut usize) -> Result<Option<(usize, Vec<u8>)>> {
    let mut prelude = [0; 12];
    loop {
        match reader.read(&mut prelude[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    reader
        .read_exact(&mut prelude[1..])
        .context("truncated Bedrock event prelude")?;
    let length = u32::from_be_bytes(prelude[..4].try_into()?) as usize;
    let hlen = u32::from_be_bytes(prelude[4..8].try_into()?) as usize;
    ensure!(
        (16..=MAX_FRAME).contains(&length) && hlen <= MAX_HEADERS && hlen <= length - 16,
        "invalid Bedrock event frame length"
    );
    ensure!(
        crc32fast::hash(&prelude[..8]) == u32::from_be_bytes(prelude[8..].try_into()?),
        "Bedrock event prelude checksum mismatch"
    );
    *consumed = consumed
        .checked_add(length)
        .context("Bedrock stream size overflow")?;
    ensure!(*consumed <= MAX_STREAM, "Bedrock stream exceeds size limit");
    let mut bytes = vec![0; length - 12];
    reader
        .read_exact(&mut bytes)
        .context("truncated Bedrock event frame")?;
    let mut crc = crc32fast::Hasher::new();
    crc.update(&prelude);
    crc.update(&bytes[..bytes.len() - 4]);
    ensure!(
        crc.finalize() == u32::from_be_bytes(bytes[bytes.len() - 4..].try_into()?),
        "Bedrock event message checksum mismatch"
    );
    bytes.truncate(bytes.len() - 4);
    Ok(Some((hlen, bytes)))
}
enum Content {
    Text(String),
    Reasoning {
        text: String,
        signature: String,
        redacted: Vec<u8>,
    },
    Tool {
        id: String,
        name: String,
        input: String,
    },
}
struct Block {
    content: Content,
    closed: bool,
}
#[derive(Default)]
struct Accumulator {
    blocks: BTreeMap<u64, Block>,
    started: bool,
    stopped: bool,
    reply: Reply,
}
impl Accumulator {
    fn feed(
        &mut self,
        event: &str,
        value: Value,
        on_text: OnText<'_, '_>,
        on_event: &mut dyn FnMut(&StreamEvent) -> bool,
    ) -> Result<bool> {
        if event == "metadata" {
            let Some(usage) = value.get("usage") else {
                return Ok(false);
            };
            self.reply.completion_tokens = usage["outputTokens"].as_u64();
            self.reply.cached_prompt_tokens = usage["cacheReadInputTokens"].as_u64();
            self.reply.cache_creation_tokens = usage["cacheWriteInputTokens"].as_u64();
            self.reply.prompt_tokens = usage["inputTokens"]
                .as_u64()
                .map(|input| {
                    input
                        .checked_add(self.reply.cached_prompt_tokens.unwrap_or(0))
                        .and_then(|total| {
                            total.checked_add(self.reply.cache_creation_tokens.unwrap_or(0))
                        })
                        .context("Bedrock input token count overflow")
                })
                .transpose()?;
            super::usage::capture_reply(&self.reply);
            return Ok(false);
        }
        if !matches!(
            event,
            "messageStart"
                | "messageStop"
                | "contentBlockStart"
                | "contentBlockDelta"
                | "contentBlockStop"
        ) {
            return Ok(false);
        }
        ensure!(!self.stopped, "Bedrock emitted content after messageStop");
        if event == "messageStart" {
            ensure!(
                !self.started && value["role"] == "assistant",
                "invalid Bedrock messageStart"
            );
            self.started = true;
            return Ok(false);
        }
        ensure!(self.started, "Bedrock content before messageStart");
        if event == "messageStop" {
            ensure!(
                self.blocks.values().all(|b| b.closed),
                "Bedrock messageStop before content blocks closed"
            );
            self.reply.finish_reason = match value["stopReason"]
                .as_str()
                .context("Bedrock messageStop missing stop reason")?
            {
                "end_turn" | "stop_sequence" => "stop",
                "tool_use" => "tool_calls",
                "max_tokens" => "length",
                other => other,
            }
            .into();
            self.stopped = true;
            return Ok(false);
        }
        if !matches!(
            event,
            "contentBlockStart" | "contentBlockDelta" | "contentBlockStop"
        ) {
            return Ok(false);
        }
        let index = value["contentBlockIndex"]
            .as_u64()
            .context("Bedrock content index missing")?;
        ensure!(index < MAX_BLOCKS, "Bedrock content block limit exceeded");
        if event == "contentBlockStart" {
            let Some(tool) = value["start"].get("toolUse") else {
                return Ok(false);
            };
            let id = tool["toolUseId"]
                .as_str()
                .context("Bedrock tool start missing ID")?
                .to_owned();
            let name = tool["name"]
                .as_str()
                .context("Bedrock tool start missing name")?
                .to_owned();
            ensure!(
                !id.is_empty() && !name.is_empty() && !self.blocks.contains_key(&index),
                "invalid or duplicate Bedrock tool start"
            );
            self.blocks.insert(
                index,
                Block {
                    content: Content::Tool {
                        id: id.clone(),
                        name: name.clone(),
                        input: String::new(),
                    },
                    closed: false,
                },
            );
            ensure!(
                on_event(&StreamEvent::ToolArguments {
                    index,
                    id,
                    name,
                    delta: String::new()
                }),
                Interrupted
            );
            return Ok(true);
        }
        if event == "contentBlockStop" {
            let Some(block) = self.blocks.get_mut(&index) else {
                // Stops for non-tool starts we ignored have no block to close.
                return Ok(false);
            };
            ensure!(!block.closed, "duplicate Bedrock content stop");
            block.closed = true;
            if let Content::Tool { id, name, input } = &block.content {
                ensure!(
                    on_event(&StreamEvent::ToolCallComplete {
                        index,
                        id: id.clone(),
                        name: name.clone(),
                        arguments: input.clone(),
                    }),
                    Interrupted
                );
            }
            return Ok(false);
        }
        let delta = &value["delta"];
        if let Some(text) = delta["text"].as_str() {
            let block = self.blocks.entry(index).or_insert_with(|| Block {
                content: Content::Text(String::new()),
                closed: false,
            });
            ensure!(!block.closed, "Bedrock delta after content stop");
            let Content::Text(content) = &mut block.content else {
                bail!("Bedrock changed content block type");
            };
            content.push_str(text);
            if let Some(callback) = on_text.as_deref_mut() {
                ensure!(callback(text), Interrupted);
            }
        } else if let Some(reasoning) = delta.get("reasoningContent") {
            let block = self.blocks.entry(index).or_insert_with(|| Block {
                content: Content::Reasoning {
                    text: String::new(),
                    signature: String::new(),
                    redacted: Vec::new(),
                },
                closed: false,
            });
            ensure!(!block.closed, "Bedrock reasoning after content stop");
            let Content::Reasoning {
                text,
                signature,
                redacted,
            } = &mut block.content
            else {
                bail!("Bedrock changed reasoning block type");
            };
            if let Some(chunk) = reasoning["text"].as_str() {
                text.push_str(chunk);
                ensure!(on_event(&StreamEvent::Reasoning(chunk.into())), Interrupted);
            }
            if let Some(chunk) = reasoning["signature"].as_str() {
                signature.push_str(chunk);
            }
            if let Some(chunk) = reasoning["redactedContent"].as_str() {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode_vec(chunk, redacted)
                    .context("invalid Bedrock redacted reasoning encoding")?;
            }
        } else if let Some(input) = delta.pointer("/toolUse/input").and_then(Value::as_str) {
            let block = self
                .blocks
                .get_mut(&index)
                .context("Bedrock tool input before start")?;
            ensure!(!block.closed, "Bedrock tool input after stop");
            let Content::Tool {
                id,
                name,
                input: arguments,
            } = &mut block.content
            else {
                bail!("Bedrock tool delta on non-tool block");
            };
            arguments.push_str(input);
            ensure!(
                on_event(&StreamEvent::ToolArguments {
                    index,
                    id: id.clone(),
                    name: name.clone(),
                    delta: input.into()
                }),
                Interrupted
            );
        } else {
            // Unknown delta kinds (e.g. citations) are ignored rather than aborting the turn.
            return Ok(false);
        }
        Ok(true)
    }
    fn finish(mut self, replay_scope: &str) -> Result<Reply> {
        if !self.stopped {
            return Err(
                super::stream::IncompleteStream("Bedrock stream ended before messageStop").into(),
            );
        }
        let mut native = Vec::with_capacity(self.blocks.len());
        for block in self.blocks.into_values() {
            match block.content {
                Content::Text(text) => {
                    self.reply.text.push_str(&text);
                    native.push(json!({"text":text}));
                }
                Content::Reasoning {
                    text,
                    signature,
                    redacted,
                } => {
                    self.reply.reasoning.push_str(&text);
                    if !redacted.is_empty() {
                        use base64::Engine;
                        native.push(json!({"reasoningContent":{"redactedContent":base64::engine::general_purpose::STANDARD.encode(redacted)}}));
                    } else {
                        native.push(json!({"reasoningContent":{"reasoningText":{"text":text,"signature":signature}}}));
                    }
                }
                Content::Tool { id, name, input } => {
                    // Bedrock sends no input delta for zero-argument tools; that is `{}`, not malformed.
                    let arguments: Value = if input.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&input).unwrap_or(Value::Null)
                    };
                    let malformed_arguments = !arguments.is_object();
                    // Bedrock requires `toolUse.input` to be an object on replay.
                    let native_input = if malformed_arguments {
                        json!({})
                    } else {
                        arguments.clone()
                    };
                    native
                        .push(json!({"toolUse":{"toolUseId":id,"name":name,"input":native_input}}));
                    self.reply.tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments,
                        malformed_arguments,
                    });
                }
            }
        }
        self.reply.provider_content =
            vec![json!({"type":"dial_bedrock_content","scope":replay_scope,"blocks":native})];
        Ok(self.reply)
    }
}
#[allow(clippy::too_many_arguments)]
pub(super) fn exchange(
    route: &Route<'_>,
    chat: &Chat,
    timeout: u64,
    on_text: OnText<'_, '_>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    if interrupt.load(Ordering::SeqCst) {
        return Err(Interrupted.into());
    }
    let (url, region) = endpoint(route)?;
    let credentials = if route.auth.is_none() {
        Some(super::aws_credentials::resolve(
            &region,
            timeout,
            interrupt.clone(),
        )?)
    } else {
        None
    };
    let replay_scope = scope(route, credentials.as_ref());
    let bytes = serde_json::to_vec(&request(route.model, chat, profile, &replay_scope)?)?;
    let mut headers = route.headers.to_vec();
    ensure!(
        !headers.iter().any(|(name, _)| [
            "authorization",
            "x-amz-date",
            "x-amz-security-token",
            "x-amz-content-sha256",
            "content-type",
            "accept"
        ]
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))),
        "Bedrock transport headers cannot be overridden"
    );
    headers.extend([
        ("content-type", "application/json"),
        ("accept", "application/vnd.amazon.eventstream"),
    ]);
    let authorization;
    let signed;
    if let Some(token) = route.auth {
        authorization = format!("Bearer {token}");
        headers.push(("authorization", &authorization));
    } else {
        signed = credentials
            .as_ref()
            .context("Bedrock credentials unavailable")?
            .sign(&region, &url, &headers, &bytes)?;
        headers.extend(
            signed
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        );
    }
    let patience = super::Patience::for_stream(timeout, false);
    let mut response = super::send_prepared_with_headers(
        &url,
        None,
        &bytes,
        timeout,
        Some(patience.clone()),
        Some(interrupt.clone()),
        &headers,
    )?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let delay = super::retry::server_delay(response.headers());
        let body =
            super::read_bounded(response.body_mut().as_reader(), super::ERROR_EXCERPT_BYTES)?;
        return Err(super::http_error(status, &body, route.auth, delay));
    }
    let mut reader = response.body_mut().as_reader();
    let mut consumed = 0;
    let mut state = Accumulator::default();
    loop {
        if interrupt.load(Ordering::SeqCst) {
            return Err(Interrupted.into());
        }
        let (hlen, bytes) = match frame(&mut reader, &mut consumed) {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(error)
                if state.stopped
                    && error.downcast_ref::<std::io::Error>().is_some_and(|e| {
                        matches!(
                            e.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        )
                    }) =>
            {
                break;
            }
            Err(error) => {
                if let Some(io) = error.downcast_ref::<std::io::Error>() {
                    if matches!(
                        io.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) {
                        return Err(super::stream::StalledStream.into());
                    }
                    if matches!(
                        super::stream::disconnect_kind(io),
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::BrokenPipe
                    ) {
                        return Err(super::stream::IncompleteStream(
                            "Bedrock event stream disconnected before completion",
                        )
                        .into());
                    }
                }
                return Err(error);
            }
        };
        let header = parse_headers(&bytes[..hlen])?;
        let payload = &bytes[hlen..];
        if header.kind == Some("exception") || header.kind == Some("error") {
            let name = header
                .exception
                .or(header.error)
                .unwrap_or("unknownException");
            let status = match name {
                "throttlingException" | "ThrottlingException" => 429,
                "serviceUnavailableException" => 503,
                "internalServerException" => 500,
                "modelStreamErrorException" => 503,
                "modelNotReadyException" => 429,
                "modelTimeoutException" => 408,
                "accessDeniedException" => 403,
                "resourceNotFoundException" => 404,
                _ => 400,
            };
            let body = super::read_bounded(payload, super::ERROR_EXCERPT_BYTES)?;
            return Err(super::http_error(status, &body, route.auth, None))
                .with_context(|| format!("Bedrock stream {name}"));
        }
        ensure!(
            header.kind == Some("event"),
            "invalid Bedrock event message type"
        );
        let event = header.event.context("Bedrock event missing event type")?;
        let value = serde_json::from_slice(payload).context("invalid Bedrock event JSON")?;
        if state.feed(event, value, &mut *on_text, on_event)? {
            patience.mark_event();
        }
        if state.stopped {
            patience.finish_within(Duration::from_millis(2500));
        }
    }
    state.finish(&replay_scope)
}

#[cfg(test)]
mod tests;
