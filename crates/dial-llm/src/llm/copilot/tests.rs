use super::*;
#[test]
fn request_fixture_uses_chat_endpoint_and_identity_headers() {
    assert_eq!(
        copilot_chat_url("https://api.githubcopilot.com/"),
        "https://api.githubcopilot.com/chat/completions"
    );
    assert_eq!(
        copilot_chat_url("https://enterprise.example/chat/completions"),
        "https://enterprise.example/chat/completions"
    );
    assert!(COPILOT_HEADERS.contains(&("Copilot-Integration-Id", "copilot-chat")));
    assert!(COPILOT_HEADERS.contains(&("X-GitHub-Api-Version", "2026-08-01")));
}
