use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;

use serde_json::{Value, json};

use super::checkout::checkout_in;
use super::runner::{Project, check_token_host, parse_project, resolve_program};
use super::test_support::FakeGlab;
use super::*;

fn workspace() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, ws)
}

fn quiet(ws: &Workspace, args: &Value) -> ToolOutput {
    execute(ws, args, &AtomicBool::new(false))
}

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

#[test]
fn tools_registers_glab_mr_once() {
    let tools = tools(&Config::default());
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "glab_mr");
}

#[test]
fn read_ops_need_no_approval_and_writes_do() {
    let tool = GlabMr;
    for op in OPS {
        let args = json!({"op": op});
        let read = READ_OPS.contains(op);
        assert_eq!(tool.needs_approval(&args), !read, "{op}");
        let expected = if read {
            Concurrency::Shared
        } else {
            Concurrency::Exclusive
        };
        assert_eq!(tool.concurrency(&args), expected, "{op}");
    }
    assert!(tool.needs_approval(&json!({})), "missing op needs approval");
}

#[test]
fn only_checkout_modifies_the_workspace() {
    let tool = GlabMr;
    for op in OPS {
        assert_eq!(
            tool.may_modify_workspace(&json!({"op": op})),
            *op == "mr_checkout",
            "{op}"
        );
    }
}

#[test]
fn mr_list_passes_state_limit_and_repo() {
    let fake = FakeGlab::new("[]", "", 0);
    let (_dir, ws) = workspace();
    let out = quiet(
        &ws,
        &json!({"op": "mr_list", "repo": "group/sub/project", "state": "merged", "limit": 5}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.text, "No merge requests match.");
    assert_eq!(
        fake.last_args(),
        argv(&[
            "mr",
            "list",
            "-F",
            "json",
            "--state",
            "merged",
            "--per-page",
            "5",
            "-R",
            "group/sub/project",
        ])
    );
}

#[test]
fn mr_list_renders_untrusted_titles_on_one_line() {
    let body = json!([{
        "iid": 12,
        "title": "Fix\nthe thing",
        "state": "opened",
        "source_branch": "fix",
        "target_branch": "main",
        "author": {"username": "ana"},
        "web_url": "https://gitlab.com/g/p/-/merge_requests/12"
    }])
    .to_string();
    let _fake = FakeGlab::new(&body, "", 0);
    let (_dir, ws) = workspace();
    let out = quiet(&ws, &json!({"op": "mr_list"}));
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.starts_with("[Untrusted GitLab merge requests"));
    assert!(
        out.text
            .contains("!12 [opened] Fix the thing (fix → main) by @ana")
    );
    assert!(!out.text.contains("Fix\nthe"));
}

#[test]
fn mr_view_runs_json_view_and_renders_description() {
    let body = json!({
        "iid": 7,
        "title": "Add thing",
        "state": "opened",
        "draft": true,
        "source_branch": "feat",
        "target_branch": "main",
        "author": {"username": "bo"},
        "description": "Adds the thing."
    })
    .to_string();
    let fake = FakeGlab::new(&body, "", 0);
    let (_dir, ws) = workspace();
    let out = quiet(&ws, &json!({"op": "mr_view", "mr": "7"}));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(fake.last_args(), argv(&["mr", "view", "7", "-F", "json"]));
    assert!(out.text.contains("!7 Add thing [opened draft]"));
    assert!(out.text.contains("Adds the thing."));
}

#[test]
fn mr_view_accepts_an_https_url() {
    let fake = FakeGlab::new("{}", "", 0);
    let (_dir, ws) = workspace();
    let url = "https://gitlab.com/g/p/-/merge_requests/9";
    let out = quiet(&ws, &json!({"op": "mr_view", "mr": url}));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(fake.last_args(), argv(&["mr", "view", url, "-F", "json"]));
}

#[test]
fn mr_diff_uses_raw_diff() {
    let fake = FakeGlab::new("diff --git a/x b/x\n", "", 0);
    let (_dir, ws) = workspace();
    let out = quiet(&ws, &json!({"op": "mr_diff", "mr": "3", "repo": "g/p"}));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(
        fake.last_args(),
        argv(&["mr", "diff", "3", "-R", "g/p", "--raw", "--color", "never"])
    );
    assert!(out.text.contains("diff --git a/x b/x"));
}

#[test]
fn mr_diff_retries_without_raw_when_raw_endpoint_is_missing() {
    let (_dir, ws) = workspace();
    let failing = FakeGlab::new("", "could not obtain raw diff: 404 Not Found\n", 1);
    let out = quiet(&ws, &json!({"op": "mr_diff", "mr": "3"}));
    assert!(out.is_error);
    assert_eq!(
        failing.calls().len(),
        2,
        "raw attempt then plain retry: {:?}",
        failing.calls()
    );
    assert_eq!(
        failing.calls()[1],
        argv(&["mr", "diff", "3", "--color", "never"])
    );
}

