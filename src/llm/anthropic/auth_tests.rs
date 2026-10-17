use super::*;

#[test]
fn subscription_headers_are_shared_by_discovery_and_inference() {
    for path in ["models", "messages"] {
        let headers = auth_headers(&format!("{BASE_URL}/{path}"), "sk-ant-oat01-test").unwrap();
        let beta = headers
            .iter()
            .find(|(name, _)| *name == "anthropic-beta")
            .unwrap()
            .1
            .as_str();
        assert!(beta.contains("oauth-2025-04-20"));
        if path == "models" {
            assert!(beta.contains("structured-outputs-2025-12-15"));
            assert!(!beta.contains("claude-code-20250219") && !beta.contains("fallback-credit"));
        } else {
            assert!(
                beta.contains("claude-code-20250219")
                    && beta.contains("fallback-credit-2026-06-01")
            );
        }
        assert!(headers.contains(&("User-Agent", "claude-cli/2.1.280 (external, cli)".into())));
        assert!(headers.contains(&("anthropic-dangerous-direct-browser-access", "true".into())));
        assert!(headers.contains(&("x-app", "cli".into())));
        assert!(headers.contains(&("X-Stainless-Lang", "js".into())));
        assert!(headers.contains(&("X-Stainless-Package-Version", "0.112.1".into())));
        assert!(headers.contains(&("X-Stainless-Runtime", "node".into())));
        assert!(!headers.iter().any(|(name, _)| *name == "x-api-key"));
    }
    let headers = auth_headers("http://localhost:1234/v1/messages", "sk-ant-api03-test").unwrap();
    assert!(headers.contains(&("x-api-key", "sk-ant-api03-test".into())));
    assert!(!headers.iter().any(|(name, _)| *name == "Authorization"));
}

#[test]
fn subscription_tokens_cannot_be_forwarded_to_other_endpoints() {
    for url in [
        "https://proxy.example/v1/messages",
        "http://api.anthropic.com/v1/messages",
        "https://api.anthropic.com:8443/v1/messages",
        "https://api.anthropic.com.evil/v1/messages",
        "https://api.anthropic.com/v1/messages?redirect=elsewhere",
        "https://api.anthropic.com/other",
    ] {
        assert!(auth_headers(url, "sk-ant-oat01-test").is_err(), "{url}");
    }
    assert!(auth_headers(BASE_URL, "sk-ant-oat01-bad\nvalue").is_err());
}
