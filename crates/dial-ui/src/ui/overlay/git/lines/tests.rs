use super::super::{GitOverlay, model::*};
use super::{LineSel, changed_keys, span_bodies};
use crate::git_panel::parse_diff;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::Path;

fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// One hunk with two changed pairs: body lines are 0 ` one`, 1 `-two`, 2 `+TWO`, 3 ` three`,
/// 4 `-four`, 5 `+FOUR`, 6 ` five`.
fn sample() -> Doc {
    parse_diff(
        "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,5 +1,5 @@\n one\n-two\n+TWO\n three\n-four\n+FOUR\n five\n",
    )
}

fn sample_change() -> Change {
    Change {
        path: "f".to_owned(),
        orig: None,
        kind: Kind::Modified,
        area: Area::Unstaged,
    }
}

#[test]
fn crlf_hunk_round_trips_cr_and_displays_without_it() {
    let text = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n one\r\n-two\r\n+TWO\r\n";
    let doc = parse_diff(text);
    let lines: Vec<&str> = doc.hunks[0].lines.iter().map(String::as_str).collect();
    assert_eq!(lines, vec![" one\r", "-two\r", "+TWO\r"]);
    assert_eq!(crate::git_panel::hunk_patch(&doc, 0).as_deref(), Some(text));
    for view in [View::Inline, View::Split] {
        for row in layout(&doc, view, Some(0)) {
            let shown: Vec<&str> = match &row.kind {
                RowKind::Line { text, .. } => vec![text.as_str()],
                RowKind::Pair { left, right } => left
                    .iter()
                    .chain(right.iter())
                    .map(|col| col.text.as_str())
                    .collect(),
                _ => Vec::new(),
            };
            assert!(shown.iter().all(|text| !text.contains('\r')));
        }
    }
}

#[test]
fn toggle_marks_then_unmarks_body_lines() {
    let doc = sample();
    let mut sel = LineSel::new(&sample_change(), 0, &doc.hunks[0], View::Inline);
    sel.toggle(&[2]);
    sel.toggle(&[1, 2]);
    assert_eq!(sel.marked.iter().copied().collect::<Vec<_>>(), vec![1, 2]);
    sel.toggle(&[1, 2]);
    assert!(sel.marked.is_empty());
    sel.toggle(&[]);
    assert!(sel.marked.is_empty());
}

#[test]
fn range_extends_over_changed_lines_in_row_order() {
    let doc = sample();
    let rows = layout(&doc, View::Inline, Some(0));
    let keys = changed_keys(&doc.hunks[0]);
    let row_of = |text: &str| {
        rows.iter()
            .position(|row| matches!(&row.kind, RowKind::Line { text: t, .. } if t == text))
            .expect("row exists")
    };
    let (from, to) = (row_of("two"), row_of("FOUR"));
    assert_eq!(span_bodies(&rows, 0, from, to, &keys), vec![1, 2, 4, 5]);
    assert_eq!(span_bodies(&rows, 0, to, to, &keys), vec![5]);
    assert_eq!(
        span_bodies(&rows, 0, row_of("one"), row_of("one"), &keys),
        Vec::<usize>::new()
    );
}

#[test]
fn selection_stages_only_the_marked_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "t"]);
    std::fs::write(root.join("f.txt"), "one\ntwo\nthree\nfour\nfive\n").expect("write");
    git(root, &["add", "f.txt"]);
    git(
        root,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "init"],
    );
    std::fs::write(root.join("f.txt"), "one\nTWO\nthree\nFOUR\nfive\n").expect("write");

    let mut overlay = GitOverlay::open(
        crate::git_panel::Repo::new(root, &crate::config::types::Config::default()),
        None,
    )
    .expect("overlay opens");
    overlay.focus = Focus::Diff;
    overlay.cursor_row = overlay
        .rows
        .iter()
        .position(|row| {
            matches!(&row.kind, RowKind::Line { side: Side::Added, text, .. } if text == "TWO")
        })
        .expect("TWO row");

    let mark = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE);
    assert!(overlay.line_key(&mark, Some('m')));
    let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
    assert!(overlay.line_key(&space, Some(' ')));

    assert_eq!(
        git(root, &["show", ":f.txt"]),
        "one\ntwo\nTWO\nthree\nfour\nfive\n"
    );
}

#[test]
fn narrow_layouts_keep_the_file_name_files_and_way_out_visible() {
    use crate::ui::overlay::DrawCtx;
    use crate::ui::overlay::settings::testkit::{Rig, render, rows, status_preview};
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "t"]);
    std::fs::create_dir_all(root.join("src/deep/dir")).expect("mkdir");
    std::fs::write(root.join("src/deep/dir/changed_file.rs"), "one\n").expect("write");
    git(root, &["add", "."]);
    git(
        root,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "init"],
    );
    std::fs::write(root.join("src/deep/dir/changed_file.rs"), "two\n").expect("write");
    let mut overlay = GitOverlay::open(
        crate::git_panel::Repo::new(root, &crate::config::types::Config::default()),
        None,
    )
    .expect("overlay opens");
    let ascii = Rig::with_theme(
        crate::ui::theme::Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii),
    );
    let rig = Rig::new();
    for (w, h, r) in [(120u16, 30u16, &rig), (60, 20, &rig), (38, 16, &ascii)] {
        let ctx = DrawCtx {
            theme: &r.theme,
            keys: &r.keys,
            status: &r.status,
            preview_status: &status_preview,
        };
        let screen = rows(&render(w, h, |f| overlay.draw(f, &ctx)));
        let all = screen.join("\n");
        // The diff header names the file even when the pane is narrow.
        assert!(screen[1].contains("changed_file.rs"), "{w}x{h}\n{all}");
        // Routine `diff --git`/`index` lines repeat the header and are not drawn.
        assert!(
            !all.contains("diff --git") && !all.contains("index "),
            "{all}"
        );
        // No empty staging group, and the way out is always offered.
        assert!(!all.to_lowercase().contains("staged (0)"), "{all}");
        assert!(screen[usize::from(h) - 2].contains("Esc close"), "{all}");
        if w < 100 {
            // The stacked file list reaches the file instead of repeating the branch.
            assert!(
                all.contains("changed_file.rs") && !all.contains("HEAD "),
                "{all}"
            );
        }
    }
}

#[test]
fn a_clean_tree_says_so_and_does_not_offer_staging() {
    use crate::ui::overlay::DrawCtx;
    use crate::ui::overlay::settings::testkit::{Rig, render, screen, status_preview};
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "t"]);
    std::fs::write(root.join("f.txt"), "one\n").expect("write");
    git(root, &["add", "."]);
    git(
        root,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "init"],
    );
    let mut overlay = GitOverlay::open(
        crate::git_panel::Repo::new(root, &crate::config::types::Config::default()),
        None,
    )
    .expect("overlay opens");
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let shown = screen(&render(120, 30, |f| overlay.draw(f, &ctx)));
    assert!(shown.contains("working tree is clean"), "{shown}");
    assert!(
        !shown.contains("stage") && !shown.contains("c commit"),
        "{shown}"
    );
}
