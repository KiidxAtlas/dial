use super::*;
use crate::llm::model_config::UiConfig;
use crate::ui::theme::Theme;

fn bash(command: &str, output: &str, state: ToolState) -> Tool {
    Tool {
        id: "call".into(),
        name: "bash".into(),
        summary: String::new(),
        presentation: Default::default(),
        input: serde_json::json!({ "command": command }).to_string(),
        state,
        output: output.into(),
        progress: None,
        elapsed: Some(0.4),
        todo: None,
        agents: Vec::new(),
        outcome: None,
    }
}

fn render(t: &Tool, w: usize, expanded: bool) -> Option<Vec<String>> {
    let theme = Theme::builtin("dial-dark").unwrap();
    let ui = UiConfig::default();
    let ctx = TranscriptCtx {
        theme: &theme,
        ui: &ui,
        tick: 0,
        tools_expanded: expanded,
        expand_key: "ctrl+o",
    };
    git_card(t, w, &ctx).map(|rows| {
        for row in &rows {
            assert!(text::line_width(row) <= w, "{w}: {row:?}");
        }
        rows.iter()
            .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    })
}

const COMMIT: &str = "[main 1a2b3c4d] Fix trailing comma parsing\n 3 files changed, 20 insertions(+), 4 deletions(-)\n create mode 100644 tests/commas.rs\n";
const PUSH: &str = "Enumerating objects: 9, done.\nTo github.com:atlas/dial.git\n   0f0f0f0..1a2b3c4  main -> main\n";

#[test]
fn commit_output_is_parsed_into_hash_message_and_stats() {
    assert_eq!(
        parse_commit(COMMIT),
        Some(Commit {
            hash: "1a2b3c4d".into(),
            branch: "main".into(),
            message: "Fix trailing comma parsing".into(),
            files: Some(3),
            added: Some(20),
            removed: Some(4),
        })
    );
    let root = parse_commit("[main (root-commit) abcdef1] init\n 1 file changed, 1 insertion(+)\n")
        .unwrap();
    assert_eq!(
        (root.hash.as_str(), root.files, root.removed),
        ("abcdef1", Some(1), None)
    );
    assert_eq!(parse_commit("[ok] not a commit\n"), None);
}

#[test]
fn push_output_is_parsed_into_refs_remote_and_range() {
    let push = parse_push(PUSH).unwrap();
    assert_eq!(push.remote, "atlas/dial");
    assert_eq!(push.range, "0f0f0f0..1a2b3c4");
    assert_eq!(push.refs, Some(("main".into(), "main".into())));
    let new =
        parse_push("To https://github.com/atlas/dial.git\n * [new branch]      feat -> feat\n")
            .unwrap();
    assert_eq!(
        (new.remote.as_str(), new.range.as_str()),
        ("atlas/dial", "new branch")
    );
    assert_eq!(
        parse_push("Everything up-to-date\n").unwrap().range,
        "up to date"
    );
}

#[test]
fn a_settled_commit_and_push_read_as_one_row_each() {
    let t = bash(
        "git add -A && git commit -m 'Fix' && git push",
        &format!("{COMMIT}{PUSH}"),
        ToolState::Completed,
    );
    let rows = render(&t, 90, false).unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        rows[0].contains("Commit") && rows[0].contains("1a2b3c4 Fix trailing comma"),
        "{rows:?}"
    );
    assert!(
        rows[0].contains("3 files") && rows[0].contains("+20 −4"),
        "{rows:?}"
    );
    assert!(
        rows[1].contains("Push") && rows[1].contains("main → atlas/dial"),
        "{rows:?}"
    );
    for w in [1, 10, 30, 60] {
        render(&t, w, false);
    }
}

#[test]
fn failures_other_commands_and_expanded_views_keep_the_ordinary_card() {
    assert!(
        render(
            &bash("git commit -m x", COMMIT, ToolState::Failed),
            80,
            false
        )
        .is_none()
    );
    assert!(
        render(
            &bash("git commit -m x", COMMIT, ToolState::Completed),
            80,
            true
        )
        .is_none()
    );
    assert!(render(&bash("git log", COMMIT, ToolState::Completed), 80, false).is_none());
    assert!(
        render(
            &bash("echo git commit", COMMIT, ToolState::Completed),
            80,
            false
        )
        .is_none()
    );
    assert!(
        render(
            &bash("git commit -m x", "nothing to commit", ToolState::Completed),
            80,
            false
        )
        .is_none()
    );
}
