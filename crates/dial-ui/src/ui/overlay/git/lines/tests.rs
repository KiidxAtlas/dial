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
fn zz_dump() {
    use crate::ui::overlay::settings::testkit::{Rig, render, screen, status_preview};
    use crate::ui::overlay::DrawCtx;
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "t"]);
    std::fs::create_dir_all(root.join("src/ui/overlay")).unwrap();
    std::fs::write(root.join("f.txt"), "one\ntwo\nthree\nfour\nfive\n").expect("write");
    std::fs::write(root.join("src/ui/overlay/providers_with_long_name.rs"), "fn a() {}\n").expect("write");
    git(root, &["add", "."]);
    git(root, &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "init"]);
    std::fs::write(root.join("f.txt"), "one\nTWO\nthree\nFOUR\nfive\n").expect("write");
    std::fs::write(root.join("src/ui/overlay/providers_with_long_name.rs"), "fn a() { println!(\"hello world this is a long line\"); }\n").expect("write");
    std::fs::write(root.join("new.md"), "new\n").expect("write");
    git(root, &["add", "f.txt"]);
    let ascii = Rig::with_theme(crate::ui::theme::Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii));
    let rig = Rig::new();
    let mut overlay = GitOverlay::open(crate::git_panel::Repo::new(root, &crate::config::types::Config::default()), None).expect("opens");
    for (w, h, r) in [(120u16, 30u16, &rig), (100, 24, &rig), (60, 20, &rig), (38, 16, &ascii)] {
        let ctx = DrawCtx { theme: &r.theme, keys: &r.keys, status: &r.status, preview_status: &status_preview };
        println!("==== {w}x{h}\n{}", screen(&render(w, h, |f| overlay.draw(f, &ctx))));
    }
    std::fs::write(root.join("f.txt"), "one\ntwo\nthree\nfour\nfive\n").expect("write");
    git(root, &["add", "."]);
    git(root, &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "clean"]);
    std::fs::write(root.join("src/ui/overlay/providers_with_long_name.rs"), "fn a() {}\n").expect("write");
    git(root, &["add", "."]);
    git(root, &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "clean2"]);
    git(root, &["rm", "-q", "--cached", "new.md"]);
    std::fs::remove_file(root.join("new.md")).unwrap();
    let mut clean = GitOverlay::open(crate::git_panel::Repo::new(root, &crate::config::types::Config::default()), None).expect("opens");
    for (w, h, r) in [(100u16, 24u16, &rig), (38, 16, &ascii)] {
        let ctx = DrawCtx { theme: &r.theme, keys: &r.keys, status: &r.status, preview_status: &status_preview };
        println!("==== clean {w}x{h}\n{}", screen(&render(w, h, |f| clean.draw(f, &ctx))));
    }
}
