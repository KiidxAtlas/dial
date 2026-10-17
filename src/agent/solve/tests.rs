use crate::config::solve::EscalationConfig;
use crate::config::solve::SolveConfig;

use super::*;
fn state(text: &str) -> FileState {
    FileState {
        bytes: text.as_bytes().to_vec(),
        mode: 0o644,
    }
}
#[test]
fn verifier_checks_real_exit_status_and_cannot_modify_candidate_or_source() {
    if !tools::sandbox::require_for_test() {
        return;
    }
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("file"), "user work").unwrap();
    let candidate = crate::workspace_copy::clone_project(source.path()).unwrap();
    fs::write(candidate.path().join("file"), "candidate").unwrap();
    let expected = tree(candidate.path(), 1000).unwrap();
    let interrupt = std::sync::atomic::AtomicBool::new(false);
    let pass = [
        Check {
            command: "test \"$(cat file)\" = candidate && printf changed > file".into(),
            timeout_secs: 5,
        },
        Check {
            command: "test \"$(cat file)\" = candidate".into(),
            timeout_secs: 5,
        },
    ];
    assert!(
        verify(
            candidate.path(),
            &expected,
            &pass,
            1000,
            &interrupt,
            &mut super::super::NullUi
        )
        .is_ok()
    );
    assert_eq!(tree(candidate.path(), 1000).unwrap(), expected);
    let fail = [Check {
        command: "printf '[exit code: 0]\\n'; exit 1".into(),
        timeout_secs: 5,
    }];
    assert!(
        verify(
            candidate.path(),
            &expected,
            &fail,
            1000,
            &interrupt,
            &mut super::super::NullUi
        )
        .is_err()
    );
    let escape = [Check {
        command: format!(
            "printf stolen > '{}'",
            source
                .path()
                .join("file")
                .display()
                .to_string()
                .replace('\'', "'\\''")
        ),
        timeout_secs: 5,
    }];
    assert!(
        verify(
            candidate.path(),
            &expected,
            &escape,
            1000,
            &interrupt,
            &mut super::super::NullUi
        )
        .is_err()
    );
    assert_eq!(
        fs::read_to_string(source.path().join("file")).unwrap(),
        "user work"
    );
    assert!(
        verify(
            candidate.path(),
            &expected,
            &[],
            1000,
            &interrupt,
            &mut super::super::NullUi
        )
        .is_err()
    );
}
#[test]
fn zero_limits_allow_candidates_and_snapshot_bytes_without_relaxing_paths() {
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("file"), "content").unwrap();
    assert_eq!(tree(source.path(), 0).unwrap().len(), 1);
    assert!(tree(source.path(), 1).is_err());
    let request = SearchRequest {
        goal: "fix".into(),
        allowed_paths: vec!["file".into()],
        candidates: vec!["approach".into(); 5],
        checks: vec![Check {
            command: "true".into(),
            timeout_secs: 1,
        }],
    };
    assert!(request.validate(&SolveConfig::default()).is_ok());
    assert!(
        request
            .validate(&SolveConfig {
                max_candidates: 2,
                ..SolveConfig::default()
            })
            .is_err()
    );
    assert!(relative_file(Path::new("../file")).is_err());
    assert_eq!(EscalationConfig::default().max_evidence_bytes, 0);
}

