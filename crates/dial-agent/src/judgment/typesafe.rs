//! TypeSafe System One judgment backend, selected by `provider = "typesafe"` on the `judge` role.
//!
//! Wire contract (`typesafeWire`): `POST {base}/v1/systemone`
//! with bearer auth and body `{ model, state, questions }`, where `questions` maps a key to a choice
//! question `{ type: "choice", instructions, criteria: { option: rubric | null } }`. The reply carries
//! `answers.<key>.choice` plus `usage.cost` (USD) for the call.

use super::{Answer, Backend, Question};
use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value, json};
use std::sync::{Arc, atomic::AtomicBool};

/// Provider name that marks the `judge` role as TypeSafe System One.
pub const PROVIDER: &str = crate::config::selection::TYPESAFE_JUDGE_PROVIDER;
/// Model used when the `judge` role names no model of its own.
pub const DEFAULT_MODEL: &str = "jev-latest";
/// Base URL used when `TYPESAFE_BASE_URL` is unset.
pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";
const API_KEY_ENV: &str = "TYPESAFE_API_KEY";
const BASE_URL_ENV: &str = "TYPESAFE_BASE_URL";
/// Key of the single question inside `questions`; the answer is read back from the same key.
const QUESTION_KEY: &str = "choice";
/// Reply bodies are small typed answers; anything larger is a protocol violation.
const MAX_REPLY_BYTES: u64 = 1 << 20;

/// TypeSafe System One as a judgment backend. Built from [`TypesafeBackend::from_env`] in production.
pub struct TypesafeBackend {
    base_url: String,
    api_key: String,
    model: String,
    timeout_secs: u64,
    cancel: Arc<AtomicBool>,
}

impl TypesafeBackend {
    /// Reads `TYPESAFE_API_KEY` (required) and `TYPESAFE_BASE_URL` (optional) from the environment.
    pub fn from_env(model: &str, timeout_secs: u64, cancel: Arc<AtomicBool>) -> Result<Self> {
        let api_key = std::env::var(API_KEY_ENV).unwrap_or_default();
        let base_url = std::env::var(BASE_URL_ENV).ok();
        Self::new(&api_key, base_url.as_deref(), model, timeout_secs, cancel)
    }

    /// Validates the credential and endpoint. An empty `base_url` falls back to [`DEFAULT_BASE_URL`].
    pub fn new(
        api_key: &str,
        base_url: Option<&str>,
        model: &str,
        timeout_secs: u64,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self> {
        let api_key = api_key.trim();
        ensure!(
            !api_key.is_empty(),
            "the judge role uses TypeSafe, but {API_KEY_ENV} is not set"
        );
        let base_url = base_url
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/')
            .to_owned();
        ensure!(
            base_url.starts_with("http://") || base_url.starts_with("https://"),
            "{BASE_URL_ENV} must be an http(s) URL"
        );
        let model = if model.trim().is_empty() {
            DEFAULT_MODEL
        } else {
            model.trim()
        };
        Ok(Self {
            base_url,
            api_key: api_key.to_owned(),
            model: model.to_owned(),
            timeout_secs,
            cancel,
        })
    }

    /// The model id sent on the wire and used as the judgment cache identity.
    pub fn model(&self) -> &str {
        &self.model
    }
}

/// Request body for one choice judgment. Missing or empty rubrics become `null`, as System One expects.
pub fn request_body(model: &str, question: &Question<'_>) -> Value {
    let criteria: Map<String, Value> = question
        .choices
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            let rubric = question
                .criteria
                .get(index)
                .filter(|text| !text.trim().is_empty())
                .map_or(Value::Null, |text| json!(text));
            ((*choice).to_owned(), rubric)
        })
        .collect();
    json!({
        "model": model,
        "state": question.context,
        "questions": {
            QUESTION_KEY: {
                "type": "choice",
                "instructions": question.question,
                "criteria": criteria,
            }
        }
    })
}

/// Extracts the chosen label and the call price from a System One reply.
pub fn decode_reply(raw: &str) -> Result<(String, Option<f64>)> {
    let reply: Value = serde_json::from_str(raw).context("invalid TypeSafe judgment response")?;
    let choice = reply["answers"][QUESTION_KEY]["choice"]
        .as_str()
        .filter(|choice| !choice.is_empty())
        .context("TypeSafe judgment response has no choice")?;
    let cost_usd = reply["usage"]["cost"]
        .as_f64()
        .filter(|cost| cost.is_finite() && *cost >= 0.0);
    Ok((choice.to_owned(), cost_usd))
}

impl Backend for TypesafeBackend {
    fn answer(&mut self, question: &Question<'_>, _retry: bool) -> Result<Answer> {
        let body = request_body(&self.model, question);
        let agent =
            crate::llm::agent_with(self.timeout_secs, false, Some(self.cancel.clone()), None);
        let mut response = agent
            .post(format!("{}/v1/systemone", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send_json(body)
            .context("TypeSafe judgment request failed")?;
        ensure!(
            response.status().is_success(),
            "TypeSafe judgment request failed: HTTP {}",
            response.status()
        );
        let raw = response
            .body_mut()
            .with_config()
            .limit(MAX_REPLY_BYTES)
            .read_to_string()
            .context("reading TypeSafe judgment response")?;
        let (choice, cost_usd) = decode_reply(&raw)?;
        Ok(Answer {
            text: json!({ "choice": choice }).to_string(),
            cost_usd,
        })
    }
}

#[cfg(test)]
mod tests;
