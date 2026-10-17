//! GitHub Copilot chat API transport.
use super::model_config::ApiProtocol;
use super::profiles::ModelProfile;
use super::transports::{self, Route, Wire};
use super::{Chat, Reply, StreamEvent};
use anyhow::Result;
use std::sync::{Arc, atomic::AtomicBool};

const COPILOT_HEADERS: [(&str, &str); 6] = [
    ("Copilot-Integration-Id", "copilot-chat"),
    ("User-Agent", "copilot/1.0.82"),
    ("Editor-Version", "copilot/1.0.82"),
    ("Copilot-Harness-Id", "copilot-sdk"),
    ("Openai-Intent", "conversation-agent"),
    ("X-GitHub-Api-Version", "2026-08-01"),
];

pub(super) fn exchange(
    route: &Route<'_>,
    req: &Chat,
    timeout_secs: u64,
    on_text: transports::OnText<'_, '_>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    debug_assert_eq!(route.protocol, ApiProtocol::Copilot);
    let mut headers = route.headers.to_vec();
    headers.extend(COPILOT_HEADERS);
    let url = copilot_chat_url(route.base_url);
    let endpoint = super::Endpoint {
        url: &url,
        auth: route.auth,
        model: route.model,
        headers: &headers,
    };
    transports::exchange_wire(
        Wire::ChatCompletions,
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

fn copilot_chat_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_owned()
    } else {
        format!("{base}/chat/completions")
    }
}

#[cfg(test)]
mod tests;
