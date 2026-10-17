use std::sync::atomic::AtomicBool;

use serde_json::{Value, json};

use super::*;
use crate::tools::testkit;
use crate::tools::vcs::gh_common::test_support::FakeGh;

/// 2026-05-10T12:00:00Z
const NOW: i64 = 1_778_414_400_000;

/// Noon UTC on the given date, in ms.
fn noon(year: i64, month: i64, day: i64) -> i64 {
    (days_from_civil(year, month, day) * 86_400 + 43_200) * 1_000
}

fn run_with(op: &str, args: Value) -> Result<ToolOutput, String> {
    let (_dir, ws) = testkit::workspace();
    run_at(op, &ws, &args, &AtomicBool::new(false), NOW)
}

fn assert_lines(text: &str, expected: &[&str]) {
    for line in expected {
        assert!(
            text.lines().any(|found| found == *line),
            "missing {line:?} in:\n{text}"
        );
    }
}

const ISSUES_JSON: &str = r#"{"total_count":1,"incomplete_results":false,"items":[{"number":42,"title":"Crash on save","state":"closed","user":{"login":"octo","name":null},"labels":[{"name":"bug"},{"name":"p1"}],"created_at":"2026-05-01T09:00:00Z","updated_at":"2026-05-02T09:00:00Z","html_url":"https://github.com/acme/widgets/pull/42","repository_url":"https://api.github.com/repos/acme/widgets","pull_request":{"merged_at":"2026-05-02T09:00:00Z"}}]}"#;

const CODE_JSON: &str = r#"{"total_count":1,"items":[{"name":"main.rs","path":"src/main.rs","sha":"0123456789abcdef0123","html_url":"https://github.com/acme/widgets/blob/main/src/main.rs","repository":{"full_name":"acme/widgets"},"text_matches":[{"fragment":"\n    fn main() {\r\n","property":"content"}]}]}"#;

const COMMITS_JSON: &str = r#"{"items":[{"sha":"0123456789abcdef","node_id":"C_1","html_url":"https://github.com/acme/widgets/commit/0123","author":{"login":"octo"},"committer":null,"commit":{"author":{"name":"Octo Cat","email":"o@example.com","date":"2026-05-04T10:00:00Z"},"committer":{"name":"Octo Cat","date":"2026-05-05T10:00:00Z"},"message":"fix: crash on save\n\nlong body"},"repository":{"full_name":"acme/widgets"}}]}"#;

const REPOS_JSON: &str = r#"{"items":[{"full_name":"acme/widgets","description":"A widget\nsecond line","language":"Rust","stargazers_count":12,"forks_count":3,"open_issues_count":1,"archived":false,"fork":false,"private":false,"visibility":"public","updated_at":"2026-05-08T00:00:00Z","created_at":"2024-01-01T00:00:00Z","html_url":"https://github.com/acme/widgets","owner":{"login":"acme"}}]}"#;

#[test]
fn limit_defaults_floors_and_caps() {
    assert_eq!(search_limit(&json!({})).unwrap(), 10);
    assert_eq!(search_limit(&json!({"limit": 500})).unwrap(), 50);
    assert_eq!(search_limit(&json!({"limit": 2.9})).unwrap(), 2);
    assert_eq!(
        search_limit(&json!({"limit": 0})).unwrap_err(),
        "limit must be a positive number"
    );
    assert!(search_limit(&json!({"limit": -1})).is_err());
    assert!(search_limit(&json!({"limit": "5"})).is_err());
}

#[test]
fn relative_bounds_subtract_fixed_units() {
    assert_eq!(parse_date_bound("3d", NOW).unwrap(), "2026-05-07");
    assert_eq!(parse_date_bound("12h", NOW).unwrap(), "2026-05-10");
    assert_eq!(parse_date_bound("30m", NOW).unwrap(), "2026-05-10");
    assert_eq!(parse_date_bound("2w", NOW).unwrap(), "2026-04-26");
    assert_eq!(parse_date_bound(" 3 D ", NOW).unwrap(), "2026-05-07");
}

#[test]
fn relative_month_and_year_roll_like_js_setutc() {
    assert_eq!(parse_date_bound("3mo", NOW).unwrap(), "2026-02-10");
    // setUTCMonth(1) on Mar 31 gives Feb 31, which rolls to Mar 3 in a non-leap year.
    assert_eq!(
        parse_date_bound("1mo", noon(2026, 3, 31)).unwrap(),
        "2026-03-03"
    );
    // setUTCFullYear(2027) on Feb 29 gives Feb 29 2027, which rolls to Mar 1.
    assert_eq!(
        parse_date_bound("1y", noon(2028, 2, 29)).unwrap(),
        "2027-03-01"
    );
}

