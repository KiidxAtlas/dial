//! Anthropic native Messages SSE accumulation. Signed thinking blocks survive tool replay.
use super::{Reply, StreamEvent, Interrupted};
use anyhow::{Result, Context, ensure, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct Accumulator {
    blocks: BTreeMap<u64, Value>,
    arguments: BTreeMap<u64, String>,
    open: std::collections::BTreeSet<u64>,
    usage: Value,
    stop_reason: Option<String>,
    started: bool,
    stopped: bool,
}
impl Accumulator {
    pub(super) fn feed(&mut self, data: &str, on_text: &mut dyn FnMut(&str)->bool, on_event: &mut dyn FnMut(&StreamEvent)->bool) -> Result<bool> {
        let event: Value = serde_json::from_str(data).context("invalid Anthropic stream JSON")?;
        match event["type"].as_str().unwrap_or_default() {
            "ping" => {},
            "error" => bail!("Anthropic reported a stream error ({})", event.pointer("/error/type").and_then(Value::as_str).filter(|s| s.len() <= 80 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')).unwrap_or("unknown")),
            "message_start" => {
                ensure!(!self.started, "duplicate Anthropic message_start");
                self.started = true;
                self.usage = event["message"]["usage"].clone();
            }
            "content_block_start" => {
                ensure!(self.started && !self.stopped, "Anthropic block before message_start");
                let index = event["index"].as_u64().context("Anthropic block has no index")?;
                ensure!(self.blocks.len() < 128 && !self.blocks.contains_key(&index), "duplicate or excessive Anthropic blocks");
                let block = event["content_block"].clone();
                ensure!(block.is_object(), "Anthropic content block is not an object");
                if let Some(text) = block.get("text").and_then(Value::as_str).filter(|s| !s.is_empty()) { if !on_text(text) { return Err(Interrupted.into()); } }
                if let Some(text) = block.get("thinking").and_then(Value::as_str).filter(|s| !s.is_empty()) { if !on_event(&StreamEvent::Reasoning(text.to_owned())) { return Err(Interrupted.into()); } }
                self.blocks.insert(index, block);
                self.open.insert(index);
            }
            "content_block_delta" => {
                let index = event["index"].as_u64().context("Anthropic delta has no index")?;
                ensure!(self.open.contains(&index), "Anthropic delta for closed block");
                let block = self.blocks.get_mut(&index).context("Anthropic delta for unknown block")?;
                let delta = &event["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" | "thinking_delta" | "signature_delta" => {
                        let (field, kind) = match delta["type"].as_str().unwrap() { "text_delta"=>("text","text"),"thinking_delta"=>("thinking","thinking"),_=>("signature","thinking") };
                        ensure!(block["type"] == kind, "Anthropic delta type conflicts with block");
                        let text = delta[field].as_str().context("Anthropic text delta is not a string")?;
                        if block[field].is_null() { block[field] = json!(""); }
                        match &mut block[field] {
                            Value::String(buffer) => buffer.push_str(text),
                            _ => bail!("invalid Anthropic block text"),
                        }
                        if field == "text" && !text.is_empty() && !on_text(text) { return Err(Interrupted.into()); }
                        if field == "thinking" && !text.is_empty() && !on_event(&StreamEvent::Reasoning(text.to_owned())) { return Err(Interrupted.into()); }
                    }
                    "input_json_delta" => {
                        ensure!(block["type"] == "tool_use", "Anthropic input delta outside tool block");
                        let piece = delta["partial_json"].as_str().context("invalid Anthropic tool JSON delta")?;
                        self.arguments.entry(index).or_default().push_str(piece);
                        if !on_event(&StreamEvent::ToolArguments { index,id:block["id"].as_str().unwrap_or_default().to_owned(),name:block["name"].as_str().unwrap_or_default().to_owned(),delta:piece.to_owned() }) { return Err(Interrupted.into()); }
                    }
                    _ => bail!("unsupported Anthropic content delta type"),
                }
            }
            "content_block_stop" => {
                let index = event["index"].as_u64().context("Anthropic block_stop has no index")?;
                ensure!(self.open.remove(&index), "Anthropic stop for unknown or closed block");
                if let Some(raw) = self.arguments.remove(&index) {
                    self.blocks.get_mut(&index).unwrap()["input"] = super::json_repair::parse_arguments(&raw).unwrap_or(Value::Null);
                }
            }
            "message_delta" => {
                ensure!(self.started, "Anthropic message delta before start");
                if let Some(reason) = event["delta"]["stop_reason"].as_str() { self.stop_reason = Some(reason.to_owned()); }
                if let Some(fields) = event["usage"].as_object() {
                    if !self.usage.is_object() { self.usage = json!({}); }
                    for (key, value) in fields { if !value.is_null() { self.usage[key] = value.clone(); } }
                }
            }
            "message_stop" => {
                ensure!(self.started && self.open.is_empty() && self.stop_reason.is_some(), "Anthropic message stopped before blocks or stop reason completed");
                self.stopped = true;
                return Ok(true);
            }
            _ => bail!("unsupported Anthropic stream event type"),
        }
        Ok(false)
    }
    pub(super) fn finish(self) -> Result<Reply> {
        ensure!(self.stopped, "Anthropic stream ended before message_stop (connection closed early)");
        super::provider::anthropic_reply(&json!({"content":self.blocks.into_values().collect::<Vec<_>>(),"stop_reason":self.stop_reason,"usage":self.usage}))
    }
}
