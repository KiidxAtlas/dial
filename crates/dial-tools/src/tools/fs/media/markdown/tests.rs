use super::document;

fn body(markdown: &str) -> String {
    let html = document(markdown);
    let start = html.find("<body>").unwrap() + "<body>".len();
    html[start..html.len() - "</body>".len()].to_owned()
}

#[test]
fn renders_common_block_and_inline_structure() {
    assert_eq!(
        body("# Title\n\nSome *em* and **strong** with `x<y`.\n"),
        "<h1>Title</h1><p>Some <em>em</em> and <strong>strong</strong> with <code>x&lt;y</code>.</p>"
    );
    assert_eq!(
        body("1. one\n2. two\n\n- [x] done\n"),
        "<ol><li>one</li><li>two</li></ol><ul><li><input type=\"checkbox\" checked disabled> done</li></ul>"
    );
    assert_eq!(
        body("```rust\nfn a() -> &'static str {}\n```\n"),
        "<pre><code>fn a() -&gt; &amp;&#39;static str {}\n</code></pre>"
    );
    assert_eq!(body("3. three\n"), "<ol start=\"3\"><li>three</li></ol>");
}

#[test]
fn tables_links_and_images_keep_their_structure() {
    assert_eq!(
        body("| a | b |\n|---|---|\n| 1 | 2 |\n"),
        "<table><thead><tr><th>a</th><th>b</th></tr></thead><tbody><tr><td>1</td><td>2</td></tr></tbody></table>"
    );
    assert_eq!(
        body("[go \"here\"](https://e.x/?a=1&b=2) ![a *b*](p.png)"),
        "<p><a href=\"https://e.x/?a=1&amp;b=2\">go &quot;here&quot;</a> <img src=\"p.png\" alt=\"a b\"></p>"
    );
}

#[test]
fn document_is_standalone_and_passes_raw_html_through() {
    let html = document("<div align=\"center\">x</div>\n");
    assert!(html.starts_with("<!doctype html><meta charset=\"utf-8\"><style>"));
    assert!(html.contains("<div align=\"center\">x</div>"));
    assert!(html.ends_with("</body>"));
}
