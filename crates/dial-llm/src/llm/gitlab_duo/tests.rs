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
