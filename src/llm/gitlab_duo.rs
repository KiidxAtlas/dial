//! GitLab Duo AI Gateway proxy transport.
use super::model_config::ApiProtocol;
use super::profiles::ModelProfile;
use super::transports::{self, Route, Wire};
use super::{Chat, Reply, StreamEvent};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

const DIRECT_ACCESS_URL: &str = "https://gitlab.com/api/v4/ai/third_party_agents/direct_access";

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
        route.protocol == ApiProtocol::GitlabDuo,
        "invalid GitLab Duo route"
    );
    let token = route
        .auth
        .context("GitLab Duo requires an OAuth or GITLAB_TOKEN credential")?;
    let mut response = super::send_with_headers(
        DIRECT_ACCESS_URL,
        Some(token),
        &json!({"feature_flags":{"DuoAgentPlatformNext":true}}),
        timeout_secs,
        None,
        Some(Arc::clone(&interrupt)),
        &[],
    )?;
    let status = response.status().as_u16();
    let delay = super::retry::server_delay(response.headers());
    let bytes = super::read_bounded(
        response.body_mut().as_reader(),
        if (200..300).contains(&status) {
            super::MAX_RESPONSE_BYTES
        } else {
            super::ERROR_EXCERPT_BYTES
        },
    )?;
    if !(200..300).contains(&status) {
        return Err(super::http_error(status, &bytes, Some(token), delay));
    }
    ensure!(
        !bytes.truncated,
        "GitLab Duo direct-access response exceeded the configured limit"
    );
    let data: Value = serde_json::from_slice(&bytes.data)
        .context("GitLab Duo returned invalid direct-access JSON")?;
    let access_token = data["token"]
        .as_str()
        .context("GitLab Duo direct-access response omitted token")?;
    ensure!(
        !access_token.is_empty(),
        "GitLab Duo direct-access response returned an empty token"
    );
    let gateway_headers = data["headers"]
        .as_object()
        .context("GitLab Duo direct-access response omitted headers")?;
    let mut owned = route
        .headers
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect::<Vec<_>>();
    owned.extend(
        gateway_headers
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|value| (k.clone(), value.to_owned()))),
    );
    let borrowed = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect::<Vec<_>>();
    let url = gateway_messages_url(route.base_url);
    let endpoint = super::Endpoint {
        url: &url,
        auth: Some(access_token),
        model: route.model,
        headers: &borrowed,
    };
    transports::exchange_wire(
        Wire::Anthropic,
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

fn gateway_messages_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/messages") {
        base.to_owned()
    } else {
        format!("{base}/messages")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn direct_access_fixture_parses_proxy_token_and_routing_headers() {
        let fixture = json!({"token":"proxy-token","headers":{"x-gitlab-instance-id":"instance"}});
        assert_eq!(fixture["token"].as_str(), Some("proxy-token"));
        assert_eq!(
            fixture["headers"]["x-gitlab-instance-id"].as_str(),
            Some("instance")
        );
        assert!(json!({"headers":{}})["token"].as_str().is_none());
    }
    #[test]
    fn messages_url_appends_anthropic_resource_once() {
        assert_eq!(
            gateway_messages_url("https://cloud.gitlab.com/ai/v1/proxy/anthropic"),
            "https://cloud.gitlab.com/ai/v1/proxy/anthropic/messages"
        );
        assert_eq!(
            gateway_messages_url("https://cloud.gitlab.com/ai/v1/proxy/anthropic/messages"),
            "https://cloud.gitlab.com/ai/v1/proxy/anthropic/messages"
        );
    }
}
