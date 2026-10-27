use super::*;

#[test]
fn flow_ids_are_unique_and_named() {
    let ids: std::collections::HashSet<_> = flows().map(|f| f.id).collect();
    assert_eq!(ids.len(), FLOWS.len());
    for flow in flows() {
        assert!(
            !flow.name.is_empty() && flow.base_url.starts_with("https://"),
            "{}",
            flow.id
        );
    }
}

#[test]
fn credentials_expire_with_margin_but_never_expires_holds() {
    let mut c = Credentials {
        access: "a".into(),
        expires_at_ms: util::now_ms() + 30_000,
        ..Default::default()
    };
    assert!(c.expired(REFRESH_MARGIN_MS));
    assert!(!c.expired(0));
    c.expires_at_ms = util::NEVER_EXPIRES;
    assert!(!c.expired(REFRESH_MARGIN_MS));
}

#[test]
fn stored_grants_round_trip_and_sign_out_removes_them() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user");
    let p = path(&user, "kilo");
    let c = Credentials {
        access: "tok".into(),
        expires_at_ms: util::NEVER_EXPIRES,
        email: Some("a@b".into()),
        ..Default::default()
    };
    {
        let _lock = locked(&p).unwrap();
        save(&p, &c).unwrap();
    }
    assert!(signed_in(&user, "kilo"));
    assert_eq!(identity(&user, "kilo").as_deref(), Some("a@b"));
    let access = access(&user, "kilo", &Arc::new(AtomicBool::new(false))).unwrap();
    assert_eq!(access.token.as_str(), "tok");
    sign_out(&user, "kilo").unwrap();
    assert!(!signed_in(&user, "kilo"));
}

#[test]
fn credentials_debug_never_prints_tokens() {
    let c = Credentials {
        access: "secret-a".into(),
        refresh: "secret-r".into(),
        ..Default::default()
    };
    let text = format!("{c:?}");
    assert!(!text.contains("secret"));
}

#[test]
fn a_login_finished_after_sign_out_is_not_published() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user");
    std::fs::create_dir_all(&user).unwrap();
    let flow = flow("kilo").unwrap();
    let generation = sign_out_generation(flow.id);
    // The sign-out lands while the login is still waiting for its callback.
    sign_out(&user, "kilo").unwrap();
    let grant = Credentials {
        access: "tok".into(),
        expires_at_ms: util::NEVER_EXPIRES,
        ..Default::default()
    };
    assert!(publish(&user, flow, generation, &grant).is_err());
    assert!(!signed_in(&user, "kilo"));
}

#[test]
fn a_login_without_a_sign_out_is_published() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user");
    std::fs::create_dir_all(&user).unwrap();
    let flow = flow("devin").unwrap();
    let generation = sign_out_generation(flow.id);
    let grant = Credentials {
        access: "tok".into(),
        expires_at_ms: util::NEVER_EXPIRES,
        ..Default::default()
    };
    publish(&user, flow, generation, &grant).unwrap();
    assert!(signed_in(&user, "devin"));
}

#[test]
fn a_grant_reaches_only_its_canonical_origin_and_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user");
    std::fs::create_dir_all(&user).unwrap();
    let grant = Credentials {
        access: "tok".into(),
        expires_at_ms: util::NEVER_EXPIRES,
        ..Default::default()
    };
    {
        let p = path(&user, "kimi-code");
        let _lock = locked(&p).unwrap();
        save(&p, &grant).unwrap();
    }
    let binding = Binding::for_provider(&user, "kimi-code").unwrap();
    let chat = ApiProtocol::ChatCompletions;
    assert!(
        binding
            .ensure_route(
                false,
                "https://api.kimi.com/coding/v1/chat/completions",
                chat
            )
            .is_ok()
    );
    assert!(
        binding
            .ensure_route(false, "https://attacker.example/v1/chat/completions", chat)
            .is_err()
    );
    assert!(
        binding
            .ensure_route(
                false,
                "http://api.kimi.com/coding/v1/chat/completions",
                chat
            )
            .is_err()
    );
    assert!(
        binding
            .ensure_route(
                false,
                "https://api.kimi.com/coding/v1/chat/completions",
                ApiProtocol::Anthropic
            )
            .is_err()
    );
    // A route the grant supplies itself is trusted; the protocol still has to match.
    assert!(
        binding
            .ensure_route(true, "https://gateway.example/v1/anthropic", chat)
            .is_ok()
    );
}

#[test]
fn explicit_methods_stay_independent_for_every_registered_flow() {
    use super::super::model_config::{AuthMethod, ProviderSpec};
    let home = tempfile::tempdir().unwrap();
    for flow in flows() {
        assert!(!selected_auth(home.path(), flow.id, AuthMethod::ApiKey).unwrap());
        assert!(selected_auth(home.path(), flow.id, AuthMethod::OAuth).unwrap());
        let binding = Binding::selected(home.path(), flow.id).unwrap();
        assert!(binding.access(&Arc::new(AtomicBool::new(false))).is_err());
        write_private(&path(home.path(), flow.id), b"corrupt-grant").unwrap();
        assert!(!selected_auth(home.path(), flow.id, AuthMethod::ApiKey).unwrap());
        assert!(selected_auth(home.path(), flow.id, AuthMethod::OAuth).unwrap());
    }
    assert!(selected_auth(home.path(), "openai", AuthMethod::OAuth).is_err());
    assert!(selected_auth(home.path(), "chatgpt", AuthMethod::ApiKey).is_err());
    let legacy: ProviderSpec = toml::from_str("base_url = 'https://example.test'").unwrap();
    assert_eq!(legacy.auth, AuthMethod::Auto);
    for (text, method) in [
        ("api-key", AuthMethod::ApiKey),
        ("oauth", AuthMethod::OAuth),
    ] {
        let spec: ProviderSpec = toml::from_str(&format!("auth = '{text}'")).unwrap();
        assert_eq!(spec.auth, method);
        assert_eq!(
            toml::Value::try_from(&spec).unwrap()["auth"].as_str(),
            Some(text)
        );
    }
    assert!(toml::from_str::<ProviderSpec>("auth = 'fallback'").is_err());
}

#[test]
fn every_flow_protocol_matches_its_provider_route() {
    // The binding refuses a route whose protocol differs from its flow's, so a flow that disagrees
    // with its provider's built-in protocol makes that signed-in provider unusable.
    for flow in flows() {
        assert_eq!(
            flow.protocol,
            ApiProtocol::builtin(flow.id),
            "{} flow protocol differs from its provider route",
            flow.id
        );
    }
}

#[test]
fn a_gateway_grant_supplies_its_own_route() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user");
    std::fs::create_dir_all(&user).unwrap();
    let mut extra = serde_json::Map::new();
    extra.insert("accountId".into(), serde_json::Value::String("acc".into()));
    extra.insert("gatewayId".into(), serde_json::Value::String("gw".into()));
    let grant = Credentials {
        access: "tok".into(),
        expires_at_ms: util::NEVER_EXPIRES,
        extra,
        ..Default::default()
    };
    {
        let p = path(&user, "cloudflare-ai-gateway");
        let _lock = locked(&p).unwrap();
        save(&p, &grant).unwrap();
    }
    assert_eq!(
        grant_route(&user, "cloudflare-ai-gateway").as_deref(),
        Some("https://gateway.ai.cloudflare.com/v1/acc/gw/anthropic")
    );
    assert!(grant_route(&user, "kimi-code").is_none());
}
