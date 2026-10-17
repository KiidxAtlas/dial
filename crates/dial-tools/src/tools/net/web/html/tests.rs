use super::*;

fn text(html: &str) -> String {
    to_text(html, "https://example.com/dir/page.html")
}

#[test]
fn converts_structure_and_drops_chrome() {
    let html = r#"<!doctype html><html><head><title>Hello &amp; welcome</title>
            <style>body { color: red }</style><script>var x = "<p>not text</p>";</script></head>
            <body><nav><a href="/home">Home</a><ul><li>menu</li></ul></nav>
            <h1>Main   heading</h1>
            <p>First <b>para</b> with a <a href="/docs/intro">link</a> and <a href="https://x.org/">https://x.org/</a>.</p>
            <ul><li>one</li><li>two<ol><li>nested</li></ol></li></ul>
            <pre><code>fn main() {
    println!("&lt;hi&gt;");
}</code></pre>
            <p>Use <code>cargo test</code>.</p>
            <footer>copyright junk</footer></body></html>"#;
    let out = text(html);
    let expected = "Title: Hello & welcome\n\n# Main heading\n\n\
            First para with a link (https://example.com/docs/intro) and https://x.org/.\n\n\
            - one\n- two\n  1. nested\n\n\
            ```\nfn main() {\n    println!(\"<hi>\");\n}\n```\n\n\
            Use `cargo test`.";
    assert_eq!(out, expected);
}

#[test]
fn tables_images_and_skipped_links() {
    let out = text(
        r##"<table><tr><th>Name</th><th>Size</th></tr><tr><td>a</td><td>1</td></tr></table>
            <img src="x.png" alt="a chart"> <a href="#top">top</a> <a href="javascript:void(0)">js</a>
            <a href="mailto:a@b.c">mail</a>"##,
    );
    assert_eq!(out, "Name | Size\na | 1\n\n[image: a chart] top js mail");
}

#[test]
fn survives_malformed_markup() {
    let out = text("<p>unclosed <b>bold <a href='/x'>link<p>next < 3 & more <div>a</p></span>");
    assert_eq!(
        out,
        "unclosed bold link (https://example.com/x)\n\nnext < 3 & more\na"
    );
    assert_eq!(text("<script>never closed <p>x"), "");
    assert_eq!(text("plain text only"), "plain text only");
}

#[test]
fn hidden_elements_are_dropped() {
    assert_eq!(
        text(r#"<div hidden><p>secret</p></div><span aria-hidden="true">x</span><p>shown</p>"#),
        "shown"
    );
}

#[test]
fn list_items_emptied_by_dropped_content_vanish() {
    assert_eq!(
        text(
            "<ul><li><button>menu</button></li><li>real</li></ul><ol><li><svg></svg></li><li>two</li></ol>"
        ),
        "- real\n\n2. two"
    );
}

#[test]
fn decodes_entities() {
    assert_eq!(
        decode_entities("a &lt;b&gt; &#65;&#x42; &amp;amp; &bogus; & &copy"),
        "a <b> AB &amp; &bogus; & &copy"
    );
}

#[test]
fn tokenizer_handles_attributes() {
    let tokens = tokenize(r#"<A HREF="/a?x=1&amp;y=2" data-x=raw hidden class='c d'/>"#);
    let Token::Open {
        name,
        attrs,
        self_closing,
    } = &tokens[0]
    else {
        panic!("{tokens:?}")
    };
    assert_eq!(name, "a");
    assert!(self_closing);
    assert_eq!(attr(attrs, "href"), Some("/a?x=1&y=2"));
    assert_eq!(attr(attrs, "data-x"), Some("raw"));
    assert_eq!(attr(attrs, "hidden"), Some(""));
    assert_eq!(attr(attrs, "class"), Some("c d"));
}
