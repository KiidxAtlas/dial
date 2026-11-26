use super::*;
use std::process::Command;

fn log(commits: &[&[&str]]) -> String {
    commits
        .iter()
        .map(|files| format!("\u{1e}\n\n{}\n", files.join("\n")))
        .collect()
}

#[test]
fn partners_need_enough_shared_commits_and_skip_sweeping_ones() {
    let wide: Vec<String> = (0..25).map(|n| format!("f{n}.rs")).collect();
    let mut wide: Vec<&str> = wide.iter().map(String::as_str).collect();
    wide.push("src/a.rs");
    let text = log(&[
        &["src/a.rs", "src/a/tests.rs", "CHANGELOG.md"],
        &["src/a.rs", "src/a/tests.rs"],
        &["src/a.rs", "src/a/tests.rs", "CHANGELOG.md"],
        &["src/a.rs", "CHANGELOG.md", "README.md"],
        &["src/a.rs", "src/b.rs"],
        &wide,
    ]);
    let history = History::parse(&text);
    let partners = history.partners("src/a.rs", &|_| false);
    // The 26-file commit is ignored: a.rs has 5 commits; README (1) and b.rs (1) are too rare.
    assert_eq!(
        partners,
        vec![
            ("CHANGELOG.md".to_owned(), 3, 5),
            ("src/a/tests.rs".to_owned(), 3, 5),
        ]
    );
    let partners = history.partners("src/a.rs", &|path| path == "CHANGELOG.md");
    assert_eq!(partners, vec![("src/a/tests.rs".to_owned(), 3, 5)]);
    assert!(history.partners("unknown.rs", &|_| false).is_empty());
}

#[test]
fn a_rare_pair_in_a_busy_file_is_not_named() {
    let mut commits: Vec<&[&str]> = vec![&["hot.rs", "rare.rs"]; 3];
    commits.extend(std::iter::repeat_n(&["hot.rs"][..], 9));
    let history = History::parse(&log(&commits));
    // 3 shared commits out of 12 is below the 40% share.
    assert!(history.partners("hot.rs", &|_| false).is_empty());
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[test]
fn edits_name_their_usual_companions_once() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git not installed; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    git(&root, &["init", "-q"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    for n in 0..3 {
        std::fs::write(root.join("src/lib.rs"), format!("{n}")).unwrap();
        std::fs::write(root.join("src/lib_tests.rs"), format!("{n}")).unwrap();
        std::fs::write(root.join("gone.rs"), format!("{n}")).unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "change"]);
    }
    std::fs::remove_file(root.join("gone.rs")).unwrap();

    let note = note(&root, &["src\\lib.rs".to_owned()]);
    assert_eq!(
        note,
        "[history hint] src/lib.rs usually changes with src/lib_tests.rs (3 of 3 commits); check whether they need updating too.\n"
    );
    // Shown once per file; editing both together names neither as the other's companion.
    assert_eq!(super::note(&root, &["src/lib.rs".to_owned()]), "");
    assert_eq!(
        super::note(
            &root,
            &["src/lib_tests.rs".to_owned(), "src/lib.rs".to_owned()]
        ),
        ""
    );

    // Outside a repository there is nothing to say.
    let plain = tempfile::tempdir().unwrap();
    assert_eq!(super::note(plain.path(), &["a.rs".to_owned()]), "");
}
