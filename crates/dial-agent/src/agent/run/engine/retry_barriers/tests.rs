use super::*;
use serde_json::json;

fn id(byte: u8) -> [u8; 32] {
    [byte; 32]
}

fn dummy_call(name: &str) -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments: json!({}),
        malformed_arguments: false,
    }
}

#[test]
fn service_start_releases_unknown_barriers_without_releasing_job_file_or_access_barriers() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let mut barriers = RetryBarriers::default();

    barriers.record(
        id(1),
        FailureRecord {
            diagnostic: Some("connection refused".into()),
            ..Default::default()
        },
    );
    barriers.record(
        id(2),
        FailureRecord {
            diagnostic: Some("job missing".into()),
            job: Some(("api".into(), "bg\0api".into())),
            ..Default::default()
        },
    );
    barriers.record(
        id(3),
        FailureRecord {
            diagnostic: Some("bad edit".into()),
            file_targets: Some(FileTargets {
                paths: vec![dir.path().join("a.rs")],
                versions: vec![None],
            }),
            ..Default::default()
        },
    );
    barriers.record(
        id(4),
        FailureRecord {
            diagnostic: Some("outside workspace".into()),
            access: Some(("read\0out".into(), vec!["/outside".into()])),
            ..Default::default()
        },
    );

    barriers.release_for_service_start();

    assert!(
        barriers
            .active_diagnostic(&workspace, &dummy_call("bash"), &id(1))
            .is_none(),
        "unknown-dependency barrier should release on service start"
    );
    assert!(
        barriers
            .active_diagnostic(&workspace, &dummy_call("bg"), &id(2))
            .is_some(),
        "job-specific barrier must wait for its own job lifecycle"
    );
    assert!(
        barriers
            .active_diagnostic(&workspace, &dummy_call("edit"), &id(3))
            .is_some(),
        "file-tied barrier must wait for its file"
    );
    assert!(
        barriers
            .active_diagnostic(&workspace, &dummy_call("read"), &id(4))
            .is_some(),
        "access barrier must wait for an explicit grant"
    );

    let released = barriers.release_job_lifecycle(Some("api"));
    assert_eq!(released, vec!["bg\0api"]);
    assert!(
        barriers
            .active_diagnostic(&workspace, &dummy_call("bg"), &id(2))
            .is_none()
    );
}

#[test]
fn preserves_first_original_diagnostic_across_re_recordings() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let mut barriers = RetryBarriers::default();

    barriers.record(
        id(9),
        FailureRecord {
            diagnostic: Some("first error".into()),
            ..Default::default()
        },
    );
    barriers.record(
        id(9),
        FailureRecord {
            diagnostic: Some("second error".into()),
            ..Default::default()
        },
    );

    assert_eq!(
        barriers.active_diagnostic(&workspace, &dummy_call("bash"), &id(9)),
        Some(Some("first error".into()))
    );
}

#[test]
fn eval_reset_releases_only_matching_namespace_atomically() {
    let mut barriers = RetryBarriers::default();
    barriers.record(
        id(10),
        FailureRecord {
            eval: Some((EvalNamespace::Python, "eval\0py".into())),
            ..Default::default()
        },
    );
    barriers.record(
        id(11),
        FailureRecord {
            eval: Some((EvalNamespace::JavaScript, "eval\0js".into())),
            ..Default::default()
        },
    );

    let released = barriers.release_eval_namespace(EvalNamespace::Python);
    assert_eq!(released, vec!["eval\0py"]);
    assert!(!barriers.identities().contains(&id(10)));
    assert!(barriers.identities().contains(&id(11)));
}