#[test]
fn mr_diff_reports_old_glab_without_raw_flag() {
    let _fake = FakeGlab::new("", "unknown flag: --raw\n", 1);
    let (_dir, ws) = workspace();
    let out = quiet(&ws, &json!({"op": "mr_diff", "mr": "3"}));
    assert!(out.is_error);
    assert_eq!(
        out.text,
        "GitLab CLI is too old for `glab mr diff --raw`; upgrade glab."
    );
}

#[test]
fn mr_create_builds_flags_and_fill_is_exclusive_with_title() {
    let fake = FakeGlab::new("https://gitlab.com/g/p/-/merge_requests/4\n", "", 0);
    let (_dir, ws) = workspace();
    let out = quiet(
        &ws,
        &json!({
            "op": "mr_create", "title": "T", "body": "B", "base": "main",
            "head": "feat", "draft": true
        }),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.text, "https://gitlab.com/g/p/-/merge_requests/4");
    assert_eq!(
        fake.last_args(),
        argv(&[
            "mr",
            "create",
            "--yes",
            "--title",
            "T",
            "--description",
            "B",
            "--target-branch",
            "main",
            "--source-branch",
            "feat",
            "--draft",
        ])
    );

    let mixed = quiet(&ws, &json!({"op": "mr_create", "title": "T", "fill": true}));
    assert!(mixed.is_error);
    assert_eq!(fake.calls().len(), 1, "rejected before spawning");
}

#[test]
fn mr_comment_approve_and_merge_argv() {
    let fake = FakeGlab::new("", "", 0);
    let (_dir, ws) = workspace();

    let comment = quiet(&ws, &json!({"op": "mr_comment", "mr": "12", "body": "hi"}));
    assert!(!comment.is_error, "{}", comment.text);
    assert_eq!(
        fake.last_args(),
        argv(&["mr", "note", "12", "--message", "hi"])
    );

    let approve = quiet(&ws, &json!({"op": "mr_approve", "mr": "12", "repo": "g/p"}));
    assert!(!approve.is_error, "{}", approve.text);
    assert_eq!(
        fake.last_args(),
        argv(&["mr", "approve", "12", "-R", "g/p"])
    );

    let merge = quiet(
        &ws,
        &json!({"op": "mr_merge", "mr": "12", "squash": true, "removeSourceBranch": true}),
    );
    assert!(!merge.is_error, "{}", merge.text);
    assert_eq!(
        fake.last_args(),
        argv(&[
            "mr",
            "merge",
            "12",
            "--yes",
            "--squash",
            "--remove-source-branch"
        ])
    );
}

#[test]
fn invalid_arguments_are_rejected_before_spawning() {
    let fake = FakeGlab::new("", "", 0);
    let (_dir, ws) = workspace();
    let rejected = [
        json!({"op": "mr_view", "mr": "abc"}),
        json!({"op": "mr_view", "mr": "0"}),
        json!({"op": "mr_view", "mr": "-R"}),
        json!({"op": "mr_view", "mr": "https://gitlab.com/g/p/ -x"}),
        json!({"op": "mr_view", "mr": "7", "repo": "solo"}),
        json!({"op": "mr_view", "mr": "7", "repo": "g/../p"}),
        json!({"op": "mr_view", "mr": "7", "repo": "g/-p"}),
        json!({"op": "mr_list", "state": "bogus"}),
        json!({"op": "mr_list", "limit": 500}),
        json!({"op": "mr_create"}),
        json!({"op": "mr_comment", "mr": "1", "body": "   "}),
        json!({"op": "mr_checkout", "mr": "https://gitlab.com/g/p/-/merge_requests/1"}),
        json!({"op": "nope"}),
        json!({}),
    ];
    for args in &rejected {
        let out = quiet(&ws, args);
        assert!(out.is_error, "expected rejection for {args}: {}", out.text);
    }
    assert!(
        fake.calls().is_empty(),
        "nothing spawned: {:?}",
        fake.calls()
    );
}

#[test]
fn nonzero_exit_surfaces_glab_message() {
    let _fake = FakeGlab::new("", "404 Project Not Found\n", 1);
    let (_dir, ws) = workspace();
    let out = quiet(&ws, &json!({"op": "mr_view", "mr": "1"}));
    assert!(out.is_error);
    assert_eq!(out.text, "404 Project Not Found");
}

#[test]
fn authentication_failure_gets_login_guidance() {
    let _fake = FakeGlab::new("", "To get started, run glab auth login\n", 1);
    let (_dir, ws) = workspace();
    let out = quiet(&ws, &json!({"op": "mr_approve", "mr": "1"}));
    assert!(out.is_error);
    assert_eq!(
        out.text,
        "GitLab CLI is not authenticated. Run `glab auth login`."
    );
}

