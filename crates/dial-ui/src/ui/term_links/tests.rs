use super::*;

fn linker_in(root: &Path, fragments: bool) -> Linker {
    let mut linker = Linker::new(fragments);
    linker.set_root(Some(root));
    linker
}

fn targets(linker: &mut Linker, text: &str, full: bool) -> Vec<(String, String)> {
    linker
        .links(text, full)
        .into_iter()
        .map(|(range, url)| (text[range].to_owned(), url))
        .collect()
}

#[test]
fn links_urls_and_existing_paths_with_line_fragments() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/a b.rs"), "").unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "").unwrap();
    let mut linker = linker_in(root.path(), true);
    let lib = url::Url::from_file_path(root.path().join("src/lib.rs")).unwrap();
    let dir = url::Url::from_file_path(root.path().join("src")).unwrap();
    assert_eq!(
        targets(
            &mut linker,
            "(src/lib.rs:4:2), `src/lib.rs`. src/ nope.rs https://x.dev/p?q=1.",
            false
        ),
        vec![
            ("src/lib.rs:4:2".to_owned(), format!("{lib}#4:2")),
            ("src/lib.rs".to_owned(), format!("{lib}#1")),
            ("src/".to_owned(), dir.to_string()),
            (
                "https://x.dev/p?q=1".to_owned(),
                "https://x.dev/p?q=1".to_owned()
            ),
        ]
    );
}

#[test]
fn without_fragments_file_links_carry_no_line() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.rs"), "").unwrap();
    let mut linker = linker_in(root.path(), false);
    let main = url::Url::from_file_path(root.path().join("main.rs")).unwrap();
    assert_eq!(
        targets(&mut linker, "main.rs:9", false),
        vec![("main.rs:9".to_owned(), main.to_string())]
    );
}

#[test]
fn a_token_at_the_edge_of_a_full_row_is_left_plain() {
    let mut linker = Linker::new(false);
    assert!(targets(&mut linker, "see https://example.com/long", true).is_empty());
    assert_eq!(
        targets(&mut linker, "https://example.com/a more", true).len(),
        1
    );
}

#[test]
fn words_numbers_and_other_schemes_are_not_links() {
    let root = tempfile::tempdir().unwrap();
    let mut linker = linker_in(root.path(), true);
    assert!(
        targets(
            &mut linker,
            "hello 1.5 ... // javascript:alert(1) file:///etc",
            false
        )
        .is_empty()
    );
    assert!(targets(&mut linker, "http://", false).is_empty());
}

#[test]
fn a_new_root_forgets_cached_lookups() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::write(second.path().join("x.rs"), "").unwrap();
    let mut linker = linker_in(first.path(), false);
    assert!(targets(&mut linker, "x.rs", false).is_empty());
    linker.set_root(Some(second.path()));
    assert_eq!(targets(&mut linker, "x.rs", false).len(), 1);
}

#[test]
fn file_urls_percent_encode_and_take_a_host() {
    let url = file_url(Path::new("/tmp/a b#c.rs"), "mac.local", Some("3")).unwrap();
    assert_eq!(url, "file://mac.local/tmp/a%20b%23c.rs#3");
    assert!(file_url(Path::new("relative"), "", None).is_none());
}
