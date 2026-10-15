//! A small, forgiving HTML tokenizer and a text/markdown-ish converter built on it.
//!
//! The converter targets reading, not fidelity: headings become `#` lines, links become
//! `text (url)`, `<pre>` becomes a fenced block, lists become `-`/`1.` lines and tables become
//! ` | ` separated rows. Scripts, styles, navigation, footers and other page chrome are dropped.

use std::borrow::Cow;

#[derive(Debug, PartialEq)]
pub enum Token<'a> {
    Text(Cow<'a, str>),
    Open {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    Close(String),
}

/// Elements whose content is raw text up to the matching close tag.
const RAW_TEXT: &[&str] = &["script", "style", "textarea", "title", "xmp"];

const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

pub fn is_void(name: &str) -> bool {
    VOID.contains(&name)
}

/// Split `html` into tokens. Comments, doctypes and processing instructions are discarded; text is
/// returned with entities still encoded (use [`decode_entities`]).
pub fn tokenize<'a>(html: &'a str) -> Vec<Token<'a>> {
    let bytes = html.as_bytes();
    let mut tokens = Vec::new();
    let (mut pos, mut text_start) = (0, 0);
    let flush = |tokens: &mut Vec<Token<'a>>, from: usize, to: usize| {
        if to > from {
            tokens.push(Token::Text(Cow::Borrowed(&html[from..to])));
        }
    };
    while pos < bytes.len() {
        if bytes[pos] != b'<' {
            pos += 1;
            continue;
        }
        let rest = &html[pos..];
        if rest.starts_with("<!--") {
            flush(&mut tokens, text_start, pos);
            pos = rest.find("-->").map_or(html.len(), |end| pos + end + 3);
        } else if rest.starts_with("<!") || rest.starts_with("<?") {
            flush(&mut tokens, text_start, pos);
            pos = rest.find('>').map_or(html.len(), |end| pos + end + 1);
        } else if rest.starts_with("</")
            && rest.as_bytes().get(2).is_some_and(u8::is_ascii_alphabetic)
        {
            flush(&mut tokens, text_start, pos);
            let end = rest.find('>').map_or(html.len(), |end| pos + end + 1);
            let name = html[pos + 2..end.min(html.len())].trim_end_matches('>');
            let name = name
                .split(|c: char| c.is_ascii_whitespace() || c == '/')
                .next()
                .unwrap_or("");
            tokens.push(Token::Close(name.to_ascii_lowercase()));
            pos = end;
        } else if rest.as_bytes().get(1).is_some_and(u8::is_ascii_alphabetic) {
            flush(&mut tokens, text_start, pos);
            let (name, attrs, self_closing, end) = parse_open_tag(html, pos);
            pos = end;
            if RAW_TEXT.contains(&name.as_str()) && !self_closing {
                let close = format!("</{name}");
                let body_end = find_ci(html, &close, pos).unwrap_or(html.len());
                tokens.push(Token::Open {
                    name: name.clone(),
                    attrs,
                    self_closing,
                });
                if body_end > pos {
                    tokens.push(Token::Text(Cow::Borrowed(&html[pos..body_end])));
                }
                pos = body_end;
            } else {
                tokens.push(Token::Open {
                    name,
                    attrs,
                    self_closing,
                });
            }
        } else {
            pos += 1;
            continue;
        }
        text_start = pos;
    }
    flush(&mut tokens, text_start, html.len());
    tokens
}

