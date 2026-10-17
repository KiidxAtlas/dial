use super::*;

fn store() -> (tempfile::TempDir, tempfile::TempDir, ChangeHistory) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let history = ChangeHistory::open(&layout).unwrap();
    (home, project, history)
}

fn version(path: &str, before: Option<&str>, after: Option<&str>) -> TurnVersion {
    TurnVersion {
        path: path.into(),
        in_project: true,
        before: before.map(|s| s.as_bytes().to_vec()),
        after: after.map(|s| s.as_bytes().to_vec()),
    }
}

#[test]
fn records_searchable_diffs_and_skips_unchanged_private_and_outside_files() {
    let (_home, _project, history) = store();
    let mut outside = version("/elsewhere/x.rs", Some("a"), Some("b"));
    outside.in_project = false;
    let stored = history
        .record(
            "s1",
            1,
            "fix the parser",
            Some("main"),
            &[
                version(
                    "src/parser.rs",
                    Some("fn parse() {}\n"),
                    Some("fn parse_token() {}\n"),
                ),
                version("src/new.rs", None, Some("pub fn fresh() {}\n")),
                version("same.rs", Some("x"), Some("x")),
                version(".env", Some("A=1"), Some("A=2")),
                outside,
            ],
        )
        .unwrap();
    assert_eq!(stored, 2);
    history
        .record(
            "s2",
            1,
            "rename again",
            Some("feature"),
            &[version(
                "src/parser.rs",
                Some("fn parse_token() {}\n"),
                None,
            )],
        )
        .unwrap();

    let by_word = history.search("parse_token", None, None, 10).unwrap();
    assert_eq!(by_word.len(), 2);
    let feature = history.search("", None, Some("feature"), 10).unwrap();
    assert_eq!(feature.len(), 1);
    assert_eq!(feature[0].change, "deleted");
    let in_src = history.search("", Some("src/"), None, 10).unwrap();
    assert_eq!(
        in_src.iter().map(|v| v.path.as_str()).collect::<Vec<_>>(),
        ["src/parser.rs", "src/new.rs", "src/parser.rs"]
    );
    assert!(history.search("", Some("sr"), None, 10).unwrap().is_empty());

    let modified = in_src.iter().find(|v| v.change == "modified").unwrap();
    assert_eq!(
        (modified.added, modified.removed, modified.label.as_str()),
        (1, 1, "fix the parser")
    );
    let (_, diff) = history.show(modified.id).unwrap().unwrap();
    assert!(diff.contains("--- a/src/parser.rs") && diff.contains("+fn parse_token() {}"));
    assert!(render_list(&in_src).contains("main · src/parser.rs (modified +1 -1)"));

    assert_eq!(history.clear().unwrap(), 3);
    assert!(history.search("", None, None, 10).unwrap().is_empty());
}

#[test]
fn rerecording_a_turn_replaces_rows_and_drops_reverted_files() {
    let (_home, _project, history) = store();
    history
        .record(
            "s",
            3,
            "edit",
            None,
            &[version("a.rs", Some("1"), Some("2"))],
        )
        .unwrap();
    history
        .record(
            "s",
            3,
            "edit",
            None,
            &[version("a.rs", Some("1"), Some("1"))],
        )
        .unwrap();
    assert!(history.search("", None, None, 10).unwrap().is_empty());
    assert!(history.search("edit", None, None, 10).unwrap().is_empty());
}

#[test]
fn binary_large_and_secret_content_is_bounded() {
    let (_home, _project, mut history) = store();
    history.max_versions = 2;
    let big = "line of text\n".repeat(10_000);
    let key = format!("token = \"ghp_{}\"\n", "a1B2".repeat(10));
    let binary = TurnVersion {
        path: "img.png".into(),
        in_project: true,
        before: None,
        after: Some(vec![0, 159, 146, 150]),
    };
    history
        .record(
            "s",
            1,
            "big",
            None,
            &[
                binary,
                version("big.txt", None, Some(&big)),
                version("key.rs", None, Some(&key)),
            ],
        )
        .unwrap();
    let all = history.search("", None, None, 10).unwrap();
    assert_eq!(all.len(), 2, "oldest version pruned beyond the cap");
    let large = all.iter().find(|v| v.path == "big.txt").unwrap();
    let (_, diff) = history.show(large.id).unwrap().unwrap();
    assert!(
        large.truncated
            && diff.len() <= MAX_DIFF_BYTES + 32
            && diff.ends_with("[diff truncated]\n")
    );
    let secret = all.iter().find(|v| v.path == "key.rs").unwrap();
    let (_, diff) = history.show(secret.id).unwrap().unwrap();
    assert!(!diff.contains("ghp_a1B2"), "{diff}");
    history.max_versions = 10;
    history
        .record(
            "s",
            2,
            "bin",
            None,
            &[TurnVersion {
                path: "img.png".into(),
                in_project: true,
                before: None,
                after: Some(vec![0, 1]),
            }],
        )
        .unwrap();
    let image = &history.search("", Some("img.png"), None, 10).unwrap()[0];
    assert!(image.binary);
    assert!(render_version(image, "").contains("no diff was stored"));
}

#[test]
fn parses_slash_command_requests() {
    assert_eq!(parse_request("show #12"), Ok(Request::Show(12)));
    assert!(parse_request("show twelve").is_err() && parse_request("show").is_err());
    assert_eq!(parse_request(" clear "), Ok(Request::Clear));
    assert_eq!(
        parse_request("retry backoff path:src/llm branch:main"),
        Ok(Request::Search {
            query: "retry backoff".into(),
            path: Some("src/llm".into()),
            branch: Some("main".into()),
        })
    );
    assert_eq!(
        parse_request(""),
        Ok(Request::Search {
            query: String::new(),
            path: None,
            branch: None
        })
    );
}
