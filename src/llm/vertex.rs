//! Google Vertex AI GenerateContent and Anthropic Messages transports.
use super::profiles::ModelProfile;
use super::transports::{OnText, Route, Wire};
use super::{Chat, Endpoint, Interrupted, Reply, StreamEvent};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char)
        } else {
            use std::fmt::Write;
            write!(out, "%{b:02X}").expect("string write")
        }
    }
    out
}
fn location(route: &Route<'_>) -> String {
    if let Some(host) = super::host_of(route.base_url)
        && let Some(loc) = host.strip_suffix("-aiplatform.googleapis.com")
        && loc != "aiplatform"
    {
        return loc.to_owned();
    }
    std::env::var("GOOGLE_CLOUD_LOCATION")
        .or_else(|_| std::env::var("CLOUDSDK_COMPUTE_REGION"))
        .unwrap_or_else(|_| "us-central1".into())
}
fn project(route: &Route<'_>) -> Result<String> {
    if let Some(p) = route.project_id.filter(|p| !p.is_empty()) {
        return Ok(p.into());
    }
    std::env::var("GOOGLE_CLOUD_PROJECT")
        .or_else(|_| std::env::var("GCLOUD_PROJECT"))
        .or_else(|_| super::vertex_adc::project_id())
        .context("Vertex requires GOOGLE_CLOUD_PROJECT or an OAuth grant project")
}
fn base(route: &Route<'_>) -> String {
    if route.base_url.contains("aiplatform.googleapis.com") {
        route.base_url.trim_end_matches('/').into()
    } else {
        let loc = location(route);
        if loc == "global" {
            "https://aiplatform.googleapis.com".into()
        } else {
            format!("https://{loc}-aiplatform.googleapis.com")
        }
    }
}
fn vertex_url(route: &Route<'_>, publisher: &str, suffix: &str) -> Result<String> {
    let loc = location(route);
    Ok(format!(
        "{}/v1/projects/{}/locations/{}/publishers/{publisher}/models/{}:{suffix}",
        base(route),
        encode(&project(route)?),
        encode(&loc),
        encode(route.model)
    ))
}
fn credentials(route: &Route<'_>, interrupt: &Arc<AtomicBool>) -> Result<Vec<(String, String)>> {
    let mut headers: Vec<(String, String)> = route
        .headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let token = match route.auth {
        Some(token) => token.to_owned(),
        None => super::vertex_adc::access_token(interrupt)?,
    };
    headers.push(("Authorization".into(), format!("Bearer {token}")));
    if let Some(project) = std::env::var("GOOGLE_CLOUD_QUOTA_PROJECT")
        .or_else(|_| std::env::var("GOOGLE_CLOUD_QUOTA_PROJECT_ID"))
        .ok()
        .filter(|p| !p.is_empty())
        .or_else(super::vertex_adc::quota_project_id)
    {
        headers.push(("x-goog-user-project".into(), project));
    }
    Ok(headers)
}
pub(super) fn exchange_gemini(
    route: &Route<'_>,
    req: &Chat,
    timeout: u64,
    on_text: OnText<'_, '_>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    if interrupt.load(Ordering::SeqCst) {
        return Err(Interrupted.into());
    }
    let url = vertex_url(route, "google", "streamGenerateContent")? + "?alt=sse";
    let owned = credentials(route, &interrupt)?;
    let headers: Vec<(&str, &str)> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let endpoint = Endpoint {
        url: &url,
        auth: None,
        model: route.model,
        headers: &headers,
    };
    super::gemini::exchange_at(
        endpoint, &url, req, timeout, on_text, interrupt, profile, on_event,
    )
}
pub(super) fn exchange_anthropic(
    route: &Route<'_>,
    req: &Chat,
    timeout: u64,
    on_text: OnText<'_, '_>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    if interrupt.load(Ordering::SeqCst) {
        return Err(Interrupted.into());
    }
    let url = vertex_url(route, "anthropic", "streamRawPredict")?;
    let mut owned = credentials(route, &interrupt)?;
    owned.push(("anthropic-version".into(), "2023-06-01".into()));
    let headers: Vec<(&str, &str)> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let endpoint = Endpoint {
        url: &url,
        auth: None,
        model: route.model,
        headers: &headers,
    };
    let shape = &|body: &mut Value| -> Result<()> {
        body.as_object_mut()
            .context("Anthropic request body must be object")?
            .remove("model");
        body["anthropic_version"] = json!("vertex-2023-10-16");
        Ok(())
    };
    super::transports::exchange_wire(
        Wire::Anthropic,
        endpoint,
        req,
        timeout,
        on_text,
        interrupt,
        profile,
        on_event,
        Some(shape),
    )
}
