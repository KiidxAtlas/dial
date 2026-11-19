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

const STATUS: &str = "On branch main\nYour branch is ahead of 'origin/main' by 2 commits.\n\nChanges to be committed:\n\tnew file:   src/new.rs\n\nChanges not staged for commit:\n\tmodified:   src/parser.rs\n\tdeleted:    old.txt\n\nUntracked files:\n\tnotes.md\n\n[exit code: 0]";

const DIFF: &str = "diff --git a/src/parser.rs b/src/parser.rs\nindex 1..2 100644\n--- a/src/parser.rs\n+++ b/src/parser.rs\n@@ -1,4 +1,5 @@\n fn parse() {\n-    old();\n+    new();\n+    more();\n--- a SQL comment that was removed\n }\ndiff --git a/logo.png b/logo.png\nBinary files a/logo.png and b/logo.png differ\n";

#[test]
fn long_and_short_status_are_parsed_into_branch_tracking_and_files() {
    let long = parse_status(STATUS, false).unwrap();
    assert_eq!(long.branch.as_deref(), Some("main"));
    assert_eq!((long.ahead, long.behind), (2, 0));
    assert_eq!((long.staged, long.changed, long.untracked), (1, 2, 1));
    assert_eq!(
        long.files,
        vec![
            ('A', "src/new.rs".into()),
            ('M', "src/parser.rs".into()),
            ('D', "old.txt".into()),
            ('?', "notes.md".into()),
        ]
    );
    let diverged = parse_status(
        "On branch x\nYour branch and 'origin/x' have diverged,\nand have 3 and 1 different commits each, respectively.\nnothing to commit, working tree clean\n",
        false,
    )
    .unwrap();
    assert_eq!((diverged.ahead, diverged.behind), (3, 1));
    assert!(diverged.files.is_empty());

    let short = parse_status(
        "## main...origin/main [ahead 1, behind 4]\nM  a.rs\n M b.rs\nUU c.rs\n?? d.rs\n",
        true,
    )
    .unwrap();
    assert_eq!(short.branch.as_deref(), Some("main"));
    assert_eq!((short.ahead, short.behind), (1, 4));
    assert_eq!(
        (
            short.staged,
            short.changed,
            short.conflicts,
            short.untracked
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(parse_status("fatal: not a git repository\n", false), None);
    assert_eq!(parse_status("not a status line\n", true), None);
}

#[test]
fn diff_counts_lines_per_file_and_refuses_cut_output() {
    let files = parse_diff(DIFF).unwrap();
    assert_eq!(files.len(), 2);
    // `--- a SQL comment` inside a hunk is a removed line, not a file header.
    assert_eq!(
        (files[0].path.as_str(), files[0].added, files[0].removed),
        ("src/parser.rs", 2, 2)
    );
    assert!(files[1].binary && files[1].path == "logo.png");
    let cut = format!("{DIFF}[... 9000 bytes of output omitted (lines 9-400) ...]\n+tail\n");
    assert_eq!(parse_diff(&cut), None);
    assert_eq!(parse_diff(" 3 files changed, 20 insertions(+)\n"), None);
}

#[test]
fn status_and_diff_read_as_a_row_with_their_files() {
    let rows = render(&bash("git status", STATUS, ToolState::Completed), 90, false).unwrap();
    assert!(
        rows[0].contains("Status") && rows[0].contains("main ↑2"),
        "{rows:?}"
    );
    assert!(
        rows[0].contains("1 staged")
            && rows[0].contains("2 changed")
            && rows[0].contains("1 untracked"),
        "{rows:?}"
    );
    assert_eq!(rows.len(), 5, "{rows:?}");
    assert!(rows[1].contains("A src/new.rs") && rows[4].contains("? notes.md"));

    let clean = render(
        &bash(
            "git status",
            "On branch main\nnothing to commit, working tree clean\n",
            ToolState::Completed,
        ),
        80,
        false,
    )
    .unwrap();
    assert_eq!(clean.len(), 1);
    assert!(clean[0].contains("clean"), "{clean:?}");

    let rows = render(&bash("git diff", DIFF, ToolState::Completed), 90, false).unwrap();
    assert!(
        rows[0].contains("Diff") && rows[0].contains("2 files") && rows[0].contains("+2 −2"),
        "{rows:?}"
    );
    assert!(
        rows[1].contains("src/parser.rs") && rows[1].ends_with("+2 −2"),
        "{rows:?}"
    );
    assert!(
        rows[2].contains("logo.png") && rows[2].ends_with("binary"),
        "{rows:?}"
    );

    let many: String = (0..8)
        .map(|i| format!("diff --git a/f{i} b/f{i}\n@@ -1 +1 @@\n+x\n"))
        .collect();
    let rows = render(
        &bash("git diff --cached", &many, ToolState::Completed),
        60,
        false,
    )
    .unwrap();
    assert!(rows.last().unwrap().contains("+3 more"), "{rows:?}");
    for w in [1, 10, 30, 60] {
        render(&bash("git status", STATUS, ToolState::Completed), w, false);
        render(&bash("git diff", DIFF, ToolState::Completed), w, false);
    }
    let empty = render(
        &bash("git diff", "[exit code: 0]", ToolState::Completed),
        80,
        false,
    )
    .unwrap();
    assert!(empty[0].contains("no changes"), "{empty:?}");
}

#[test]
fn piped_cut_or_other_status_and_diff_output_keeps_the_ordinary_card() {
    for (command, output) in [
        ("git diff | head -40", DIFF),
        ("git status > s.txt", STATUS),
        ("git diff --stat", " a.rs | 2 +-\n 1 file changed\n"),
        ("git status -v", STATUS),
        ("git status --porcelain=v2", "1 .M N... a.rs\n"),
        ("git status -s && git diff", DIFF),
        ("git diff", "warning: something odd\n"),
    ] {
        assert!(
            render(&bash(command, output, ToolState::Completed), 80, false).is_none(),
            "{command}"
        );
    }
    let cut = format!("{DIFF}[... 9000 bytes of output omitted (lines 9-400) ...]\n");
    assert!(render(&bash("git diff", &cut, ToolState::Completed), 80, false).is_none());
    assert!(render(&bash("git status", STATUS, ToolState::Failed), 80, false).is_none());
    assert!(render(&bash("git status", STATUS, ToolState::Completed), 80, true).is_none());
}
