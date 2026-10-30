//! Explicit paid native API batches. No retries, subscription routing, proxies or automatic polling.
use super::auth::SecretToken;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    io::Read,
    path::Path,
    time::Duration,
};

pub const MAX_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_ITEMS: usize = 1000;
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Openai,
    Anthropic,
}
impl Provider {
    pub fn base(self) -> &'static str {
        match self {
            Self::Openai => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com/v1",
        }
    }
    fn route(self) -> &'static str {
        match self {
            Self::Openai => "/batches",
            Self::Anthropic => "/messages/batches",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub custom_id: String,
    pub prompt: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub provider: Provider,
    pub model: String,
    pub max_tokens: u32,
    pub items: Vec<Item>,
}
impl Plan {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.model.is_empty() && self.model.len() <= 200,
            "invalid batch model"
        );
        ensure!(
            (1..=16384).contains(&self.max_tokens),
            "invalid output token bound"
        );
        ensure!(
            !self.items.is_empty() && self.items.len() <= MAX_ITEMS,
            "batch item count exceeds bounds"
        );
        let mut ids = HashSet::new();
        for item in &self.items {
            validate_id(&item.custom_id)?;
            ensure!(
                self.provider != Provider::Anthropic || item.custom_id.len() <= 64,
                "Anthropic custom_id must be at most 64 bytes"
            );
            ensure!(ids.insert(&item.custom_id), "duplicate batch item id");
            ensure!(!item.prompt.trim().is_empty(), "empty batch prompt");
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_BYTES,
            "batch input exceeds byte bound"
        );
        Ok(())
    }
    fn params(&self, item: &Item) -> Value {
        json!({"model": self.model, "max_tokens": self.max_tokens, "messages": [{"role":"user", "content":item.prompt}]})
    }
}
fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
        "invalid batch identity"
    );
    Ok(())
}