#[test]
fn silent_failure_names_the_command() {
    let _fake = FakeGlab::new("", "", 2);
    let (_dir, ws) = workspace();
    let out = quiet(&ws, &json!({"op": "mr_approve", "mr": "1"}));
    assert!(out.is_error);
    assert_eq!(out.text, "GitLab CLI command failed: glab mr approve 1");
}

#[test]
fn missing_glab_reports_install_and_login_steps() {
    let empty = tempfile::tempdir().unwrap();
    let err = resolve_program(Some(empty.path().as_os_str()), "glab").unwrap_err();
    assert!(err.contains("not installed"), "{err}");
    assert!(err.contains("glab auth login"), "{err}");
    assert!(resolve_program(None, "glab").is_err());
}

#[test]
fn resolve_program_finds_named_file_on_path() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("glab");
    fs::write(&exe, "").unwrap();
    let found = resolve_program(Some(dir.path().as_os_str()), "glab").unwrap();
    assert_eq!(found, exe);
}

#[test]
fn parse_project_accepts_path_and_host_forms() {
    let plain = parse_project("group/sub/project").unwrap();
    assert_eq!(plain.host(), None);
    assert_eq!(plain.glab_arg(), "group/sub/project");

    let hosted = parse_project("GitLab.Example.com/group/project").unwrap();
    assert_eq!(hosted.host(), Some("gitlab.example.com"));
    assert_eq!(
        hosted.glab_arg(),
        "https://gitlab.example.com/group/project"
    );

    for bad in [
        "",
        "solo",
        "a/b/",
        "a//b",
        "g/.",
        "g/-x",
        "bad_host.x:port/g/p",
        "https://h/g/p",
    ] {
        assert!(parse_project(bad).is_err(), "{bad:?} should be refused");
    }
}

#[test]
fn token_is_not_sent_to_untrusted_explicit_host() {
    let project = parse_project("evil.example/g/p").unwrap();
    assert!(check_token_host(&project, true, &[]).is_err());
    assert!(check_token_host(&project, true, &["evil.example".to_owned()]).is_ok());
    assert!(check_token_host(&project, false, &[]).is_ok());

    let gitlab = parse_project("gitlab.com/g/p").unwrap();
    assert!(check_token_host(&gitlab, true, &[]).is_ok());

    let plain: Project = parse_project("g/p").unwrap();
    assert!(check_token_host(&plain, true, &[]).is_ok());
}

/// A repository with one commit, usable as the primary checkout for worktree tests.
fn git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    run(&["init", "-q"]);
    run(&[
        "-c",
        "user.name=test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "init",
    ]);
    dir
}

#[test]
fn checkout_creates_detached_worktree_and_runs_glab_in_it() {
    let repo = git_repo();
    let base = tempfile::tempdir().unwrap();
    let fake = FakeGlab::new("Switched to branch feat\n", "", 0);
    let out = checkout_in(repo.path(), base.path(), 7, None, &AtomicBool::new(false)).unwrap();
    assert_eq!(fake.last_args(), argv(&["mr", "checkout", "7"]));
    assert!(out.contains("Checked out merge request !7"), "{out}");
    let worktree = base
        .path()
        .join(crate::tools::vcs::gh_pr::hash_path(repo.path()))
        .join("mr-7");
    assert!(worktree.join(".git").exists(), "worktree created");
}

#[test]
fn checkout_failure_removes_the_worktree_it_created() {
    let repo = git_repo();
    let base = tempfile::tempdir().unwrap();
    let fake = FakeGlab::new("", "merge request not found\n", 1);
    let err = checkout_in(repo.path(), base.path(), 8, None, &AtomicBool::new(false)).unwrap_err();
    assert_eq!(err, "merge request not found");
    let worktree = base
        .path()
        .join(crate::tools::vcs::gh_pr::hash_path(repo.path()))
        .join("mr-8");
    assert!(!worktree.exists(), "rolled back");
    assert_eq!(fake.calls().len(), 1);
}

#[test]
fn checkout_refuses_an_existing_worktree_path() {
    let repo = git_repo();
    let base = tempfile::tempdir().unwrap();
    let existing: &Path = &base
        .path()
        .join(crate::tools::vcs::gh_pr::hash_path(repo.path()))
        .join("mr-9");
    fs::create_dir_all(existing).unwrap();
    let fake = FakeGlab::new("", "", 0);
    let err = checkout_in(repo.path(), base.path(), 9, None, &AtomicBool::new(false)).unwrap_err();
    assert!(err.contains("already exists"), "{err}");
    assert!(fake.calls().is_empty());
}
