use super::*;
use crate::storage::memory_backend::test_support::{FakeRunner, authority, scope};

#[test]
fn flush_sends_writes_queued_by_earlier_sessions_and_generations() {
    let server = crate::llm::fake::serve(vec![
        crate::llm::fake::json(200, &json!({})),
        crate::llm::fake::json(200, &json!({"success": true})),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let layout = Layout::with_user_dir(&project, dir.path().join("home")).unwrap();
    let config = HindsightConfig {
        endpoint: server.url.clone(),
        mental_models_enabled: false,
        ..HindsightConfig::default()
    };
    let mut driver = open(&layout, &config).unwrap();
    let earlier = Operation::for_test(authority("session-a"), scope(), None);
    let input = SaveInput {
        content: "the retain queue survives a restart".into(),
        context: None,
        source: None,
        global: false,
    };
    driver
        .save(&earlier, &input, &mut FakeRunner::new(""))
        .unwrap();
    // A later session (or a later history generation) owns the maintenance pass.
    let later = Operation::for_test(authority("session-b"), scope(), None);
    assert_eq!(driver.status(&later).unwrap()["pending"], 1);
    driver.maintain(&later, &mut FakeRunner::new("")).unwrap();
    assert_eq!(driver.status(&later).unwrap()["pending"], 0);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(String::from_utf8_lossy(&requests[1]).contains("the retain queue survives a restart"));
}

#[test]
fn missing_api_key_names_the_variable_to_export() {
    let config = HindsightConfig {
        endpoint: "https://hindsight.example".into(),
        api_key_env: "DIAL_TEST_UNSET_HINDSIGHT_KEY".into(),
        ..HindsightConfig::default()
    };
    let error = credential(&config).unwrap_err().to_string();
    assert!(error.contains("DIAL_TEST_UNSET_HINDSIGHT_KEY"), "{error}");
    assert!(
        error.contains("export") && error.contains("before starting dial"),
        "{error}"
    );
}