#[test]
fn absolute_bounds_normalise_to_github_literals() {
    assert_eq!(parse_date_bound("2026-05-09", NOW).unwrap(), "2026-05-09");
    assert_eq!(
        parse_date_bound("2026-05-09T10:00:00.123Z", NOW).unwrap(),
        "2026-05-09T10:00:00Z"
    );
    assert_eq!(
        parse_date_bound("2026-05-09T12:00:00+02:00", NOW).unwrap(),
        "2026-05-09T10:00:00Z"
    );
    assert_eq!(
        parse_date_bound("2026-05-09T10:30-05:00", NOW).unwrap(),
        "2026-05-09T15:30:00Z"
    );
    assert_eq!(
        parse_date_bound("2026-05", NOW).unwrap(),
        "2026-05-01T00:00:00Z"
    );
}

#[test]
fn invalid_bounds_are_errors() {
    assert_eq!(
        parse_date_bound("  ", NOW).unwrap_err(),
        "date bound must not be empty"
    );
    let error = parse_date_bound("soon", NOW).unwrap_err();
    assert!(
        error.starts_with("invalid date bound: soon. Expected"),
        "{error}"
    );
    assert!(parse_date_bound("2026-05-09T25:00Z", NOW).is_err());
    assert!(parse_date_bound("99999999999999999999d", NOW).is_err());
}

#[test]
fn date_qualifier_forms() {
    assert_eq!(
        date_qualifier("created", Some("3d"), Some("1d"), NOW)
            .unwrap()
            .as_deref(),
        Some("created:2026-05-07..2026-05-09")
    );
    assert_eq!(
        date_qualifier("created", Some("3d"), None, NOW)
            .unwrap()
            .as_deref(),
        Some("created:>=2026-05-07")
    );
    assert_eq!(
        date_qualifier("created", None, Some("2026-01-31"), NOW)
            .unwrap()
            .as_deref(),
        Some("created:<=2026-01-31")
    );
    assert_eq!(
        date_qualifier("created", Some(""), None, NOW).unwrap(),
        None
    );
    assert_eq!(date_qualifier("created", None, None, NOW).unwrap(), None);
}

#[test]
fn date_field_per_command() {
    let none = json!({});
    let updated = json!({"dateField": "updated"});
    assert_eq!(date_field("issues", &none).unwrap(), "created");
    assert_eq!(date_field("prs", &updated).unwrap(), "updated");
    assert_eq!(date_field("repos", &updated).unwrap(), "pushed");
    assert_eq!(
        date_field("repos", &json!({"dateField": "created"})).unwrap(),
        "created"
    );
    assert_eq!(date_field("commits", &updated).unwrap(), "committer-date");
    assert!(date_field("issues", &json!({"dateField": "closed"})).is_err());
}

#[test]
fn compose_query_drops_blanks_and_requires_content() {
    assert_eq!(
        compose_query(&[Some(" a "), None, Some(""), Some("b")]).unwrap(),
        "a b"
    );
    assert_eq!(
        compose_query(&[None, Some("  ")]).unwrap_err(),
        "query is required (or pass since/until to filter by date)"
    );
}

#[test]
fn repo_scope_helpers() {
    assert_eq!(split_repo_ref("acme/widgets"), (None, "acme/widgets"));
    assert_eq!(
        split_repo_ref("ghe.example.com/acme/widgets"),
        (Some("ghe.example.com"), "acme/widgets")
    );
    assert_eq!(split_repo_ref("a/b/c/d"), (None, "a/b/c/d"));
    assert!(has_scope_qualifier("fix org:acme"));
    assert!(has_scope_qualifier("-repo:acme/widgets bug"));
    assert!(has_scope_qualifier("OWNER:acme"));
    assert!(!has_scope_qualifier("reporter:octo repo:"));
    assert!(!has_scope_qualifier("docs-repo:x"));
    assert_eq!(
        repo_from_url("https://github.com/acme/widgets/pull/3").as_deref(),
        Some("acme/widgets")
    );
    assert_eq!(
        repo_from_url("https://ghe.example.com/acme/widgets.git").as_deref(),
        Some("ghe.example.com/acme/widgets.git")
    );
    assert_eq!(repo_from_url("not a url"), None);
    assert_eq!(
        repo_from_repository_url("https://api.github.com/repos/acme/widgets").as_deref(),
        Some("acme/widgets")
    );
    assert_eq!(
        repo_from_repository_url("https://ghe.example.com/api/v3/repos/acme/widgets").as_deref(),
        Some("acme/widgets")
    );
    assert_eq!(
        repo_from_repository_url("https://x.example/repos/a/b/c"),
        None
    );
}

