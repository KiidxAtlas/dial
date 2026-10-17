use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde_json::json;

use super::*;
use crate::storage::artifact::ArtifactStore;
use crate::tools::gh_common::test_support::GhOverride;

/// A `gh` for one test, routed through `GhOverride`. It answers an invocation whose argv ends
/// with a route suffix with that route's body, and records each argv on its own line.
struct FakeGh {
    dir: tempfile::TempDir,
    _override: GhOverride,
}

impl FakeGh {
    fn install(routes: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n",
            root.join("argv").display()
        );
        for (index, (suffix, body)) in routes.iter().enumerate() {
            let file = root.join(format!("response{index}"));
            fs::write(&file, body).unwrap();
            script.push_str(&format!(
                "  *'{suffix}') cat '{}'; exit 0 ;;\n",
                file.display()
            ));
        }
        script.push_str("esac\necho \"unexpected gh call: $*\" >&2\nexit 1\n");
        let bin = root.join("gh");
        fs::write(&bin, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Windows has no file mode bits, so the chmod step is skipped there; the fake is a
        // POSIX shell script and only runs on Unix.
        Self {
            dir,
            _override: GhOverride::install(&bin),
        }
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("argv"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn workspace(root: &Path) -> Workspace {
    Workspace::new(root).unwrap()
}

const RUN_FAILED: &str = r#"{"id":42,"name":"CI","display_title":"Fix build","status":"completed","conclusion":"failure","head_branch":"main","head_sha":"0123456789abcdef0123","created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:05:00Z","html_url":"https://github.com/o/r/actions/runs/42"}"#;
const JOBS_FAILED: &str = r#"{"total_count":2,"jobs":[{"id":7,"name":"build","status":"completed","conclusion":"failure","started_at":"2024-01-01T00:00:10Z","completed_at":"2024-01-01T00:04:00Z","html_url":"https://github.com/o/r/actions/runs/42/job/7"},{"id":8,"name":"lint","status":"completed","conclusion":"success","started_at":"2024-01-01T00:00:10Z","completed_at":"2024-01-01T00:01:00Z","html_url":"https://github.com/o/r/actions/runs/42/job/8"}]}"#;
const RUN_PASSED: &str = r#"{"id":42,"name":"CI","display_title":"Fix build","status":"completed","conclusion":"success","head_branch":"main","head_sha":"0123456789abcdef0123","created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:05:00Z","html_url":"https://github.com/o/r/actions/runs/42"}"#;
const JOBS_PASSED: &str = r#"{"total_count":1,"jobs":[{"id":8,"name":"lint","status":"completed","conclusion":"success","started_at":"2024-01-01T00:00:10Z","completed_at":"2024-01-01T00:01:00Z","html_url":"https://github.com/o/r/actions/runs/42/job/8"}]}"#;
const LOG_BUILD: &str = "compiling\nerror: boom\nlast line\n";

const JOBS_PATH: &str = "/actions/runs/42/jobs -F per_page=100 -F page=1";

fn expected_failed_text() -> String {
    [
        "# GitHub Actions Run #42",
        "",
        "Repository: o/r",
        "Workflow: CI",
        "Title: Fix build",
        "Branch: main",
        "Status: completed",
        "Conclusion: failure",
        "Created: 2024-01-01T00:00:00Z",
        "Updated: 2024-01-01T00:05:00Z",
        "URL: https://github.com/o/r/actions/runs/42",
        "",
        "## Jobs (2)",
        "",
        "- [failure] build",
        "  Started: 2024-01-01T00:00:10Z",
        "  Completed: 2024-01-01T00:04:00Z",
        "  URL: https://github.com/o/r/actions/runs/42/job/7",
        "- [success] lint",
        "  Started: 2024-01-01T00:00:10Z",
        "  Completed: 2024-01-01T00:01:00Z",
        "  URL: https://github.com/o/r/actions/runs/42/job/8",
        "",
        "## Failed Jobs",
        "",
        "### build [failure]",
        "Run: #42",
        "Workflow: CI",
        "Started: 2024-01-01T00:00:10Z",
        "Completed: 2024-01-01T00:04:00Z",
        "URL: https://github.com/o/r/actions/runs/42/job/7",
        "",
        "Last 2 log lines:",
        "```text",
        "error: boom",
        "last line",
        "```",
        "",
        "Run failed.",
    ]
    .join("\n")
}

#[test]
fn parses_run_ids_and_urls() {
    assert_eq!(
        parse_run_reference(Some(" 42 ")).unwrap(),
        RunRef {
            repo: None,
            run_id: Some(42)
        }
    );
    assert_eq!(
        parse_run_reference(Some("https://github.com/o/r/actions/runs/99/job/5")).unwrap(),
        RunRef {
            repo: Some("github.com/o/r".to_owned()),
            run_id: Some(99)
        }
    );
    assert_eq!(parse_run_reference(None).unwrap(), RunRef::default());
    assert_eq!(
        parse_run_reference(Some("nope")).unwrap_err(),
        RUN_REF_ERROR
    );
}

#[test]
fn resolves_tail_limit() {
    assert_eq!(resolve_tail_limit(&json!({})).unwrap(), 15);
    assert_eq!(resolve_tail_limit(&json!({"tail": 2.9})).unwrap(), 2);
    assert_eq!(resolve_tail_limit(&json!({"tail": 500})).unwrap(), 200);
    assert_eq!(
        resolve_tail_limit(&json!({"tail": 0})).unwrap_err(),
        "tail must be a positive number"
    );
}

#[test]
fn parses_github_timestamps() {
    assert_eq!(
        parse_timestamp_ms("2024-01-01T00:00:10Z"),
        Some(1_704_067_210_000)
    );
    assert_eq!(parse_timestamp_ms("not a time"), None);
}

#[test]
fn completed_failed_run_reports_tail_and_calls_gh_in_order() {
    let fake = FakeGh::install(&[
        ("/actions/jobs/7/logs", LOG_BUILD),
        (JOBS_PATH, JOBS_FAILED),
        ("/actions/runs/42", RUN_FAILED),
    ]);
    let work = tempfile::tempdir().unwrap();
    let ws = workspace(work.path());
    let cancel = AtomicBool::new(false);
    let out = run_with_progress(
        "run_watch",
        &ws,
        &json!({"run": "42", "repo": "o/r", "tail": 2}),
        &cancel,
        &mut |_: &str| {},
    )
    .unwrap();

    assert!(!out.is_error);
    assert_eq!(out.text, expected_failed_text());
    assert_eq!(
        fake.calls(),
        vec![
            "api --method GET /repos/o/r/actions/runs/42",
            "api --method GET /repos/o/r/actions/runs/42/jobs -F per_page=100 -F page=1",
            "api /repos/o/r/actions/jobs/7/logs",
        ]
    );
    let data = out.data.unwrap();
    assert_eq!(data["runId"], json!(42));
    assert_eq!(
        data["watch"]["failedLogs"][0]["tail"],
        json!("error: boom\nlast line")
    );
    assert_eq!(
        data["sourceUrl"],
        json!("https://github.com/o/r/actions/runs/42")
    );
    assert!(data.get("artifactId").is_none());
}

#[test]
fn completed_passing_run_reports_all_jobs_passed_without_logs() {
    let fake = FakeGh::install(&[(JOBS_PATH, JOBS_PASSED), ("/actions/runs/42", RUN_PASSED)]);
    let work = tempfile::tempdir().unwrap();
    let ws = workspace(work.path());
    let cancel = AtomicBool::new(false);
    let out = run_with_progress(
        "run_watch",
        &ws,
        &json!({"run": "42", "repo": "o/r"}),
        &cancel,
        &mut |_: &str| {},
    )
    .unwrap();

    assert!(out.text.ends_with("All jobs passed."));
    assert_eq!(fake.calls().len(), 2);
    assert_eq!(out.data.unwrap()["watch"]["failedLogs"], json!([]));
}

#[test]
fn cancel_before_first_poll_makes_no_gh_call() {
    let fake = FakeGh::install(&[]);
    let work = tempfile::tempdir().unwrap();
    let ws = workspace(work.path());
    let cancel = AtomicBool::new(true);
    let err = run_with_progress(
        "run_watch",
        &ws,
        &json!({"run": "42", "repo": "o/r"}),
        &cancel,
        &mut |_: &str| {},
    )
    .unwrap_err();

    assert_eq!(err, CANCELLED);
    assert!(fake.calls().is_empty());
}

#[test]
fn failed_logs_are_retained_as_artifact_when_store_exists() {
    let _fake = FakeGh::install(&[
        ("/actions/jobs/7/logs", LOG_BUILD),
        (JOBS_PATH, JOBS_FAILED),
        ("/actions/runs/42", RUN_FAILED),
    ]);
    let work = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let ws = workspace(work.path()).with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let cancel = AtomicBool::new(false);
    let out = run_with_progress(
        "run_watch",
        &ws,
        &json!({"run": "42", "repo": "o/r"}),
        &cancel,
        &mut |_: &str| {},
    )
    .unwrap();

    let data = out.data.unwrap();
    let id = data["artifactId"].as_str().unwrap();
    assert!(!id.is_empty());
    assert!(
        out.text
            .ends_with(&format!("\n\nFull failed-job logs: artifact://{id}"))
    );
}
