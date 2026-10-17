use super::*;
#[test]
fn local_endpoint_and_budgets_are_enforced() {
    assert!(Config::default().validate().is_ok());
    for endpoint in [
        "http://example.com:8001",
        "http://127.0.0.1:8001/path",
        "https://127.0.0.1:8001",
        "http://127.0.0.1:0",
        "http://user@localhost:8001",
    ] {
        assert!(
            Config {
                endpoint: endpoint.into(),
                ..Config::default()
            }
            .validate()
            .is_err()
        );
    }
    assert!(
        Config {
            timeout_secs: 0,
            ..Config::default()
        }
        .validate()
        .is_ok()
    );
}
#[test]
fn an_engine_without_an_implementation_is_refused_before_any_request() {
    let config = Config {
        engine: DecisionEngineType::Kev,
        ..Config::default()
    };
    let error = engine(&config).err().expect("kev has no implementation");
    assert!(error.to_string().contains("`kev`"), "{error:#}");
    assert!(
        predict(
            &config,
            "investigation",
            "hello",
            Arc::new(AtomicBool::new(false))
        )
        .is_err()
    );
    assert!(engine(&Config::default()).is_ok());
}
#[test]
fn foreign_service_is_rejected_instead_of_becoming_a_dependency() {
    let foreign = json!({"service":"another-agent","protocol":1,"status":"ok","checkpoint":"external","device":"cpu"});
    assert!(decode_health(&foreign.to_string(), LAYA_SERVICE).is_err());
    let mut own = foreign;
    own["service"] = json!("dial-laya");
    assert_eq!(
        decode_health(&own.to_string(), LAYA_SERVICE)
            .unwrap()
            .service,
        "dial-laya"
    );
    own["protocol"] = json!(2);
    assert!(decode_health(&own.to_string(), LAYA_SERVICE).is_err());
}
fn ready() -> Health {
    Health {
        busy: false,
        status: "ok".into(),
        checkpoint: expected_checkpoint(&Config::default())
            .unwrap_or_else(|| "test-checkpoint".into()),
        device: "cpu".into(),
        service: "dial-laya".into(),
        protocol: 1,
    }
}
fn refused() -> anyhow::Error {
    std::io::Error::from(std::io::ErrorKind::ConnectionRefused).into()
}

#[test]
fn busy_service_does_not_submit_another_prediction() {
    let mut health = ready();
    health.busy = true;
    let server = crate::llm::fake::serve(vec![crate::llm::fake::json(
        200,
        &serde_json::to_value(health).unwrap(),
    )]);
    let config = Config {
        endpoint: server.url.clone(),
        ..Default::default()
    };
    let error = predict(
        &config,
        "investigation",
        "hello",
        Arc::new(AtomicBool::new(false)),
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("Laya is busy"));
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(crate::llm::fake::request_line(&requests[0]).starts_with("GET /health"));
}

#[test]
fn prediction_body_timeout_is_an_advisory_delay() {
    let server = crate::llm::fake::serve(vec![
        crate::llm::fake::json(200, &serde_json::to_value(ready()).unwrap()),
        crate::llm::fake::Script::ColdStart {
            delay: std::time::Duration::from_millis(1200),
            body: String::new(),
        },
    ]);
    let config = Config {
        endpoint: server.url.clone(),
        timeout_secs: 1,
        ..Default::default()
    };
    let error = predict(
        &config,
        "investigation",
        "hello",
        Arc::new(AtomicBool::new(false)),
    )
    .err()
    .unwrap();
    assert!(
        error.to_string().contains("exceeded its 1s budget"),
        "{error:#}"
    );
    assert!(error.to_string().contains("coding continues"));
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn refused_service_is_started_once_and_existing_service_is_reused() {
    let config = Config::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut probes = 0;
    let mut launches = 0;
    let health = ensure_service_with(
        &config,
        cancel.clone(),
        false,
        || {
            probes += 1;
            if probes == 1 {
                Err(refused())
            } else {
                Ok(ready())
            }
        },
        || {
            launches += 1;
            Ok(Arc::new(AtomicBool::new(false)))
        },
    )
    .unwrap();
    assert_eq!(health.status, "ok");
    assert_eq!(launches, 1);
    ensure_service_with(
        &config,
        cancel,
        false,
        || Ok(ready()),
        || panic!("must reuse running service"),
    )
    .unwrap();
}
#[test]
fn loading_service_is_not_duplicated_and_errors_do_not_trigger_startup() {
    let config = Config::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut probes = 0;
    ensure_service_with(
        &config,
        cancel.clone(),
        false,
        || {
            probes += 1;
            let mut health = ready();
            if probes == 1 {
                health.status = "starting".into();
            }
            Ok(health)
        },
        || panic!("must reuse loading service"),
    )
    .unwrap();
    assert!(
        ensure_service_with(
            &config,
            cancel,
            false,
            || Err(anyhow::anyhow!("invalid response")),
            || panic!("must not replace unhealthy service")
        )
        .is_err()
    );
}
#[test]
fn disabled_startup_cancel_and_early_process_exit_are_observable() {
    let config = Config {
        auto_start: false,
        ..Config::default()
    };
    let cancel = Arc::new(AtomicBool::new(false));
    assert!(
        ensure_service_with(
            &config,
            cancel.clone(),
            false,
            || Err(refused()),
            || panic!("startup disabled")
        )
        .is_err()
    );
    assert!(
        ensure_service_with(
            &Config::default(),
            Arc::new(AtomicBool::new(true)),
            false,
            || Err(refused()),
            || panic!("cancelled")
        )
        .is_err()
    );
    let error = ensure_service_with(
        &config,
        cancel,
        true,
        || Err(refused()),
        || Ok(Arc::new(AtomicBool::new(true))),
    )
    .unwrap_err();
    assert!(error.to_string().contains("exited during startup"));
}
#[test]
#[ignore = "requires the installed Dial Laya service"]
fn installed_service_accepts_dials_native_typed_request() {
    let config = Config {
        enabled: true,
        auto_start: false,
        ..Config::default()
    };
    let decision = predict(
        &config,
        "investigation",
        "The user says hello. No code change or repository lookup is requested.",
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(decision.health.status, "ok");
    assert!(!decision.health.checkpoint.is_empty());
    assert!(matches!(
        decision.answers["retrieval"]["choice"].as_str(),
        Some("none" | "targeted" | "explore")
    ));
}
