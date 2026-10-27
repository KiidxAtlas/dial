use super::*;
use crate::config::settings_store::write_private;
use crate::config::types::EndpointConfig;
use tempfile::TempDir;

const KEY_ENV: &str = "DIAL_BACKEND_AUTH_TEST_UNUSED_KEY";
const OPENAI_URL: &str = "https://api.openai.com/v1";

struct Fixture {
    _root: TempDir,
    config: Config,
}

impl Fixture {
    fn new(provider: &str, base_url: &str) -> Self {
        // Never consult an operator's shared subscription store or credential variable.
        assert!(std::env::var_os(crate::config::paths::SUBSCRIPTION_AUTH_HOME).is_none());
        assert!(std::env::var_os(KEY_ENV).is_none());
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let config = Config {
            provider: Some(provider.into()),
            model: "fixture-model".into(),
            user_dir: root.path().join("private"),
            state_dir: root.path().join("state"),
            project_root: project,
            endpoint: Some(EndpointConfig {
                base_url: base_url.into(),
                api_key_env: Some(KEY_ENV.into()),
                headers: None,
                protocol: None,
                account_id: None,
                allow_insecure: false,
            }),
            ..Default::default()
        };
        crate::platform::atomic::private_dir(&config.user_dir).unwrap();
        Self {
            _root: root,
            config,
        }
    }

    fn save_key(&self, base_url: &str) {
        llm::catalog::save_credential(&self.config.user_dir, base_url, "fixture-api-key").unwrap();
    }

    fn claude_sign_in(&self) {
        write_private(
            &llm::anthropic_oauth::credential_path(&self.config.user_dir),
            &serde_json::to_vec(&json!({"tokens": {
                "access_token": "fixture-access-token",
                "refresh_token": "fixture-refresh-token",
                "expires_at": 4102444800_u64
            }}))
            .unwrap(),
        )
        .unwrap();
    }
}

fn uses_claude_subscription(backend: &Backend) -> bool {
    matches!(
        backend.test_provider_kind(),
        ProviderKind::AnthropicSubscription { .. }
    )
}

#[test]
fn canonical_anthropic_subscription_precedes_saved_key() {
    let fixture = Fixture::new("anthropic", llm::anthropic::BASE_URL);
    fixture.save_key(llm::anthropic::BASE_URL);
    fixture.claude_sign_in();
    assert!(uses_claude_subscription(
        &make_backend(&fixture.config).unwrap()
    ));
    assert_eq!(
        llm::catalog::credential_token(&fixture.config.user_dir, llm::anthropic::BASE_URL)
            .unwrap()
            .as_deref(),
        Some("fixture-api-key")
    );
}

#[test]
fn canonical_anthropic_subscription_ignores_explicit_api_key_env() {
    let fixture = Fixture::new("anthropic", llm::anthropic::BASE_URL);
    fixture.claude_sign_in();
    // The explicit variable is absent and there is no saved key: resolving API credentials fails.
    assert!(
        llm::catalog::resolve_credential(
            &fixture.config.user_dir,
            llm::anthropic::BASE_URL,
            Some(KEY_ENV),
        )
        .is_err()
    );
    assert!(uses_claude_subscription(
        &make_backend(&fixture.config).unwrap()
    ));
}

#[test]
fn anthropic_signout_restores_saved_api_key_route() {
    let fixture = Fixture::new("anthropic", llm::anthropic::BASE_URL);
    fixture.save_key(llm::anthropic::BASE_URL);
    fixture.claude_sign_in();
    assert!(uses_claude_subscription(
        &make_backend(&fixture.config).unwrap()
    ));
    llm::anthropic_oauth::sign_out(&fixture.config.user_dir).unwrap();
    assert!(!uses_claude_subscription(
        &make_backend(&fixture.config).unwrap()
    ));
    assert_eq!(
        llm::catalog::resolve_credential(
            &fixture.config.user_dir,
            llm::anthropic::BASE_URL,
            Some(KEY_ENV)
        )
        .unwrap()
        .as_deref(),
        Some("fixture-api-key")
    );
}

#[test]
fn custom_anthropic_endpoint_uses_key_despite_canonical_subscription() {
    let url = "https://anthropic-proxy.example.invalid/v1";
    let fixture = Fixture::new("anthropic", url);
    fixture.save_key(url);
    fixture.claude_sign_in();
    let backend = make_backend(&fixture.config).unwrap();
    assert!(!uses_claude_subscription(&backend));
    assert!(
        matches!(backend.test_provider_kind(), ProviderKind::AnthropicEndpoint { base_url, api_key, .. }
        if base_url == url && api_key.as_deref() == Some("fixture-api-key"))
    );
    assert!(llm::anthropic_oauth::signed_in_checked(&fixture.config.user_dir).unwrap());
}

#[test]
fn corrupt_anthropic_subscription_does_not_fall_back_to_saved_key() {
    let fixture = Fixture::new("anthropic", llm::anthropic::BASE_URL);
    fixture.save_key(llm::anthropic::BASE_URL);
    write_private(
        &llm::anthropic_oauth::credential_path(&fixture.config.user_dir),
        b"not valid JSON",
    )
    .unwrap();
    assert!(make_backend(&fixture.config).is_err());
}

#[test]
fn openai_key_route_is_separate_from_signed_in_chatgpt() {
    let fixture = Fixture::new("openai", OPENAI_URL);
    fixture.save_key(OPENAI_URL);
    llm::chatgpt::test_account(&fixture.config.user_dir, false);
    let backend = make_backend(&fixture.config).unwrap();
    assert!(backend.account_id().is_none());
    assert!(
        matches!(backend.test_provider_kind(), ProviderKind::Endpoint { base_url, api_key, .. }
        if base_url == OPENAI_URL && api_key.as_deref() == Some("fixture-api-key"))
    );
    assert!(llm::chatgpt::resolve_account(&fixture.config.user_dir, None).is_ok());
}

