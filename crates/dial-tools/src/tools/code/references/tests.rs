use super::*;

#[test]
fn whole_word_mentions_respect_exclusions_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn parse_config() {}\n").unwrap();
    std::fs::write(root.join("src/a.rs"), "let c = parse_config();\n").unwrap();
    std::fs::write(root.join("src/b.rs"), "let c = parse_config_v2();\n").unwrap();
    std::fs::write(root.join("src/c.rs"), "use crate::parse_config;\n").unwrap();
    let ws = Workspace::new(root).unwrap();
    let names = vec!["parse_config".to_owned()];
    let exclude = BTreeSet::from(["src/lib.rs".to_owned()]);
    let mentions = files_mentioning(&ws, &names, &exclude, 20);
    let paths: Vec<_> = mentions.files.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(paths, ["src/a.rs", "src/c.rs"]);
    assert!(!mentions.incomplete);
    let capped = files_mentioning(&ws, &names, &exclude, 1);
    assert_eq!(capped.files.len(), 1);
    assert!(capped.incomplete);
}