pub struct Client {
    provider: Provider,
    base: String,
    key: SecretToken,
    agent: ureq::Agent,
}
impl Client {
    pub fn resolve(provider: Provider, user_dir: &Path) -> Result<Self> {
        let key = super::catalog::resolve_credential(user_dir, provider.base(), None)?
            .context("paid API key required for native batches")?;
        // OAuth/subscription credentials must never be sent to a paid batch endpoint.
        ensure!(
            match provider {
                Provider::Openai => key.starts_with("sk-") && !key.starts_with("sk-ant-"),
                Provider::Anthropic => key.starts_with("sk-ant-api"),
            },
            "native batches require a paid API key, not subscription credentials"
        );
        Ok(Self::new(provider, provider.base().to_owned(), key))
    }
    fn new(provider: Provider, base: String, key: String) -> Self {
        Self {
            provider,
            base,
            key: SecretToken::new(key),
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(60)))
                .max_redirects(0)
                .http_status_as_error(false)
                .build()
                .into(),
        }
    }
    fn request(
        &self,
        method: &str,
        path: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<Vec<u8>> {
        ensure!(
            body.len() <= MAX_BYTES,
            "batch wire payload exceeds byte bound"
        );
        let url = format!("{}{path}", self.base);
        let auth = match self.provider {
            Provider::Openai => format!("Bearer {}", self.key.as_str()),
            Provider::Anthropic => self.key.as_str().to_owned(),
        };
        let header = match self.provider {
            Provider::Openai => "Authorization",
            Provider::Anthropic => "x-api-key",
        };
        let mut response = if method == "GET" {
            self.agent
                .get(&url)
                .header(header, &auth)
                .header("anthropic-version", "2023-06-01")
                .call()
        } else {
            self.agent
                .post(&url)
                .header(header, &auth)
                .header("anthropic-version", "2023-06-01")
                .header("Content-Type", content_type)
                .send(body)
        }
        .map_err(|_| {
            anyhow::anyhow!("batch transport failed; submission may have occurred (not retried)")
        })?;
        ensure!(
            response.status().is_success(),
            "batch provider returned HTTP {}",
            response.status().as_u16()
        );
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| anyhow::anyhow!("batch response read failed"))?;
        ensure!(
            bytes.len() <= MAX_BYTES,
            "batch response exceeds byte bound"
        );
        Ok(bytes)
    }
    fn json(&self, method: &str, path: &str, value: &Value) -> Result<Value> {
        let bytes = self.request(
            method,
            path,
            "application/json",
            &serde_json::to_vec(value)?,
        )?;
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid batch provider JSON"))
    }
    pub fn submit(&self, plan: &Plan) -> Result<String> {
        plan.validate()?;
        ensure!(self.provider == plan.provider, "batch provider mismatch");
        let payload = match self.provider {
            Provider::Anthropic => {
                json!({"requests":plan.items.iter().map(|i| json!({"custom_id": i.custom_id, "params":plan.params(i)})).collect::<Vec<_>>()})
            }
            Provider::Openai => {
                let mut lines = String::new();
                for item in &plan.items {
                    lines.push_str(&serde_json::to_string(&json!({"custom_id":item.custom_id,"method":"POST","url":"/v1/chat/completions","body":plan.params(item)}))?);
                    lines.push('\n');
                }
                let boundary = format!("dial-batch-{:016x}", rand::random::<u64>());
                let body = format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"batch.jsonl\"\r\nContent-Type: application/jsonl\r\n\r\n{lines}\r\n--{boundary}--\r\n"
                );
                let uploaded: Value = serde_json::from_slice(&self.request(
                    "POST",
                    "/files",
                    &format!("multipart/form-data; boundary={boundary}"),
                    body.as_bytes(),
                )?)
                .map_err(|_| anyhow::anyhow!("invalid batch upload receipt"))?;
                let file = uploaded["id"].as_str().context("missing batch file id")?;
                validate_id(file)?;
                json!({"input_file_id":file,"endpoint":"/v1/chat/completions","completion_window":"24h"})
            }
        };
        let reply = self.json("POST", self.provider.route(), &payload)?;
        let id = reply["id"]
            .as_str()
            .context("missing remote batch identity")?;
        validate_id(id)?;
        Ok(id.to_owned())
    }
    pub fn status(&self, id: &str) -> Result<Value> {
        validate_id(id)?;
        let value = self.json(
            "GET",
            &format!("{}/{id}", self.provider.route()),
            &Value::Null,
        )?;
        // Never expose arbitrary provider error messages, metadata or echoed request content.
        let mut safe = serde_json::Map::new();
        for field in [
            "id",
            "status",
            "processing_status",
            "request_counts",
            "output_file_id",
            "error_file_id",
            "created_at",
            "expires_at",
            "completed_at",
            "cancelled_at",
            "ended_at",
        ] {
            if let Some(value) = value.get(field) {
                safe.insert(field.to_owned(), value.clone());
            }
        }
        Ok(Value::Object(safe))
    }
    pub fn cancel(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        self.json(
            "POST",
            &format!("{}/{id}/cancel", self.provider.route()),
            &json!({}),
        )?;
        Ok(())
    }
    pub fn results(&self, id: &str, plan: &Plan) -> Result<BTreeMap<String, Outcome>> {
        ensure!(plan.provider == self.provider, "batch provider mismatch");
        let status = self.status(id)?;
        let mut bytes = Vec::new();
        match self.provider {
            Provider::Anthropic => {
                ensure!(
                    status["processing_status"] == "ended",
                    "batch has not ended (cancel is nonterminal)"
                );
                bytes = self.request("GET", &format!("/messages/batches/{id}/results"), "", &[])?;
            }
            Provider::Openai => {
                ensure!(
                    ["completed", "failed", "expired", "cancelled"]
                        .iter()
                        .any(|s| status["status"] == *s),
                    "batch has not ended (cancel is nonterminal)"
                );
                for field in ["output_file_id", "error_file_id"] {
                    if let Some(file) = status[field].as_str() {
                        validate_id(file)?;
                        bytes.extend(self.request(
                            "GET",
                            &format!("/files/{file}/content"),
                            "",
                            &[],
                        )?);
                        bytes.push(b'\n');
                        ensure!(
                            bytes.len() <= MAX_BYTES,
                            "combined results exceed byte bound"
                        );
                    }
                }
            }
        }
        let mut results = parse_results(self.provider, &bytes, plan)?;
        for outcome in results.values_mut() {
            if let Outcome::Succeeded(text) = outcome {
                *text = text.replace(self.key.as_str(), "[REDACTED:ApiKey]");
            }
        }
        Ok(results)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", content = "text", rename_all = "snake_case")]
pub enum Outcome {
    Succeeded(String),
    Errored,
    Canceled,
    Expired,
    Missing,
}
fn parse_results(
    provider: Provider,
    bytes: &[u8],
    plan: &Plan,
) -> Result<BTreeMap<String, Outcome>> {
    let mut results: BTreeMap<_, _> = plan
        .items
        .iter()
        .map(|i| (i.custom_id.clone(), Outcome::Missing))
        .collect();
    let mut seen = HashSet::new();
    for line in bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
    {
        let value: Value = serde_json::from_slice(line)
            .map_err(|_| anyhow::anyhow!("invalid batch result JSONL"))?;
        let id = value["custom_id"]
            .as_str()
            .context("missing result custom_id")?;
        ensure!(
            results.contains_key(id) && seen.insert(id.to_owned()),
            "unexpected or duplicate batch result identity"
        );
        let outcome = match provider {
            Provider::Openai if value["response"]["status_code"] == 200 => {
                value["response"]["body"]["choices"][0]["message"]["content"]
                    .as_str()
                    .map(|s| Outcome::Succeeded(s.to_owned()))
                    .unwrap_or(Outcome::Errored)
            }
            Provider::Openai => Outcome::Errored,
            Provider::Anthropic => match value["result"]["type"].as_str() {
                Some("succeeded") => {
                    let text = value["result"]["message"]["content"]
                        .as_array()
                        .context("invalid successful batch message")?
                        .iter()
                        .filter_map(|v| v["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    Outcome::Succeeded(text)
                }
                Some("canceled") => Outcome::Canceled,
                Some("expired") => Outcome::Expired,
                Some("errored") => Outcome::Errored,
                _ => anyhow::bail!("unknown batch result state"),
            },
        };
        results.insert(id.to_owned(), outcome);
    }
    Ok(results)
}
#[cfg(test)]
mod tests;