#[test]
fn civil_round_trip_and_fixed_now() {
    assert_eq!(civil_from_days(days_from_civil(2026, 5, 10)), (2026, 5, 10));
    assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    assert_eq!(format_date(i128::from(NOW)).as_deref(), Some("2026-05-10"));
}

#[test]
fn api_argv_shape() {
    assert_eq!(
        api_search_args("issues", "fix repo:acme/widgets", 5, None, &[]),
        strings(&[
            "api",
            "-X",
            "GET",
            "/search/issues",
            "-f",
            "q=fix repo:acme/widgets",
            "-F",
            "per_page=5"
        ])
    );
    assert_eq!(
        api_search_args(
            "code",
            "q",
            3,
            Some("ghe.example.com"),
            &[TEXT_MATCH_ACCEPT]
        ),
        strings(&[
            "api",
            "--hostname",
            "ghe.example.com",
            "-X",
            "GET",
            "/search/code",
            "-f",
            "q=q",
            "-F",
            "per_page=3",
            "-H",
            TEXT_MATCH_ACCEPT,
        ])
    );
}

#[test]
fn unknown_op_is_an_error() {
    assert!(run_with("search_nothing", json!({"query": "x"})).is_err());
}

#[test]
fn search_issues_builds_argv_and_formats_results() {
    let fake = FakeGh::new(ISSUES_JSON, "", 0);
    let out = run_with(
        "search_issues",
        json!({"query": "fix bug", "since": "3d", "repo": "acme/widgets", "limit": 5}),
    )
    .unwrap();
    assert!(!out.is_error);
    assert_eq!(
        fake.calls(),
        vec![strings(&[
            "api",
            "-X",
            "GET",
            "/search/issues",
            "-f",
            "q=fix bug created:>=2026-05-07 repo:acme/widgets is:issue",
            "-F",
            "per_page=5",
        ])]
    );
    assert_lines(
        &out.text,
        &[
            "# GitHub issues search",
            "Query: fix bug created:>=2026-05-07",
            "Repository: acme/widgets",
            "Results: 1",
            "- #42 Crash on save",
            "  Repo: acme/widgets",
            "  State: merged",
            "  Author: @octo",
            "  Labels: bug, p1",
            "  Created: 2026-05-01T09:00:00Z",
            "  Updated: 2026-05-02T09:00:00Z",
            "  URL: https://github.com/acme/widgets/pull/42",
        ],
    );
}

#[test]
fn search_issues_without_repo_falls_back_to_checkout_or_no_scope() {
    // `gh repo view` answers with non-URL JSON, so no scope is added and the search still runs.
    let fake = FakeGh::new(r#"{"items":[]}"#, "", 0);
    let out = run_with("search_issues", json!({"query": "fix"})).unwrap();
    let calls = fake.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0][0], "repo");
    assert_eq!(
        calls[1],
        strings(&[
            "api",
            "-X",
            "GET",
            "/search/issues",
            "-f",
            "q=fix is:issue",
            "-F",
            "per_page=10"
        ])
    );
    assert_lines(&out.text, &["Query: fix", "No issues found."]);
    assert!(!out.text.contains("Repository:"));
}

