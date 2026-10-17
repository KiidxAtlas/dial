use super::super::model::{Area, Change, Kind};
use super::*;
use crate::git_panel::change_paths;

fn change(path: &str, area: Area) -> Change {
    Change {
        path: path.to_owned(),
        orig: None,
        kind: Kind::Modified,
        area,
    }
}

fn dir(key: &str, name: &str, depth: usize, collapsed: bool) -> Entry {
    Entry::Dir {
        key: key.to_owned(),
        name: name.to_owned(),
        depth,
        collapsed,
    }
}

fn sample() -> Vec<Change> {
    vec![
        change("src/a.rs", Area::Unstaged),
        change("src/ui/overlay/x.rs", Area::Unstaged),
        change("src/ui/y.rs", Area::Unstaged),
        change("README.md", Area::Unstaged),
    ]
}

#[test]
fn nested_directories_flatten_depth_first() {
    let rows = flatten(&sample(), Area::Unstaged, &HashSet::new());
    assert_eq!(
        rows,
        vec![
            dir("unstaged:src", "src", 0, false),
            dir("unstaged:src/ui", "ui", 1, false),
            dir("unstaged:src/ui/overlay", "overlay", 2, false),
            Entry::File { index: 1, depth: 3 },
            Entry::File { index: 2, depth: 2 },
            Entry::File { index: 0, depth: 1 },
            Entry::File { index: 3, depth: 0 },
        ]
    );
}

#[test]
fn collapsed_directory_hides_its_children() {
    let mut collapsed = HashSet::new();
    collapsed.insert("unstaged:src/ui".to_owned());
    let rows = flatten(&sample(), Area::Unstaged, &collapsed);
    assert_eq!(
        rows,
        vec![
            dir("unstaged:src", "src", 0, false),
            dir("unstaged:src/ui", "ui", 1, true),
            Entry::File { index: 0, depth: 1 },
            Entry::File { index: 3, depth: 0 },
        ]
    );
}

#[test]
fn single_directory_chains_compress() {
    let changes = vec![
        change("a/b/c.txt", Area::Unstaged),
        change("a/b/d/e.txt", Area::Unstaged),
    ];
    let rows = flatten(&changes, Area::Unstaged, &HashSet::new());
    assert_eq!(
        rows,
        vec![
            dir("unstaged:a/b", "a/b", 0, false),
            dir("unstaged:a/b/d", "d", 1, false),
            Entry::File { index: 1, depth: 2 },
            Entry::File { index: 0, depth: 1 },
        ]
    );
}

#[test]
fn sections_are_separate() {
    let mut changes = sample();
    changes[0].area = Area::Staged;
    let staged = flatten(&changes, Area::Staged, &HashSet::new());
    assert_eq!(
        staged,
        vec![
            dir("staged:src", "src", 0, false),
            Entry::File { index: 0, depth: 1 },
        ]
    );
}

#[test]
fn parent_of_finds_enclosing_directory() {
    let rows = flatten(&sample(), Area::Unstaged, &HashSet::new());
    assert_eq!(parent_of(&rows, 3), Some(2));
    assert_eq!(parent_of(&rows, 2), Some(1));
    assert_eq!(parent_of(&rows, 0), None);
    assert_eq!(parent_of(&rows, 6), None);
}

#[test]
fn rename_entry_yields_both_paths() {
    let renamed = Change {
        path: "new/name.rs".to_owned(),
        orig: Some("old.rs".to_owned()),
        kind: Kind::Renamed,
        area: Area::Staged,
    };
    let rows = flatten(
        std::slice::from_ref(&renamed),
        Area::Staged,
        &HashSet::new(),
    );
    assert_eq!(
        rows,
        vec![
            dir("staged:new", "new", 0, false),
            Entry::File { index: 0, depth: 1 }
        ]
    );
    assert_eq!(change_paths(&renamed), vec!["new/name.rs", "old.rs"]);
}

#[test]
fn unstaged_half_of_a_staged_rename_stages_only_its_path() {
    let half = Change {
        path: "new.rs".to_owned(),
        orig: Some("old.rs".to_owned()),
        kind: Kind::Modified,
        area: Area::Unstaged,
    };
    assert_eq!(change_paths(&half), vec!["new.rs"]);
}
