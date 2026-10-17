use super::*;

#[test]
fn extended_protocols_are_exactly_the_new_transports() {
    for p in [
        ApiProtocol::ChatCompletions,
        ApiProtocol::Responses,
        ApiProtocol::Anthropic,
        ApiProtocol::Gemini,
        ApiProtocol::Ollama,
    ] {
        assert!(!is_extended(p), "{}", p.as_str());
    }
    for p in [
        ApiProtocol::Bedrock,
        ApiProtocol::Vertex,
        ApiProtocol::VertexAnthropic,
        ApiProtocol::AzureResponses,
        ApiProtocol::Copilot,
        ApiProtocol::CloudCodeAssist,
        ApiProtocol::Antigravity,
        ApiProtocol::GitlabDuo,
        ApiProtocol::FactoryDroid,
        ApiProtocol::DevinAgent,
        ApiProtocol::Cursor,
    ] {
        assert!(is_extended(p), "{}", p.as_str());
    }
}

#[test]
fn route_urls_follow_protocol_conventions() {
    assert_eq!(
        route_url(
            ApiProtocol::ChatCompletions,
            "https://api.githubcopilot.com",
            false
        )
        .unwrap(),
        "https://api.githubcopilot.com/v1/chat/completions"
    );
    assert_eq!(
        route_url(ApiProtocol::Responses, "https://x.example/v1", false).unwrap(),
        "https://x.example/v1/responses"
    );
    assert_eq!(
        route_url(ApiProtocol::Anthropic, "https://x.example/v1", false).unwrap(),
        "https://x.example/v1/messages"
    );
    assert_eq!(
        route_url(
            ApiProtocol::Copilot,
            "https://api.githubcopilot.com/",
            false
        )
        .unwrap(),
        "https://api.githubcopilot.com"
    );
    assert!(route_url(ApiProtocol::Copilot, "http://remote.example", false).is_err());
}