/// Parse the open tag starting at `start` (`<` byte). Returns (name, attrs, self_closing, end).
fn parse_open_tag(html: &str, start: usize) -> (String, Vec<(String, String)>, bool, usize) {
    let bytes = html.as_bytes();
    let mut pos = start + 1;
    let name_start = pos;
    while pos < bytes.len()
        && !bytes[pos].is_ascii_whitespace()
        && bytes[pos] != b'>'
        && bytes[pos] != b'/'
    {
        pos += 1;
    }
    let name = html[name_start..pos].to_ascii_lowercase();
    let mut attrs = Vec::new();
    let mut self_closing = false;
    while pos < bytes.len() {
        match bytes[pos] {
            b'>' => {
                pos += 1;
                break;
            }
            b'/' => {
                self_closing = bytes.get(pos + 1) == Some(&b'>');
                pos += 1;
            }
            b if b.is_ascii_whitespace() => pos += 1,
            _ => {
                let key_start = pos;
                while pos < bytes.len()
                    && !bytes[pos].is_ascii_whitespace()
                    && !matches!(bytes[pos], b'=' | b'>' | b'/')
                {
                    pos += 1;
                }
                let key = html[key_start..pos].to_ascii_lowercase();
                while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
                    pos += 1;
                }
                let mut value = String::new();
                if bytes.get(pos) == Some(&b'=') {
                    pos += 1;
                    while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
                        pos += 1;
                    }
                    let (raw, next) = match bytes.get(pos) {
                        Some(&quote @ (b'"' | b'\'')) => {
                            let from = pos + 1;
                            let to = bytes[from..]
                                .iter()
                                .position(|&b| b == quote)
                                .map_or(bytes.len(), |i| from + i);
                            (&html[from..to], (to + 1).min(bytes.len()))
                        }
                        _ => {
                            let from = pos;
                            let mut to = from;
                            while to < bytes.len()
                                && !bytes[to].is_ascii_whitespace()
                                && bytes[to] != b'>'
                            {
                                to += 1;
                            }
                            (&html[from..to], to)
                        }
                    };
                    value = decode_entities(raw).into_owned();
                    pos = next;
                }
                if !key.is_empty() {
                    attrs.push((key, value));
                }
            }
        }
    }
    (name, attrs, self_closing, pos.min(bytes.len()))
}

