use super::test_support::FakeGh;
use super::*;

fn args(words: &[&str]) -> Vec<String> {
    strings(words)
}

#[test]
fn valid_repo_accepts_owner_name_and_rejects_the_rest() {
    assert_eq!(
        valid_repo("octo-org/hello.world_1"),
        Ok("octo-org/hello.world_1")
    );
    for bad in ["a", "a/b/c", "-x/y", "a/", "/b", "a/b c", "a/b;rm"] {
        assert!(valid_repo(bad).is_err(), "{bad} should be rejected");
    }
}

#[test]
fn failure_messages_follow_known_mapping() {
    let auth = format_failure(&args(&["api"]), "", "To get started run gh auth login");
    assert_eq!(
        auth,
        "GitHub CLI is not authenticated. Run `gh auth login`."
    );
    let context = format_failure(&args(&["repo", "view"]), "", "no git remotes found");
    assert!(context.starts_with("GitHub repository context is unavailable."));
    assert_eq!(format_failure(&args(&["x"]), "", "  boom \n"), "boom");
    assert_eq!(
        format_failure(&args(&["x", "y"]), "", ""),
        "GitHub CLI command failed: gh x y"
    );
}

#[test]
fn json_failure_appends_api_error_details() {
    let body = r#"{"message":"Validation Failed","errors":[{"resource":"Repo","field":"name","code":"missing"}]}"#;
    let text = format_json_failure(&args(&["api", "x"]), body, "");
    assert!(text.contains("GitHub details:"), "{text}");
    assert!(text.contains("- Repo.name: missing"), "{text}");
}

#[test]
fn clip_keeps_short_text_and_cuts_long_text_on_a_char_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    assert_eq!(clip(&ws, "short".to_owned()), "short");
    let long = "é".repeat(MAX_OUTPUT);
    let cut = clip(&ws, long);
    assert!(cut.contains("[output cut at 20480 bytes;"), "{cut}");
}

#[test]
fn gh_returns_stdout_and_records_argv() {
    let fake = FakeGh::new("{\"ok\":true}\n", "", 0);
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let out = gh(
        &ws,
        &args(&["repo", "view", "--json", "x"]),
        GH_TIMEOUT,
        &cancel,
    )
    .unwrap();
    assert_eq!(out, "{\"ok\":true}\n");
    assert_eq!(fake.last_args(), args(&["repo", "view", "--json", "x"]));
}

#[test]
fn gh_nonzero_exit_surfaces_stderr() {
    let _fake = FakeGh::new("", "could not find repo\n", 1);
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let err = gh(&ws, &args(&["repo", "view"]), GH_TIMEOUT, &cancel).unwrap_err();
    assert_eq!(err, "could not find repo");
}

#[test]
fn gh_json_rejects_empty_and_invalid_output() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    {
        let _fake = FakeGh::new("", "", 0);
        let err = gh_json(&ws, &args(&["x"]), GH_TIMEOUT, &cancel).unwrap_err();
        assert_eq!(err, "GitHub CLI returned empty output.");
    }
    {
        let _fake = FakeGh::new("not json", "", 0);
        let err = gh_json(&ws, &args(&["x"]), GH_TIMEOUT, &cancel).unwrap_err();
        assert_eq!(err, "GitHub CLI returned invalid JSON output.");
    }
}

#[test]
fn parse_repo_ref_accepts_owner_name_and_host_owner_name() {
    assert_eq!(
        parse_repo_ref("octo/repo").unwrap(),
        GhRepo {
            host: None,
            slug: "octo/repo".into()
        }
    );
    assert_eq!(
        parse_repo_ref("ghe.example.com/octo/repo").unwrap(),
        GhRepo {
            host: Some("ghe.example.com".into()),
            slug: "octo/repo".into()
        }
    );
    assert!(parse_repo_ref("a/b/c/d").is_err());
    assert!(parse_repo_ref("/octo/repo").is_err());
    assert!(parse_repo_ref("ghe.example.com/octo/").is_err());
}

#[test]
fn repo_from_url_reads_host_and_slug_and_drops_default_host() {
    assert_eq!(
        repo_from_url("https://github.com/octo/repo.git").unwrap(),
        GhRepo {
            host: None,
            slug: "octo/repo".into()
        }
    );
    assert_eq!(
        repo_from_url("https://ghe.example.com/octo/repo\n").unwrap(),
        GhRepo {
            host: Some("ghe.example.com".into()),
            slug: "octo/repo".into()
        }
    );
    assert!(repo_from_url("git@github.com:octo/repo.git").is_err());
}

#[test]
fn resolve_repo_uses_explicit_value_without_running_gh() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let fake = FakeGh::new("", "", 0);
    assert_eq!(
        resolve_repo(&ws, Some("octo/repo"), &cancel).unwrap(),
        GhRepo {
            host: None,
            slug: "octo/repo".into()
        }
    );
    assert_eq!(
        resolve_repo(&ws, Some("ghe.example.com/octo/repo"), &cancel).unwrap(),
        GhRepo {
            host: Some("ghe.example.com".into()),
            slug: "octo/repo".into()
        }
    );
    assert!(fake.calls().is_empty());
}

#[test]
fn resolve_repo_derives_slug_and_host_from_checkout_url() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let fake = FakeGh::new("https://ghe.example.com/octo/checkout.git\n", "", 0);
    assert_eq!(
        resolve_repo(&ws, None, &cancel).unwrap(),
        GhRepo {
            host: Some("ghe.example.com".into()),
            slug: "octo/checkout".into()
        }
    );
    assert_eq!(
        fake.last_args(),
        args(&["repo", "view", "--json", "url", "-q", ".url"])
    );
}

#[test]
fn gh_repo_argv_helpers_carry_the_host() {
    let remote = GhRepo {
        host: Some("ghe.example.com".into()),
        slug: "octo/repo".into(),
    };
    assert_eq!(remote.qualified(), "ghe.example.com/octo/repo");
    assert_eq!(
        remote.hostname_args(),
        args(&["--hostname", "ghe.example.com"])
    );
    let local = GhRepo {
        host: None,
        slug: "octo/repo".into(),
    };
    assert_eq!(local.qualified(), "octo/repo");
    assert!(local.hostname_args().is_empty());
}