#[test]
fn chatgpt_never_falls_back_to_openai_key_when_subscription_is_missing_or_signed_out() {
    let mut fixture = Fixture::new("chatgpt", llm::chatgpt::BASE_URL);
    fixture.config.endpoint.as_mut().unwrap().api_key_env = None;
    fixture.save_key(OPENAI_URL);
    assert!(make_backend(&fixture.config).is_err());

    llm::chatgpt::test_account(&fixture.config.user_dir, false);
    let backend = make_backend(&fixture.config).unwrap();
    assert_eq!(backend.account_id(), Some("oaiapp_test"));
    // A signed-out remembered account is distinct from a missing account store.
    let path = fixture.config.user_dir.join("chatgpt-accounts.json");
    let mut state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    state["accounts"]["oaiapp_test"]["tokens"] = Value::Null;
    write_private(&path, &serde_json::to_vec(&state).unwrap()).unwrap();
    assert!(make_backend(&fixture.config).is_err());
    assert_eq!(
        llm::catalog::credential_token(&fixture.config.user_dir, OPENAI_URL)
            .unwrap()
            .as_deref(),
        Some("fixture-api-key")
    );
}

fn select_auth(fixture: &mut Fixture, method: llm::model_config::AuthMethod) {
    fixture.config.providers.insert(
        fixture.config.provider.clone().unwrap(),
        llm::model_config::ProviderSpec {
            base_url: fixture.config.endpoint.as_ref().unwrap().base_url.clone(),
            auth: method,
            ..Default::default()
        },
    );
}

#[test]
fn explicit_api_key_keeps_subscription_signed_in_and_bypasses_corrupt_state() {
    let mut fixture = Fixture::new("anthropic", llm::anthropic::BASE_URL);
    fixture.save_key(llm::anthropic::BASE_URL);
    fixture.claude_sign_in();
    select_auth(&mut fixture, llm::model_config::AuthMethod::ApiKey);
    let assert_key_route = || {
        let backend = make_backend(&fixture.config).unwrap();
        assert!(
            matches!(backend.test_provider_kind(), ProviderKind::AnthropicEndpoint { api_key, .. }
            if api_key.as_deref() == Some("fixture-api-key"))
        );
    };
    assert_key_route();
    assert!(llm::anthropic_oauth::signed_in_checked(&fixture.config.user_dir).unwrap());
    write_private(
        &llm::anthropic_oauth::credential_path(&fixture.config.user_dir),
        b"corrupt-sign-in",
    )
    .unwrap();
    assert_key_route();
}

#[test]
fn explicit_subscription_stays_selected_after_signout_despite_saved_key() {
    let mut fixture = Fixture::new("anthropic", llm::anthropic::BASE_URL);
    fixture.save_key(llm::anthropic::BASE_URL);
    fixture.claude_sign_in();
    select_auth(&mut fixture, llm::model_config::AuthMethod::OAuth);
    assert!(uses_claude_subscription(
        &make_backend(&fixture.config).unwrap()
    ));
    llm::anthropic_oauth::sign_out(&fixture.config.user_dir).unwrap();
    // The subscription backend will report missing credentials at dispatch, not bill the API key.
    assert!(uses_claude_subscription(
        &make_backend(&fixture.config).unwrap()
    ));
    assert_eq!(
        llm::catalog::credential_token(&fixture.config.user_dir, llm::anthropic::BASE_URL)
            .unwrap()
            .as_deref(),
        Some("fixture-api-key")
    );
}

#[test]
fn generic_oauth_and_api_key_can_be_selected_independently() {
    let flow = llm::oauth::flow("kimi-code").unwrap();
    let mut fixture = Fixture::new(flow.id, flow.base_url);
    fixture.save_key(flow.base_url);
    let path = fixture.config.user_dir.join("oauth-kimi-code.json");
    write_private(
        &path,
        &serde_json::to_vec(&llm::oauth::Credentials {
            access: "fixture-oauth-access".into(),
            expires_at_ms: u64::MAX,
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();
    select_auth(&mut fixture, llm::model_config::AuthMethod::ApiKey);
    let backend = make_backend(&fixture.config).unwrap();
    assert!(
        matches!(backend.test_provider_kind(), ProviderKind::Endpoint { api_key, .. }
        if api_key.as_deref() == Some("fixture-api-key"))
    );
    assert!(llm::oauth::signed_in(&fixture.config.user_dir, flow.id));
    select_auth(&mut fixture, llm::model_config::AuthMethod::OAuth);
    let assert_signin_route = |config| {
        let backend = make_backend(config).unwrap();
        assert!(matches!(
            backend.test_provider_kind(),
            ProviderKind::Endpoint { api_key: None, .. }
        ));
    };
    assert_signin_route(&fixture.config);
    llm::oauth::sign_out(&fixture.config.user_dir, flow.id).unwrap();
    assert_signin_route(&fixture.config);
    select_auth(&mut fixture, llm::model_config::AuthMethod::ApiKey);
    write_private(&path, b"corrupt-sign-in").unwrap();
    assert!(make_backend(&fixture.config).is_ok());
}

#[test]
fn explicit_subscription_refuses_custom_anthropic_endpoint() {
    let mut fixture = Fixture::new("anthropic", "https://proxy.example/v1");
    fixture.save_key("https://proxy.example/v1");
    select_auth(&mut fixture, llm::model_config::AuthMethod::OAuth);
    assert!(make_backend(&fixture.config).is_err());
}
