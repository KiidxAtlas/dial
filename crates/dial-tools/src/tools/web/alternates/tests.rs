use super::*;

#[test]
fn llms_probes_walk_the_scope_then_the_root() {
    assert_eq!(
        llms_candidates("https://example.com/docs/guide/intro"),
        vec![
            "https://example.com/docs/guide/llms.txt",
            "https://example.com/docs/guide/llms.md",
            "https://example.com/docs/llms.txt",
            "https://example.com/docs/llms.md",
            "https://example.com/.well-known/llms.txt",
            "https://example.com/llms.txt",
            "https://example.com/llms.md",
        ]
    );
}

#[test]
fn llms_probes_at_root_are_only_root_files() {
    assert_eq!(
        llms_candidates("https://example.com/"),
        vec![
            "https://example.com/.well-known/llms.txt",
            "https://example.com/llms.txt",
            "https://example.com/llms.md",
        ]
    );
}

#[test]
fn llms_probes_for_a_directory_include_the_directory() {
    let candidates = llms_candidates("https://example.com/docs/");
    assert_eq!(candidates[0], "https://example.com/docs/llms.txt");
    assert_eq!(candidates[1], "https://example.com/docs/llms.md");
}

#[test]
fn md_sibling_of_page_and_directory() {
    assert_eq!(
        md_candidate("https://example.com/guide?x=1").as_deref(),
        Some("https://example.com/guide.md")
    );
    assert_eq!(
        md_candidate("https://example.com/guide/").as_deref(),
        Some("https://example.com/guide/index.html.md")
    );
}

#[test]
fn markdown_alternate_links_are_resolved() {
    let page = r#"<html><head>
            <link rel="alternate" type="text/markdown" href="/page.md">
            <link rel="stylesheet" type="text/css" href="/site.css">
            <link REL="Alternate" type="Text/Markdown" href="notes.md">
            </head></html>"#;
    assert_eq!(
        markdown_alternates(page, "https://example.com/a/b"),
        vec![
            "https://example.com/page.md",
            "https://example.com/a/notes.md"
        ]
    );
}

#[test]
fn accepts_rejects_html_and_stubs() {
    let long = "word ".repeat(40);
    assert!(accepts(Some("text/markdown"), &long));
    assert!(accepts(None, &long));
    assert!(!accepts(Some("text/html"), &long));
    assert!(!accepts(None, &format!("<!DOCTYPE html><p>{long}")));
    assert!(!accepts(None, "short"));
}
