use super::*;
#[test]
fn runtime_wire_map_fixtures_cover_every_wire() {
    assert_eq!(factory_wire("claude-opus-5-5"), FactoryWire::Anthropic);
    assert_eq!(factory_wire("gpt-6-luna"), FactoryWire::Responses);
    assert_eq!(factory_wire("gemini-3.1-pro-preview"), FactoryWire::Gemini);
    assert_eq!(factory_wire("kimi-k3"), FactoryWire::Completions);
    assert_eq!(
        factory_upstream("grok-4.7", FactoryWire::Responses, None).unwrap(),
        "xai"
    );
    assert_eq!(
        factory_upstream("glm-5.3", FactoryWire::Completions, Some("eu")).unwrap(),
        "mistral"
    );
    assert!(factory_upstream("kimi-k3", FactoryWire::Completions, Some("eu")).is_err());
}
#[test]
fn proxy_url_fixtures_match_factory_wire_paths() {
    assert_eq!(
        factory_wire_url("https://api.factory.ai", FactoryWire::Anthropic),
        "https://api.factory.ai/api/llm/a/v1/messages"
    );
    assert_eq!(
        factory_wire_url("https://api.eu.factory.ai", FactoryWire::Responses),
        "https://api.eu.factory.ai/api/llm/o/v1/responses"
    );
    assert_eq!(
        factory_wire_url("https://api.factory.ai", FactoryWire::Completions),
        "https://api.factory.ai/api/llm/o/v1/chat/completions"
    );
    assert_eq!(
        factory_wire_url("https://api.factory.ai", FactoryWire::Gemini),
        "https://api.factory.ai/api/llm/g/v1/generate"
    );
}
#[test]
fn request_ids_are_v4_uuids() {
    let id = new_request_id().unwrap();
    assert_eq!(id.len(), 36);
    assert_eq!(&id[14..15], "4");
}
