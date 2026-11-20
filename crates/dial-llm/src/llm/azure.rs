//! Azure OpenAI Responses endpoint adapter.
use super::profiles::ModelProfile;
use super::transports::{OnText, Route, Wire};
use super::{Chat, Interrupted, Reply, StreamEvent};
use crate::url::percent_encode;
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
        super::transports::WireRequest {
            wire: Wire::Responses,
            endpoint,
            req,
            limits: super::Limits {
                timeout_secs,
                interrupt,
            },
            profile,
        },
        on_text,
        on_event,
        None,
    )
}

#[cfg(test)]
mod tests;