#[test]
fn search_prs_uses_pr_filter_and_updated_field() {
    let fake = FakeGh::new(r#"{"items":[]}"#, "", 0);
    let out = run_with(
        "search_prs",
        json!({"query": "org:acme label:bug", "dateField": "updated", "until": "2026-01-31"}),
    )
    .unwrap();
    assert_eq!(
        fake.calls(),
        vec![strings(&[
            "api",
            "-X",
            "GET",
            "/search/issues",
            "-f",
            "q=org:acme label:bug updated:<=2026-01-31 is:pr",
            "-F",
            "per_page=10",
        ])]
    );
    assert_lines(
        &out.text,
        &[
            "# GitHub pull requests search",
            "Query: org:acme label:bug updated:<=2026-01-31",
            "Results: 0",
            "No pull requests found.",
        ],
    );
}

#[test]
fn search_prs_renders_pull_request_items() {
    let fake = FakeGh::new(ISSUES_JSON, "", 0);
    let out = run_with(
        "search_prs",
        json!({"query": "org:acme", "repo": "acme/widgets"}),
    )
    .unwrap();
    assert_eq!(fake.last_args().last().unwrap(), "per_page=10");
    assert!(
        fake.last_args()
            .contains(&"q=org:acme repo:acme/widgets is:pr".to_owned())
    );
    assert_lines(
        &out.text,
        &[
            "# GitHub pull requests search",
            "Repository: acme/widgets",
            "- #42 Crash on save",
        ],
    );
}

#[test]
fn search_code_passes_text_match_header_and_formats_fragments() {
    let fake = FakeGh::new(CODE_JSON, "", 0);
    let out = run_with(
        "search_code",
        json!({"query": "fn main", "repo": "acme/widgets", "limit": 3}),
    )
    .unwrap();
    assert_eq!(
        fake.calls(),
        vec![strings(&[
            "api",
            "-X",
            "GET",
            "/search/code",
            "-f",
            "q=fn main repo:acme/widgets",
            "-F",
            "per_page=3",
            "-H",
            "Accept: application/vnd.github.text-match+json",
        ])]
    );
    assert_lines(
        &out.text,
        &[
            "# GitHub code search",
            "Query: fn main",
            "- src/main.rs",
            "  Repo: acme/widgets",
            "  Commit: 0123456789ab",
            "  URL: https://github.com/acme/widgets/blob/main/src/main.rs",
            "  Match: fn main() {",
        ],
    );
}

#[test]
fn search_code_enterprise_repo_moves_host_to_hostname_flag() {
    let fake = FakeGh::new(r#"{"items":[]}"#, "", 0);
    let out = run_with(
        "search_code",
        json!({"query": "fn main", "repo": "ghe.example.com/acme/widgets"}),
    )
    .unwrap();
    assert_eq!(
        fake.calls(),
        vec![strings(&[
            "api",
            "--hostname",
            "ghe.example.com",
            "-X",
            "GET",
            "/search/code",
            "-f",
            "q=fn main repo:acme/widgets",
            "-F",
            "per_page=10",
            "-H",
            "Accept: application/vnd.github.text-match+json",
        ])]
    );
    assert_lines(
        &out.text,
        &[
            "Repository: ghe.example.com/acme/widgets",
            "No code matches found.",
        ],
    );
}

#[test]
fn search_code_rejects_dates_and_empty_query() {
    assert!(
        run_with("search_code", json!({"query": "x", "since": "3d"}))
            .unwrap_err()
            .contains("since/until")
    );
    assert_eq!(
        run_with("search_code", json!({})).unwrap_err(),
        "query must not be empty"
    );
    assert_eq!(
        run_with("search_code", json!({"query": "   "})).unwrap_err(),
        "query must not be empty"
    );
}

#[test]
fn search_commits_uses_committer_date_and_formats_commits() {
    let fake = FakeGh::new(COMMITS_JSON, "", 0);
    let out = run_with(
        "search_commits",
        json!({"query": "crash", "since": "2026-05-01", "repo": "acme/widgets", "dateField": "updated"}),
    )
    .unwrap();
    assert_eq!(
        fake.calls(),
        vec![strings(&[
            "api",
            "-X",
            "GET",
            "/search/commits",
            "-f",
            "q=crash committer-date:>=2026-05-01 repo:acme/widgets",
            "-F",
            "per_page=10",
        ])]
    );
    assert_lines(
        &out.text,
        &[
            "# GitHub commits search",
            "Query: crash committer-date:>=2026-05-01",
            "Repository: acme/widgets",
            "Results: 1",
            "- 0123456789ab fix: crash on save",
            "  Repo: acme/widgets",
            "  Author: @octo",
            "  Date: 2026-05-04T10:00:00Z",
            "  URL: https://github.com/acme/widgets/commit/0123",
        ],
    );
}

#[test]
fn search_repos_maps_updated_to_pushed_and_formats_repos() {
    let fake = FakeGh::new(REPOS_JSON, "", 0);
    let out = run_with(
        "search_repos",
        json!({"query": "rust cli", "since": "1w", "dateField": "updated", "limit": 500}),
    )
    .unwrap();
    assert_eq!(
        fake.calls(),
        vec![strings(&[
            "api",
            "-X",
            "GET",
            "/search/repositories",
            "-f",
            "q=rust cli pushed:>=2026-05-03",
            "-F",
            "per_page=50",
        ])]
    );
    assert_lines(
        &out.text,
        &[
            "# GitHub repositories search",
            "Query: rust cli pushed:>=2026-05-03",
            "Results: 1",
            "- acme/widgets",
            "  Description: A widget",
            "  Language: Rust",
            "  Stars: 12",
            "  Forks: 3",
            "  Open issues: 1",
            "  Visibility: public",
            "  Archived: false",
            "  Fork: false",
            "  Updated: 2026-05-08T00:00:00Z",
            "  URL: https://github.com/acme/widgets",
        ],
    );
    assert!(!out.text.contains("Repository:"));
}

#[test]
fn search_failure_is_an_error_not_a_panic() {
    let _fake = FakeGh::new("", "HTTP 422: validation failed", 1);
    assert!(run_with("search_repos", json!({"query": "x"})).is_err());
}