/// Byte offset of the first ASCII-case-insensitive occurrence of `needle` at or after `from`.
fn find_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let (hay, needle) = (haystack.as_bytes(), needle.as_bytes());
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len())
        .find(|&i| hay[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

pub fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// Decode named (common subset) and numeric character references; unknown ones are kept verbatim.
pub fn decode_entities(text: &str) -> Cow<'_, str> {
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest[1..]
            .find(';')
            .filter(|&len| len <= 10)
            .and_then(|len| entity(&rest[1..1 + len]).map(|c| (c, len + 2)));
        match decoded {
            Some((c, consumed)) => {
                out.push_str(&c);
                rest = &rest[consumed..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

fn entity(body: &str) -> Option<String> {
    if let Some(number) = body.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse().ok()?,
        };
        return char::from_u32(code)
            .filter(|&c| c != '\0')
            .map(String::from);
    }
    let c = match body {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "ndash" => '–',
        "mdash" => '—',
        "hellip" => '…',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "bull" => '•',
        "middot" => '·',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "deg" => '°',
        "times" => '×',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "sect" => '§',
        "para" => '¶',
        "larr" => '←',
        "rarr" => '→',
        _ => return None,
    };
    Some(c.to_string())
}

/// Elements dropped together with everything inside them.
const DROPPED: &[&str] = &[
    "script", "style", "noscript", "svg", "canvas", "template", "iframe", "object", "embed", "nav",
    "footer", "aside", "button", "select", "dialog",
];

/// Elements that start and end a paragraph (blank line around them).
const PARAGRAPH: &[&str] = &[
    "p",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "pre",
    "blockquote",
    "table",
    "ul",
    "ol",
    "dl",
    "figure",
    "details",
    "hr",
];

/// Elements that start and end a line.
const LINE: &[&str] = &[
    "div",
    "section",
    "article",
    "main",
    "header",
    "li",
    "tr",
    "dt",
    "dd",
    "form",
    "fieldset",
    "figcaption",
    "address",
    "center",
    "summary",
    "body",
    "html",
    "caption",
    "thead",
    "tbody",
    "tfoot",
    "br",
];

/// Readable text of an HTML document. `base` (the page's own URL) resolves relative links.
pub fn to_text(html: &str, base: &str) -> String {
    let mut writer = Writer::new(base);
    for token in tokenize(html) {
        writer.feed(token);
    }
    writer.finish()
}

struct Skip {
    name: String,
    depth: usize,
}

struct Link {
    href: Option<String>,
    text_start: usize,
}

struct Writer<'a> {
    base: &'a str,
    out: String,
    title: Option<String>,
    in_title: bool,
    skip: Option<Skip>,
    /// One entry per open list: the next number for `<ol>`, `None` for `<ul>`.
    lists: Vec<Option<usize>>,
    pre_depth: usize,
    link: Option<Link>,
    cells_in_row: usize,
    pending_space: bool,
}

impl<'a> Writer<'a> {
    fn new(base: &'a str) -> Self {
        Self {
            base,
            out: String::new(),
            title: None,
            in_title: false,
            skip: None,
            lists: Vec::new(),
            pre_depth: 0,
            link: None,
            cells_in_row: 0,
            pending_space: false,
        }
    }

    fn feed(&mut self, token: Token<'_>) {
        if let Some(skip) = &mut self.skip {
            match &token {
                Token::Open {
                    name,
                    self_closing: false,
                    ..
                } if *name == skip.name && !is_void(name) => skip.depth += 1,
                Token::Close(name) if *name == skip.name => {
                    skip.depth -= 1;
                    if skip.depth == 0 {
                        self.skip = None;
                    }
                }
                _ => {}
            }
            return;
        }
        match token {
            Token::Text(text) if self.in_title => {
                let title = self.title.get_or_insert_with(String::new);
                title.push_str(&decode_entities(&text));
            }
            Token::Text(text) => self.text(&decode_entities(&text)),
            Token::Open {
                name,
                attrs,
                self_closing,
            } => self.open(name, &attrs, self_closing),
            Token::Close(name) => self.close(&name),
        }
    }

    fn open(&mut self, name: String, attrs: &[(String, String)], self_closing: bool) {
        let hidden = attr(attrs, "hidden").is_some() || attr(attrs, "aria-hidden") == Some("true");
        if name == "title" {
            self.in_title = true;
            return;
        }
        if (DROPPED.contains(&name.as_str()) || hidden) && !is_void(&name) && !self_closing {
            self.skip = Some(Skip { name, depth: 1 });
            return;
        }
        if PARAGRAPH.contains(&name.as_str()) {
            self.end_link_with_text();
        }
        match name.as_str() {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.paragraph_break();
                let level = usize::from(name.as_bytes()[1] - b'0');
                self.out.push_str(&"#".repeat(level));
                self.out.push(' ');
            }
            "pre" => {
                self.paragraph_break();
                self.out.push_str("```\n");
                self.pre_depth += 1;
            }
            "ul" | "ol" => {
                if self.lists.is_empty() {
                    self.paragraph_break();
                } else {
                    self.line_break();
                }
                self.lists.push((name == "ol").then_some(1));
            }
            "li" => {
                self.line_break();
                self.out
                    .push_str(&"  ".repeat(self.lists.len().saturating_sub(1)));
                match self.lists.last_mut() {
                    Some(Some(n)) => {
                        self.out.push_str(&format!("{n}. "));
                        *n += 1;
                    }
                    _ => self.out.push_str("- "),
                }
                self.pending_space = false;
            }
            "td" | "th" => {
                if self.cells_in_row > 0 {
                    self.out.push_str(" | ");
                }
                self.cells_in_row += 1;
                self.pending_space = false;
            }
            "tr" => {
                self.line_break();
                self.cells_in_row = 0;
            }
            "hr" => {
                self.paragraph_break();
                self.out.push_str("---");
                self.paragraph_break();
            }
            "br" => self.newline(),
            "code" if self.pre_depth == 0 => {
                self.space_if_pending();
                self.out.push('`');
            }
            "a" => {
                self.end_link();
                let href = attr(attrs, "href").and_then(|href| self.link_target(href));
                self.link = Some(Link {
                    href,
                    text_start: self.out.len(),
                });
            }
            "img" => {
                if let Some(alt) = attr(attrs, "alt")
                    .map(str::trim)
                    .filter(|alt| !alt.is_empty())
                {
                    self.text(&format!("[image: {alt}]"));
                }
            }
            _ if PARAGRAPH.contains(&name.as_str()) => self.paragraph_break(),
            _ if LINE.contains(&name.as_str()) => self.line_break(),
            _ => {}
        }
    }

    fn close(&mut self, name: &str) {
        match name {
            "title" => self.in_title = false,
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.paragraph_break(),
            "pre" => {
                if self.pre_depth > 0 {
                    self.pre_depth -= 1;
                    self.line_break();
                    self.out.push_str("```");
                    self.paragraph_break();
                }
            }
            "ul" | "ol" => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.paragraph_break();
                }
            }
            "code" if self.pre_depth == 0 => self.out.push('`'),
            "a" => self.end_link(),
            _ if PARAGRAPH.contains(&name) => self.paragraph_break(),
            _ if LINE.contains(&name) => self.line_break(),
            _ => {}
        }
    }

    fn end_link(&mut self) {
        let Some(link) = self.link.take() else { return };
        let Some(href) = link.href else { return };
        let text = self.out[link.text_start..].trim();
        if text.is_empty() || text == href {
            return;
        }
        self.out.push_str(" (");
        self.out.push_str(&href);
        self.out.push(')');
    }

    /// A block starts inside a link that never closed: end the link now if it already has text.
    fn end_link_with_text(&mut self) {
        if self
            .link
            .as_ref()
            .is_some_and(|link| !self.out[link.text_start..].trim().is_empty())
        {
            self.end_link();
        }
    }

    /// An absolute http(s) URL for a link, or `None` for fragments, `javascript:` and other schemes.
    fn link_target(&self, href: &str) -> Option<String> {
        let href = href.trim();
        if href.is_empty() || href.starts_with('#') {
            return None;
        }
        let url = super::fetch::resolve_url(self.base, href)?;
        (url.starts_with("http://") || url.starts_with("https://")).then_some(url)
    }

    fn text(&mut self, text: &str) {
        if self.pre_depth > 0 {
            self.out.push_str(text);
            return;
        }
        for c in text.chars() {
            if c.is_whitespace() {
                self.pending_space = true;
            } else {
                self.space_if_pending();
                self.out.push(c);
            }
        }
    }

    fn space_if_pending(&mut self) {
        if std::mem::take(&mut self.pending_space)
            && self
                .out
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_whitespace())
        {
            self.out.push(' ');
        }
    }

    fn newline(&mut self) {
        self.pending_space = false;
        self.out.push('\n');
    }

    /// End the current line, if it has content.
    fn line_break(&mut self) {
        self.pending_space = false;
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }

    /// Leave a blank line after the current content.
    fn paragraph_break(&mut self) {
        self.line_break();
        if !self.out.is_empty() && !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }

    fn finish(mut self) -> String {
        self.end_link();
        let mut body = String::with_capacity(self.out.len());
        let mut blank_run = 0;
        for line in self.out.lines() {
            let line = line.trim_end();
            // List items whose content was dropped (buttons, icons) leave a bare marker behind.
            let marker = line.trim_start();
            if marker == "-"
                || marker
                    .strip_suffix('.')
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            {
                continue;
            }
            blank_run = if line.is_empty() { blank_run + 1 } else { 0 };
            if blank_run <= 1 {
                body.push_str(line);
                body.push('\n');
            }
        }
        self.out = body.trim().to_owned();
        match self
            .title
            .as_deref()
            .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        {
            Some(title) if !title.is_empty() => format!("Title: {title}\n\n{}", self.out)
                .trim_end()
                .to_owned(),
            _ => self.out,
        }
    }
}

#[cfg(test)]
mod tests {
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
}
