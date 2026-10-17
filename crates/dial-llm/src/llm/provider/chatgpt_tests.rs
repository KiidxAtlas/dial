use super::*;
#[test]
fn remote_silence_limit_preserves_local_worker_allowances() {
    let make = |kind| Backend::new(kind, PathBuf::new(), 600).with_remote_idle_timeout(90);
    for kind in [
        ProviderKind::ChatGpt {
            model: "test".into(),
            user_dir: PathBuf::new(),
        },
        ProviderKind::Endpoint {
            base_url: "https://example.com/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        ProviderKind::AnthropicEndpoint {
            base_url: "https://example.com/v1".into(),
            model: "test".into(),
            api_key: None,
        },
    ] {
        let backend = make(kind);
        assert_eq!(backend.response_timeout(3600), 90);
        assert_eq!(backend.response_timeout(30), 30);
    }
    for kind in [
        ProviderKind::Local {
            model_dir: "local-model".into(),
        },
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:1234/v1".into(),
            model: "test".into(),
            api_key: None,
        },
    ] {
        assert_eq!(make(kind).response_timeout(3600), 3600);
    }
}

#[test]
fn remote_default_bounds_unlimited_workers_while_local_servers_stay_unlimited() {
    let limit = crate::config::types::DEFAULT_REMOTE_IDLE_TIMEOUT_SECS;
    let remote = Backend::new(
        ProviderKind::ChatGpt {
            model: "test".into(),
            user_dir: PathBuf::new(),
        },
        PathBuf::new(),
        600,
    );
    assert_eq!(remote.response_timeout(0), limit);
    let local = Backend::new(
        ProviderKind::Local {
            model_dir: "local-model".into(),
        },
        PathBuf::new(),
        600,
    );
    assert_eq!(local.response_timeout(0), 0);
}
