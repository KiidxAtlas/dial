use super::*;
use crate::config::types::HookConfig;

fn code_of(error: anyhow::Error) -> ErrorCode {
    error
        .downcast_ref::<super::super::RequestError>()
        .map_or(ErrorCode::Failed, |refused| refused.code)
}

fn layout() -> (tempfile::TempDir, tempfile::TempDir, Layout) {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    (project, home, layout)
}

fn hook(event: &str, fail_closed: Option<bool>) -> HookConfig {
    HookConfig {
        event: event.into(),
        matcher: None,
        command: vec!["true".into()],
        timeout_secs: 30,
        fail_closed,
    }
}

#[test]
fn hooks_report_their_effective_failure_mode_and_the_usable_events() {
    let config = Config {
        hooks: vec![
            hook("pre_tool_use", None),
            hook("tool_call", None),
            hook("stop", None),
            hook("stop", Some(true)),
        ],
        ..Default::default()
    };
    let listed = hooks(&config).unwrap();
    let modes: Vec<bool> = listed["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["fails_closed"].as_bool().unwrap())
        .collect();
    // `tool_call` is an alias of `pre_tool_use`, so it fails closed by default too.
    assert_eq!(modes, [true, true, false, true]);
    assert_eq!(listed["hooks"][0]["event"], "pre_tool_use");
    assert!(listed["hooks"][0].get("fail_closed").is_none());
    let events = listed["events"].as_array().unwrap();
    assert_eq!(events.len(), crate::config::hooks::EVENTS.len());
}

#[test]
fn configured_ssh_hosts_are_marked_as_dial_entries() {
    let mut config = Config::default();
    config.ssh_hosts.insert(
        "zz-dial-test-box".into(),
        crate::config::types::SshHostConfig {
            host: "10.0.0.9".into(),
            user: Some("ops".into()),
            port: Some(2222),
            key: None,
        },
    );
    let listed = ssh_hosts(&config).unwrap();
    let row = listed["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "zz-dial-test-box")
        .unwrap()
        .clone();
    assert_eq!(row["host"], "10.0.0.9");
    assert_eq!(row["user"], "ops");
    assert_eq!(row["port"], 2222);
    assert_eq!(row["origin"], "dial");
    assert!(row["source"].is_null());
}

#[test]
fn worktrees_mark_the_primary_checkout_this_engine_runs_in() {
    let (project, home, _layout) = layout();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(project.path())
            .env("GIT_CONFIG_GLOBAL", home.path().join("gitconfig"))
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "first",
    ]);
    let listed = worktrees(project.path()).unwrap();
    assert_eq!(listed["repository"], true);
    let rows = listed["worktrees"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["branch"], "main");
    assert_eq!(rows[0]["primary"], true);
    assert_eq!(rows[0]["current"], true);
}

#[test]
fn imports_name_a_source_and_only_accept_listed_transcripts() {
    let (_project, _home, layout) = layout();
    for params in [json!({}), json!({"source": "cursor"}), json!([])] {
        assert_eq!(
            code_of(import_candidates(&layout, &params).unwrap_err()),
            ErrorCode::InvalidParams,
            "{params}"
        );
    }
    assert_eq!(
        code_of(import_session(&layout, &json!({"source": "claude"})).unwrap_err()),
        ErrorCode::InvalidParams
    );
    // A real transcript file that no Claude store lists for this project is still refused.
    let stray = layout.project_root.join("stray.jsonl");
    std::fs::write(&stray, "{}\n").unwrap();
    let refused = import_session(
        &layout,
        &json!({"source": "claude", "path": stray.display().to_string()}),
    )
    .unwrap_err();
    assert_eq!(code_of(refused), ErrorCode::NotFound);
}
