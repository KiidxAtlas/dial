use super::*;
fn contract() -> Contract {
    Contract {
        schema_version: 1,
        suite_sha256: "suite".into(),
        source_sha256: "source".into(),
        executable_sha256: "build".into(),
        configuration_sha256: "configuration".into(),
    }
}
#[test]
fn resume_requires_the_exact_execution_contract_and_one_active_owner() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("run.json");
    let recorder = Recorder::open(&path, contract(), false).unwrap();
    assert!(Recorder::open(&path, contract(), true).is_err());
    drop(recorder);
    assert!(Recorder::open(&path, contract(), true).is_ok());
    let mut changed = contract();
    changed.executable_sha256 = "different-build".into();
    assert!(Recorder::open(&path, changed, true).is_err());
    assert!(Recorder::open(&path, contract(), false).is_err());
}
#[test]
fn source_identity_observes_contents_and_configuration_omits_credentials() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.rs");
    fs::write(&path, "first").unwrap();
    let first = source_digest(root.path()).unwrap();
    fs::write(&path, "second").unwrap();
    assert_ne!(first, source_digest(root.path()).unwrap());
    let suite = BenchmarkSuite {
        version: 1,
        name: "fixture".into(),
        cases: vec![],
    };
    let a = Contract::new(
        &suite,
        root.path(),
        &serde_json::json!({"model":"a","api_key":"secret-a"}),
    )
    .unwrap();
    let b = Contract::new(
        &suite,
        root.path(),
        &serde_json::json!({"model":"a","api_key":"secret-b"}),
    )
    .unwrap();
    assert_eq!(a.configuration_sha256, b.configuration_sha256);
    assert!(a.matched_tasks(&b));
    let c = Contract::new(&suite, root.path(), &serde_json::json!({"model":"b"})).unwrap();
    assert_ne!(a.configuration_sha256, c.configuration_sha256);
}
#[test]
fn configuration_digest_keeps_token_budgets_and_drops_only_credentials() {
    let root = tempfile::tempdir().unwrap();
    let suite = BenchmarkSuite {
        version: 1,
        name: "fixture".into(),
        cases: vec![],
    };
    let digest = |value: serde_json::Value| {
        Contract::new(&suite, root.path(), &value)
            .unwrap()
            .configuration_sha256
    };
    let base = serde_json::json!({
        "harness": {"max_tokens": 1000, "history_token_cap": 5000, "num_draft_tokens": 4},
        "api_key_env": "DIAL_KEY",
        "endpoint": {"api_key": "secret-a", "headers": {"authorization": "x"}, "env": {"A": "b"}}
    });
    let mut budget = base.clone();
    budget["harness"]["max_tokens"] = serde_json::json!(2000);
    assert_ne!(digest(base.clone()), digest(budget));
    let mut nested_cap = base.clone();
    nested_cap["harness"]["history_token_cap"] = serde_json::json!(6000);
    assert_ne!(digest(base.clone()), digest(nested_cap));
    let mut credential = base.clone();
    credential["endpoint"]["api_key"] = serde_json::json!("secret-b");
    credential["endpoint"]["env"] = serde_json::json!({"A": "other"});
    assert_eq!(digest(base), digest(credential));
    let mut access = serde_json::json!({"access_token": "one"});
    let plain = digest(serde_json::json!({}));
    assert_eq!(digest(access.clone()), plain);
    access["max_token"] = serde_json::json!(3);
    assert_ne!(digest(access), plain);
}
