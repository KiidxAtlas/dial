//! Factory Droid's OAuth-backed, four-wire inference proxy.
use super::model_config::ApiProtocol;
use super::profiles::ModelProfile;
use super::transports::{self, Route, Wire};
use super::{Chat, Reply, StreamEvent};
use anyhow::{Context, Result, ensure};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::json;
use std::sync::{Arc, atomic::AtomicBool};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FactoryWire {
    Anthropic,
    Responses,
    Completions,
    Gemini,
}

pub(super) fn exchange(
    route: &Route<'_>,
    req: &Chat,
    timeout_secs: u64,
    on_text: transports::OnText<'_, '_>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    ensure!(
        route.protocol == ApiProtocol::FactoryDroid,
        "invalid Factory Droid route"
    );
    let token = route
        .auth
        .ok_or_else(|| anyhow::anyhow!("Factory Droid requires a WorkOS OAuth token"))?;
    let factory_wire = factory_wire(route.model);
    let region = route
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("X-Factory-Inference-Region"))
        .map(|(_, value)| *value);
    let upstream = factory_upstream(route.model, factory_wire, region)?;
    let (wire, package_version) = match factory_wire {
        FactoryWire::Anthropic => (Wire::Anthropic, Some("0.70.1")),
        FactoryWire::Responses => (Wire::Responses, Some("6.25.0")),
        FactoryWire::Completions => (Wire::ChatCompletions, Some("6.25.0")),
        FactoryWire::Gemini => (Wire::Gemini, None),
    };
    let session_id = route
        .session_id
        .context("Factory Droid requires a bound conversation id")?;
    let assistant_id = route
        .request_id
        .context("Factory Droid requires a request id")?;
    let mut headers = route.headers.to_vec();
    headers.extend([
        ("Authorization", token),
        ("User-Agent", "factory-cli/0.230.0"),
        ("X-Client-Version", "0.230.0"),
        ("X-Factory-Client", "cli"),
        ("x-api-provider", upstream),
        ("x-provider-routing-source", "registry_default"),
        ("x-session-id", session_id),
        ("x-assistant-message-id", assistant_id),
    ]);
    if let Some(package_version) = package_version {
        headers.extend([
            ("X-Stainless-Lang", "js"),
            ("X-Stainless-Package-Version", package_version),
            ("X-Stainless-Runtime", "node"),
            ("X-Stainless-Runtime-Version", "v26.3.0"),
            ("X-Stainless-Retry-Count", "0"),
            ("X-Stainless-Arch", stainless_arch()),
            ("X-Stainless-OS", stainless_os()),
        ]);
    }
    if factory_wire == FactoryWire::Anthropic {
        headers.extend([("x-api-key", "placeholder"), ("X-Stainless-Timeout", "600")]);
    }
    if matches!(
        factory_wire,
        FactoryWire::Responses | FactoryWire::Completions
    ) {
        headers.push(("Accept", "application/json"));
    }
    let mut request = req.clone();
    request.messages.insert(0, json!({"role":"system","content":"You are Droid, an AI software engineering agent built by Factory."}));
    let url = factory_wire_url(route.base_url, factory_wire);
    let endpoint = super::Endpoint {
        url: &url,
        auth: None,
        model: route.model,
        headers: &headers,
    };
    if factory_wire == FactoryWire::Gemini {
        return super::gemini::exchange_at(
            endpoint,
            &url,
            &request,
            timeout_secs,
            on_text,
            interrupt,
            profile,
            on_event,
        );
    }
    transports::exchange_wire(
        wire,
        endpoint,
        &request,
        timeout_secs,
        on_text,
        interrupt,
        profile,
        on_event,
        None,
    )
}

fn factory_wire(model: &str) -> FactoryWire {
    if contains(
        &[
            "claude-fable-5.1",
            "claude-fable-5",
            "claude-opus-5-5",
            "claude-opus-5-5-fast",
            "claude-opus-5",
            "claude-opus-5-fast",
            "claude-opus-4-8",
            "claude-opus-4-8-fast",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-opus-4-5-20251101",
            "claude-sonnet-5-5",
            "claude-sonnet-5",
            "claude-sonnet-4-6",
            "claude-sonnet-4-5-20250929",
            "claude-haiku-4-5-20251001",
            "atlas-07-21",
            "aster-07-15",
            "minimax-m2.7",
        ],
        model,
    ) {
        FactoryWire::Anthropic
    } else if contains(
        &[
            "gpt-6-astra",
            "gpt-6.1-sol",
            "gpt-6-sol",
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-sol-fast",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
            "gpt-5.5-fast",
            "gpt-5.5-pro",
            "gpt-5.4",
            "gpt-5.4-fast",
            "gpt-5.4-mini",
            "gpt-5.4-mini-fast",
            "gpt-5.3-codex",
            "gpt-5.3-codex-fast",
            "gpt-5.2",
            "grok-4.7",
            "grok-4.6",
            "grok-4.5",
        ],
        model,
    ) {
        FactoryWire::Responses
    } else if contains(
        &[
            "garnet-07-15",
            "gemini-3.1-pro-preview",
            "gemini-3.8-flash",
            "gemini-3.7-flash",
            "gemini-3.6-flash",
            "gemini-3.5-flash",
            "gemini-3-flash-preview",
        ],
        model,
    ) {
        FactoryWire::Gemini
    } else {
        FactoryWire::Completions
    }
}

