//! Google Cloud Code Assist / Gemini CLI streaming transport.
use super::profiles::ModelProfile;
use super::transports::{OnText, Route};
use super::{Chat, Endpoint, Reply, StreamEvent};
use anyhow::{Context, Result, ensure};
use rand::RngCore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

fn endpoint(route: &Route<'_>) -> Result<String> {
    let base = route.base_url.trim_end_matches('/');
    // The configured base must be a plain endpoint; the stream query is ours.
    super::provider::endpoint_url(base, route.allow_insecure)?;
    Ok(format!("{base}/v1internal:streamGenerateContent?alt=sse"))
}
struct AntigravityState {
    session_id: String,
    trajectory_id: String,
    request_id: String,
    step: u64,
    last_execution_id: Option<String>,
}

fn identity_hash(kind: &str, session: &str, account: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    for part in ["dial-antigravity-v1", kind, session, account] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize().into()
}

fn uuid(mut bytes: [u8; 16], version: u8) -> String {
    bytes[6] = (bytes[6] & 0x0f) | (version << 4);
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = crate::hex(&bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn deterministic_uuid(kind: &str, session: &str, account: &str) -> String {
    let hash = identity_hash(kind, session, account);
    uuid(hash[..16].try_into().expect("16-byte UUID slice"), 5)
}

fn random_uuid() -> String {
    let mut bytes = [0; 16];
    rand::rng().fill_bytes(&mut bytes);
    uuid(bytes, 4)
}

fn first_user_text(chat: &Chat) -> Option<&str> {
    chat.messages
        .iter()
        .find(|message| message["role"] == "user")
        .and_then(|message| {
            message["content"].as_str().or_else(|| {
                message["content"]
                    .as_array()?
                    .iter()
                    .find(|part| matches!(part["type"].as_str(), Some("text" | "input_text")))
                    .and_then(|part| part["text"].as_str())
            })
        })
}

fn antigravity_state(route: &Route<'_>, chat: &Chat, scope: &str) -> Result<AntigravityState> {
    let account = route
        .account_id
        .or(route.project_id)
        .context("Antigravity grant has no account or project identity")?;
    let session_id = if let Some(conversation) = route.session_id {
        let digest = identity_hash("session", conversation, account);
        let value = u64::from_be_bytes(digest[..8].try_into().expect("8-byte digest prefix"))
            & i64::MAX as u64;
        format!("-{}", value % 9_000_000_000_000_000_000)
    } else if let Some(text) = first_user_text(chat).filter(|text| !text.trim().is_empty()) {
        let digest = Sha256::digest(text.as_bytes());
        let value = u64::from_be_bytes(digest[..8].try_into().expect("8-byte digest prefix"))
            & i64::MAX as u64;
        format!("-{value}")
    } else {
        format!("-{}", rand::random::<u64>() % 9_000_000_000_000_000_000)
    };
    let (agent_id, trajectory_id) = if let Some(conversation) = route.session_id {
        (
            deterministic_uuid("agent", conversation, account),
            deterministic_uuid("trajectory", conversation, account),
        )
    } else {
        (random_uuid(), random_uuid())
    };
    let mut step = 2_u64;
    let mut last_execution_id = None;
    for message in &chat.messages {
        if message["role"] != "assistant" {
            continue;
        }
        let Some(items) = message["dial_provider_content"].as_array() else {
            continue;
        };
        if let Some(carrier) = items
            .iter()
            .find(|item| item["type"] == "dial_gemini_content" && item["scope"] == scope)
        {
            step = step
                .checked_add(1)
                .context("Antigravity request step overflow")?;
            if let Some(id) = carrier["execution_id"].as_str() {
                last_execution_id = Some(id.to_owned());
            }
        }
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?
        .as_millis();
    let request_id = format!("agent/{agent_id}/{timestamp}/{trajectory_id}/{step}");
    Ok(AntigravityState {
        session_id,
        trajectory_id,
        request_id,
        step,
        last_execution_id,
    })
}

fn antigravity_model_profile(model: &str) -> (Option<&'static str>, Option<u32>) {
    match model {
        "gemini-3.5-flash-extra-low" => (Some("MODEL_PLACEHOLDER_M187"), Some(65536)),
        "gemini-3.5-flash-low" => (Some("MODEL_PLACEHOLDER_M20"), Some(65536)),
        "gemini-3-flash-agent" => (Some("MODEL_PLACEHOLDER_M132"), Some(65536)),
        "gemini-3.1-pro-low" => (Some("MODEL_PLACEHOLDER_M36"), Some(65535)),
        "gemini-pro-agent" => (Some("MODEL_PLACEHOLDER_M16"), Some(65535)),
        "claude-sonnet-4-6" | "claude-opus-4-6-thinking" => (None, Some(64000)),
        _ => (None, None),
    }
}
fn request_headers(route: &Route<'_>) -> Result<Vec<(String, String)>> {
    let mut headers = route
        .headers
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect::<Vec<_>>();
    ensure!(
        !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("user-agent")
                || name.eq_ignore_ascii_case("client-metadata")),
        "Cloud Code Assist transport headers cannot override User-Agent or Client-Metadata"
    );
    if route.protocol == super::model_config::ApiProtocol::Antigravity {
        headers.push((
            "User-Agent".into(),
            "antigravity/hub/2.19.1 (aidev_client; os_type=darwin; arch=arm64; cl=963137146)"
                .into(),
        ));
    } else {
        let version = std::env::var("DIAL_GEMINI_CLI_VERSION").unwrap_or_else(|_| "0.46.0".into());
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "win32",
            value => value,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            value => value,
        };
        headers.push((
            "User-Agent".into(),
            format!(
                "GeminiCLI/{version}/{} ({os}; {arch}; terminal)",
                route.model
            ),
        ));
        headers.push((
            "Client-Metadata".into(),
            "ideType=IDE_UNSPECIFIED,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI".into(),
        ));
    }
    Ok(headers)
}

fn envelope(route: &Route<'_>, mut request: Value, chat: &Chat, scope: &str) -> Result<Value> {
    let antigravity = route.protocol == super::model_config::ApiProtocol::Antigravity;
    let project = route
        .project_id
        .context("Cloud Code Assist grant has no project ID")?;
    if antigravity {
        let state = antigravity_state(route, chat, scope)?;
        request["sessionId"] = json!(state.session_id);
        let (model_enum, max_output_tokens) = antigravity_model_profile(route.model);
        let used_claude = route.model.starts_with("claude").to_string();
        let mut labels = serde_json::Map::new();
        if let Some(id) = state.last_execution_id {
            labels.insert("last_execution_id".into(), json!(id));
        }
        labels.insert(
            "last_step_index".into(),
            json!((state.step - 1).to_string()),
        );
        labels.insert("trajectory_id".into(), json!(state.trajectory_id));
        if let Some(model_enum) = model_enum {
            labels.insert("model_enum".into(), json!(model_enum));
        }
        labels.insert("used_claude".into(), json!(used_claude));
        labels.insert("used_claude_conservative".into(), json!(used_claude));
        request["labels"] = Value::Object(labels);
        if let Some(max_output_tokens) = max_output_tokens {
            request["generationConfig"]["maxOutputTokens"] = json!(max_output_tokens);
        }
        Ok(json!({
            "model": route.model,
            "project": project,
            "request": request,
            "userAgent": "antigravity",
            "requestId": state.request_id,
            "requestType": "agent"
        }))
    } else {
        Ok(json!({"model": route.model, "project": project, "request": request}))
    }
}

fn antigravity_fallback(route: &Route<'_>) -> Option<String> {
    if route.protocol != super::model_config::ApiProtocol::Antigravity {
        return None;
    }
    match route.base_url.trim_end_matches('/') {
        "https://daily-cloudcode-pa.googleapis.com" =>
            Some("https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:streamGenerateContent?alt=sse".into()),
        "https://daily-cloudcode-pa.sandbox.googleapis.com" =>
            Some("https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse".into()),
        _ => None,
    }
}

pub(super) fn exchange(
    route: &Route<'_>,
    req: &Chat,
    timeout: u64,
    on_text: OnText<'_, '_>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    if interrupt.load(Ordering::SeqCst) {
        return Err(super::Interrupted.into());
    }
    let mut url = endpoint(route)?;
    let fallback = antigravity_fallback(route);
    let owned_headers = request_headers(route)?;
    let headers = owned_headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    let endpoint = Endpoint {
        url: route.base_url,
        auth: route.auth,
        model: route.model,
        headers: &headers,
    };
    let scope = super::model_config::native_replay_scope(
        if route.protocol == super::model_config::ApiProtocol::Antigravity {
            "antigravity"
        } else {
            "cloud-code-assist"
        },
        route.base_url,
        Some(route.model),
        route.account_id,
        route.auth,
        &headers,
    );
    let request = super::gemini::request_for_proxy(&endpoint, req, profile, &scope)?;
    let body = envelope(route, request, req, &scope)?;
    let patience = super::Patience::for_stream(timeout, false);
    let mut response = match super::send_with_headers(
        &url,
        route.auth,
        &body,
        timeout,
        Some(patience.clone()),
        Some(interrupt.clone()),
        &headers,
    ) {
        Ok(response) => response,
        Err(_error) if fallback.is_some() && !interrupt.load(Ordering::SeqCst) => {
            url = fallback.clone().unwrap();
            super::send_with_headers(
                &url,
                route.auth,
                &body,
                timeout,
                Some(patience.clone()),
                Some(interrupt.clone()),
                &headers,
            )?
        }
        Err(error) => return Err(error),
    };
    let first_status = response.status().as_u16();
    if (first_status == 429 || first_status >= 500)
        && let Some(fallback) = fallback
    {
        url = fallback;
        response = super::send_with_headers(
            &url,
            route.auth,
            &body,
            timeout,
            Some(patience.clone()),
            Some(interrupt.clone()),
            &headers,
        )?;
    }
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let delay = super::retry::server_delay(response.headers());
        let bytes =
            super::read_bounded(response.body_mut().as_reader(), super::ERROR_EXCERPT_BYTES)?;
        if status == 429
            && let Some(error) = super::gemini::throttle_error(&bytes, delay)
        {
            return Err(error);
        }
        return Err(super::http_error(status, &bytes, route.auth, delay));
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
        "Cloud Code Assist requires an SSE stream"
    );
    let mut accumulator = super::gemini::Accumulator::default();
    let read = super::stream::read_events_with_progress(
        response.body_mut().as_reader(),
        super::MAX_STREAM_BYTES,
        super::stream::MAX_LINE_BYTES,
        patience.clone(),
        |_| false,
        |data| {
            if interrupt.load(Ordering::SeqCst) {
                return Err(super::Interrupted.into());
            }
            accumulator.feed(data, &patience, on_text, on_event)
        },
    );
    if interrupt.load(Ordering::SeqCst) {
        return Err(super::Interrupted.into());
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
        if !accumulator.is_terminal() || !(stalled || incomplete) {
            return Err(super::response_error(error, route.auth));
        }
    }
    accumulator.finish(&endpoint, scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn antigravity_envelope_replays_scoped_state_and_honors_loopback() {
        let route = Route {
            protocol: super::super::model_config::ApiProtocol::Antigravity,
            base_url: "http://127.0.0.1:9000",
            model: "gemini-3.1-pro-low",
            auth: None,
            account_id: Some("account"),
            headers: &[],
            project_id: Some("project"),
            allow_insecure: true,
            session_id: Some("session"),
            request_id: None,
        };
        let owned = request_headers(&route).unwrap();
        let headers = owned
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        let scope = super::super::model_config::native_replay_scope(
            "antigravity",
            route.base_url,
            Some(route.model),
            route.account_id,
            route.auth,
            &headers,
        );
        let chat = Chat {
            messages: vec![
                json!({"role":"user","content":"hello"}),
                json!({"role":"assistant","content":"prior", "dial_provider_content":[{
                    "type":"dial_gemini_content","scope":scope,"execution_id":"exec","parts":[],"calls":[]}]}),
            ],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 100,
            seed: None,
        };
        let body = envelope(&route, json!({"contents": []}), &chat, &scope).unwrap();
        assert_eq!(body["userAgent"], "antigravity");
        assert_eq!(body["requestType"], "agent");
        assert!(
            body["request"]["sessionId"]
                .as_str()
                .unwrap()
                .starts_with('-')
        );
        assert!(body["requestId"].as_str().unwrap().starts_with("agent/"));
        assert_eq!(body["request"]["labels"]["last_execution_id"], "exec");
        assert_eq!(
            body["request"]["labels"]["trajectory_id"]
                .as_str()
                .unwrap()
                .len(),
            36
        );
        assert_eq!(body["request"]["labels"]["last_step_index"], "2");
        assert_eq!(
            body["request"]["labels"]["model_enum"],
            "MODEL_PLACEHOLDER_M36"
        );
        assert_eq!(
            body["request"]["generationConfig"]["maxOutputTokens"],
            65535
        );
        assert_eq!(
            endpoint(&route).unwrap(),
            "http://127.0.0.1:9000/v1internal:streamGenerateContent?alt=sse"
        );
    }
}
