//! Azure OpenAI Responses endpoint adapter.
use super::profiles::ModelProfile;
use super::transports::{OnText, Route, Wire};
use super::{Chat, Interrupted, Reply, StreamEvent};
use anyhow::{Result, ensure};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn url(base: &str) -> Result<String> {
    super::provider::endpoint_url(base, true)?;
    ensure!(
        !base.contains('#'),
        "Azure endpoint must not contain a fragment"
    );
    let base = base.trim_end_matches('/');
    let path = if base.ends_with("/openai/v1") {
        "/responses"
    } else {
        "/openai/v1/responses"
    };
    let version = std::env::var("AZURE_OPENAI_API_VERSION").unwrap_or_else(|_| "v1".into());
    Ok(format!(
        "{base}{path}?api-version={}",
        percent_encode(&version)
    ))
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(out, "%{byte:02X}").expect("string write");
        }
    }
    out
}

pub(super) fn exchange(
    route: &Route<'_>,
    req: &Chat,
    timeout_secs: u64,
    on_text: OnText<'_, '_>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    if interrupt.load(Ordering::SeqCst) {
        return Err(Interrupted.into());
    }
    let url = url(route.base_url)?;
    let mut headers = route.headers.to_vec();
    if let Some(auth) = route.auth {
        ensure!(!auth.is_empty(), "Azure credential is empty");
        headers.push(("api-key", auth));
    }
    let endpoint = super::Endpoint {
        url: &url,
        auth: None,
        model: route.model,
        headers: &headers,
    };
    super::transports::exchange_wire(
        Wire::Responses,
        endpoint,
        req,
        timeout_secs,
        on_text,
        interrupt,
        profile,
        on_event,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deployment_scoped_v1_endpoint_and_query_encoding() {
        assert_eq!(
            url("https://resource.openai.azure.com/openai/v1").unwrap(),
            "https://resource.openai.azure.com/openai/v1/responses?api-version=v1"
        );
        assert_eq!(percent_encode("2025-01-01 preview"), "2025-01-01%20preview");
    }
}