fn factory_upstream(model: &str, wire: FactoryWire, region: Option<&str>) -> Result<&'static str> {
    let region = region.unwrap_or("global");
    match wire {
        FactoryWire::Anthropic if region == "eu" => match model {
            "claude-fable-5.1"
            | "claude-fable-5"
            | "claude-opus-5-5"
            | "claude-opus-4-6"
            | "claude-sonnet-5-5"
            | "claude-sonnet-5"
            | "claude-sonnet-4-6"
            | "claude-opus-4-5-20251101"
            | "claude-sonnet-4-5-20250929"
            | "claude-haiku-4-5-20251001" => Ok("vertex_anthropic"),
            "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" => Ok("bedrock_anthropic"),
            _ => anyhow::bail!(
                "Factory Droid model {model:?} is not served in the EU inference region"
            ),
        },
        FactoryWire::Anthropic => Ok("anthropic"),
        FactoryWire::Responses
            if region == "eu" && contains(&["grok-4.7", "grok-4.6", "grok-4.5"], model) =>
        {
            anyhow::bail!("Factory Droid model {model:?} is not served in the EU inference region")
        }
        FactoryWire::Responses => {
            if contains(&["grok-4.7", "grok-4.6", "grok-4.5"], model) {
                Ok("xai")
            } else {
                Ok("openai")
            }
        }
        FactoryWire::Gemini if region == "eu" => {
            anyhow::bail!("Factory Droid model {model:?} is not served in the EU inference region")
        }
        FactoryWire::Gemini => Ok("google"),
        FactoryWire::Completions if region == "eu" => match model {
            "mistral-medium-3.5" | "glm-5.3" | "glm-5.2" => Ok("mistral"),
            _ => anyhow::bail!(
                "Factory Droid model {model:?} is not served in the EU inference region"
            ),
        },
        FactoryWire::Completions if region == "us" => match model {
            "mistral-medium-3.5" => Ok("mistral"),
            "glm-5.3-flash"
            | "glm-5.3"
            | "glm-5.2"
            | "glm-5.2-fast"
            | "kimi-k3"
            | "nemotron-3-ultra"
            | "deepseek-v4.1-flash"
            | "deepseek-v4-flash-0731"
            | "deepseek-v4-pro" => Ok("baseten"),
            _ => anyhow::bail!(
                "Factory Droid model {model:?} is not served in the US inference region"
            ),
        },
        FactoryWire::Completions => match model {
            "mistral-medium-3.5" => Ok("mistral"),
            "glm-5.2" | "glm-5.2-fast" | "nemotron-3-ultra" => Ok("baseten"),
            _ => Ok("fireworks"),
        },
    }
}

fn contains(ids: &[&str], model: &str) -> bool {
    ids.contains(&model)
}

fn factory_wire_url(base: &str, wire: FactoryWire) -> String {
    let base = base.trim_end_matches('/');
    let suffix = match wire {
        FactoryWire::Anthropic => "/api/llm/a/v1/messages",
        FactoryWire::Responses => "/api/llm/o/v1/responses",
        FactoryWire::Completions => "/api/llm/o/v1/chat/completions",
        FactoryWire::Gemini => "/api/llm/g/v1/generate",
    };
    if base.ends_with(suffix) {
        base.to_owned()
    } else if base.ends_with("/api/llm/a") && wire == FactoryWire::Anthropic {
        format!("{base}/v1/messages")
    } else if base.ends_with("/api/llm/o/v1")
        && matches!(wire, FactoryWire::Responses | FactoryWire::Completions)
    {
        format!(
            "{base}/{}",
            if wire == FactoryWire::Responses {
                "responses"
            } else {
                "chat/completions"
            }
        )
    } else if base.ends_with("/api/llm/g/v1") && wire == FactoryWire::Gemini {
        format!("{base}/generate")
    } else {
        format!("{base}{suffix}")
    }
}

pub(super) fn new_request_id() -> Result<String> {
    let rng = SystemRandom::new();
    let mut b = [0u8; 16];
    rng.fill(&mut b)
        .map_err(|_| anyhow::anyhow!("could not generate Factory request identity"))?;
    b[6] = (b[6] & 15) | 64;
    b[8] = (b[8] & 63) | 128;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    ))
}
fn stainless_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x64"
    } else {
        "unknown"
    }
}
fn stainless_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "MacOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests;
