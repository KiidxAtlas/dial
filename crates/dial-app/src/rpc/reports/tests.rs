use super::*;
use crate::storage::checkpoint::TurnVersion;

fn layout() -> (tempfile::TempDir, tempfile::TempDir, Layout) {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    (project, home, layout)
}

fn code(error: anyhow::Error) -> ErrorCode {
    error
        .downcast_ref::<super::super::RequestError>()
        .map_or(ErrorCode::Failed, |refused| refused.code)
}

fn history_on() -> Config {
    let mut config = Config::default();
    config.codebase.enabled = true;
    config.codebase.history = true;
    config
}

#[test]
fn code_history_is_refused_while_the_setting_is_off() {
    let (_project, _home, layout) = layout();
    let mut config = history_on();
    config.codebase.history = false;
    let error = code_history(&layout, &config, &json!({})).unwrap_err();
    assert_eq!(code(error), ErrorCode::Refused);
}

#[test]
fn code_history_searches_and_shows_the_versions_the_terminal_records() {
    let (_project, _home, layout) = layout();
    let config = history_on();
    let store = ChangeHistory::open(&layout).unwrap();
    store
        .record(
            "s1",
            1,
            "fix parser",
            Some("main"),
            &[TurnVersion {
                path: "src/parser.rs".into(),
                in_project: true,
                before: Some(b"old\n".to_vec()),
                after: Some(b"new\n".to_vec()),
            }],
        )
        .unwrap();

    let found = code_history(&layout, &config, &json!({"action":"search","path":"src"})).unwrap();
    let versions = found["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 1, "{found}");
    assert_eq!(versions[0]["path"], "src/parser.rs");
    let id = versions[0]["id"].as_i64().unwrap();

    let shown = code_history(&layout, &config, &json!({"action":"show","id":id})).unwrap();
    assert_eq!(shown["version"]["id"], id);
    let diff = shown["diff"].as_str().unwrap();
    assert!(diff.contains("-old") && diff.contains("+new"), "{diff}");

    let missing = code_history(&layout, &config, &json!({"action":"show","id":id + 99}));
    assert_eq!(code(missing.unwrap_err()), ErrorCode::NotFound);
}

#[test]
fn code_history_rejects_bad_arguments_and_never_clears() {
    let (_project, _home, layout) = layout();
    let config = history_on();
    for params in [
        json!({"action":"search","limit":0}),
        json!({"action":"search","limit":MAX_HISTORY_LIMIT + 1}),
        json!({"action":"search","query":7}),
        json!({"action":"search","path":"x".repeat(MAX_TEXT_BYTES + 1)}),
        json!({"action":"show"}),
        json!({"action":"show","id":"1"}),
        json!({"action":"purge"}),
    ] {
        let error = code_history(&layout, &config, &params).unwrap_err();
        assert_eq!(code(error), ErrorCode::InvalidParams, "{params}");
    }
    let store = ChangeHistory::open(&layout).unwrap();
    store
        .record(
            "s1",
            1,
            "keep",
            None,
            &[TurnVersion {
                path: "a.txt".into(),
                in_project: true,
                before: None,
                after: Some(b"a\n".to_vec()),
            }],
        )
        .unwrap();
    let error = code_history(&layout, &config, &json!({"action":"clear"})).unwrap_err();
    assert_eq!(code(error), ErrorCode::Refused);
    let kept = code_history(&layout, &config, &json!({})).unwrap();
    assert_eq!(
        kept["versions"].as_array().unwrap().len(),
        1,
        "clear must not delete"
    );
}

#[test]
fn stats_reports_an_empty_user_directory_and_rejects_a_bad_range() {
    let (_project, _home, layout) = layout();
    let report = stats(&layout, &json!({"range":"7d"})).unwrap();
    assert_eq!(report["range"], "7d");
    assert_eq!(report["report"]["sessions"]["sessions"], 0, "{report}");
    let error = stats(&layout, &json!({"range":"not a range"})).unwrap_err();
    assert_eq!(code(error), ErrorCode::InvalidParams);
    let error = stats(&layout, &json!({"range":7})).unwrap_err();
    assert_eq!(code(error), ErrorCode::InvalidParams);
}
