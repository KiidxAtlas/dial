use super::*;

fn version(path: &str, before: &str, after: &str) -> TurnVersion {
    TurnVersion {
        path: path.into(),
        in_project: true,
        before: Some(before.as_bytes().to_vec()),
        after: Some(after.as_bytes().to_vec()),
    }
}

#[test]
fn only_changed_or_removed_headers_of_distinctive_names_count() {
    let before = "pub fn parse_config(path: &str) -> u32 { 1 }\nfn keep_same(a: u8) {}\nfn main() {}\nfn removed_helper() {}\n";
    let after = "pub fn parse_config(path: &str, strict: bool) -> u32 { 2 }\nfn keep_same(a: u8) { let _ = a; }\nfn main(x: u8) {}\nfn brand_new() {}\n";
    let changed = changed_signatures(&[version("src/lib.rs", before, after)]);
    assert_eq!(
        changed,
        [
            ("src/lib.rs".to_owned(), "parse_config".to_owned()),
            ("src/lib.rs".to_owned(), "removed_helper".to_owned()),
        ],
        "body-only edits, common names and new declarations are not obligations"
    );
    // Unsupported languages and new files produce nothing.
    assert!(changed_signatures(&[version("notes.txt", "a", "b")]).is_empty());
    let created = TurnVersion {
        before: None,
        ..version("src/new.rs", "", "fn created_thing(a: u8) {}")
    };
    assert!(changed_signatures(&[created]).is_empty());
}

#[test]
fn unvisited_mentions_are_listed_and_cleared_by_reads() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let after = "pub fn parse_config(path: &str, strict: bool) {}\n";
    std::fs::write(root.join("src/lib.rs"), after).unwrap();
    std::fs::write(root.join("src/caller.rs"), "parse_config(\"x\");\n").unwrap();
    std::fs::write(root.join("src/other.rs"), "fn unrelated() {}\n").unwrap();
    let workspace = Workspace::new(root).unwrap();
    let versions = [version(
        "src/lib.rs",
        "pub fn parse_config(path: &str) {}\n",
        after,
    )];
    let open = evaluate(&workspace, &versions, &BTreeSet::new());
    assert_eq!(open["status"], "open");
    assert_eq!(open["unvisited_mentions"][0]["path"], "src/caller.rs");
    assert_eq!(open["unvisited_mentions"].as_array().unwrap().len(), 1);
    let visited: BTreeSet<String> =
        read_targets(&workspace, &json!({"paths":["src/caller.rs:1-5"]}))
            .into_iter()
            .collect();
    assert_eq!(visited, BTreeSet::from(["src/caller.rs".to_owned()]));
    assert_eq!(evaluate(&workspace, &versions, &visited)["status"], "clear");
    // Nothing changed: no record at all.
    let same = [version("src/lib.rs", after, after)];
    assert!(evaluate(&workspace, &same, &BTreeSet::new()).is_null());
}