#[test]
fn rejects_out_of_scope_patches_even_when_allowed_file_also_changed() {
    let before = Tree::from([
        ("file".into(), state("original")),
        ("other".into(), state("user work")),
    ]);
    let mut after = before.clone();
    after.insert("file".into(), state("candidate"));
    after.insert("other".into(), state("unrelated"));
    assert!(scoped_patch(&before, &after, &["file".into()]).is_err());
    assert!(scoped_patch(&before, &before, &["file".into()]).is_err());
}
#[test]
fn copies_preserve_dirty_work_and_isolate_candidates() {
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("file"), "dirty user work").unwrap();
    fs::write(source.path().join("untracked"), "keep").unwrap();
    let first = crate::workspace_copy::clone_project(source.path()).unwrap();
    let second = crate::workspace_copy::clone_project(source.path()).unwrap();
    fs::write(first.path().join("file"), "candidate").unwrap();
    assert_eq!(
        fs::read_to_string(second.path().join("file")).unwrap(),
        "dirty user work"
    );
    assert_eq!(
        fs::read_to_string(source.path().join("file")).unwrap(),
        "dirty user work"
    );
    assert_eq!(
        fs::read_to_string(source.path().join("untracked")).unwrap(),
        "keep"
    );
}
#[test]
fn stale_source_refuses_whole_patch() {
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("file"), "original").unwrap();
    let ws = Workspace::new(source.path()).unwrap();
    let before = tree(source.path(), 1000).unwrap();
    let mut after = before.clone();
    after.insert("file".into(), state("candidate"));
    fs::write(source.path().join("untracked"), "new user work").unwrap();
    assert!(
        apply(&ws, &before, &after, &["file".into()], 1000)
            .unwrap_err()
            .to_string()
            .contains("source changed")
    );
    assert_eq!(
        fs::read_to_string(source.path().join("file")).unwrap(),
        "original"
    );
}
#[test]
fn applies_scoped_create_modify_delete_preserving_user_files() {
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("edit"), "user original").unwrap();
    fs::write(source.path().join("remove"), "remove").unwrap();
    fs::write(source.path().join("keep"), "unrelated user work").unwrap();
    let ws = Workspace::new(source.path()).unwrap();
    let before = tree(source.path(), 1000).unwrap();
    let mut after = before.clone();
    after.insert("edit".into(), state("new"));
    after.insert("create".into(), state("created"));
    after.remove(Path::new("remove"));
    apply(&ws, &before, &after, &changed(&before, &after), 1000).unwrap();
    assert_eq!(tree(source.path(), 1000).unwrap(), after);
    assert_eq!(
        fs::read_to_string(source.path().join("keep")).unwrap(),
        "unrelated user work"
    );
}
#[test]
fn smallest_patch_counts_changed_bytes_not_whole_file() {
    let before = Tree::from([(
        "file".into(),
        state("long unchanged prefix old long unchanged suffix"),
    )]);
    let small = Tree::from([(
        "file".into(),
        state("long unchanged prefix new long unchanged suffix"),
    )]);
    let big = Tree::from([("file".into(), state("rewrite"))]);
    assert!(
        patch_size(&before, &small, &["file".into()]) < patch_size(&before, &big, &["file".into()])
    );
}
#[test]
fn empty_checks_and_escaping_scope_fail_closed() {
    let mut request: SearchRequest = serde_json::from_value(
        json!({"goal":"fix","allowed_paths":["file"],"candidates":["small"],"checks":[]}),
    )
    .unwrap();
    assert!(request.validate(&SolveConfig::default()).is_err());
    request.checks.push(Check {
        command: "true".into(),
        timeout_secs: 1,
    });
    request.allowed_paths = vec!["../file".into()];
    assert!(request.validate(&SolveConfig::default()).is_err());
}
#[test]
fn symlinks_cannot_return_to_source() {
    let source = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), source.path().join("link")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(outside.path(), source.path().join("link")).unwrap();
    assert!(confined_destination(source.path(), Path::new("link/file")).is_err());
    assert!(tree(source.path(), 1000).is_err());
}
#[test]
fn exact_route_privacy_is_local_by_default() {
    assert!(!EscalationConfig::default().allow_remote);
    for url in [
        "https://localhost.attacker.invalid/v1",
        "https://127.0.0.1.attacker.invalid/v1",
        "https://private.local/v1",
        "http://192.168.1.2/v1",
        "http://user@127.0.0.1/v1",
        "http://127.0.0.1/v1#fragment",
        "ftp://127.0.0.1/v1",
    ] {
        let mut config = Config::default();
        config.endpoint = Some(crate::config::types::EndpointConfig {
            headers: None,
            base_url: url.into(),
            api_key_env: None,
            allow_insecure: false,
            protocol: None,
            account_id: None,
        });
        assert!(local_route(config, false).is_err(), "{url}");
    }
    for url in [
        "http://127.0.0.1:1234/v1",
        "http://[::1]:1234/v1",
        "http://localhost:1234/v1",
    ] {
        let mut config = Config::default();
        config.endpoint = Some(crate::config::types::EndpointConfig {
            headers: None,
            base_url: url.into(),
            api_key_env: None,
            allow_insecure: false,
            protocol: None,
            account_id: None,
        });
        let accepted = local_route(config, false).unwrap();
        assert!(!accepted.endpoint.unwrap().base_url.contains("localhost"));
    }
    let remote = Config {
        endpoint: Some(crate::config::types::EndpointConfig {
            headers: None,
            base_url: "https://models.example.test/v1".into(),
            api_key_env: None,
            allow_insecure: false,
            protocol: None,
            account_id: None,
        }),
        ..Config::default()
    };
    assert!(local_route(remote.clone(), false).is_err());
    assert!(local_route(remote, true).is_ok());
    let subscription = Config {
        provider: Some(crate::llm::chatgpt::PROVIDER.into()),
        ..Config::default()
    };
    assert!(local_route(subscription, false).is_err());
    let local = Config {
        provider: Some("mlx".into()),
        ..Config::default()
    };
    assert!(local_route(local, false).is_ok());
}
