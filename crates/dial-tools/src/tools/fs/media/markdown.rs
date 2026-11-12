//! Minimal Markdown → HTML writer for rendered previews (`notes.md:img`).
//!
//! The workspace builds `pulldown-cmark` without its `html` feature, so this writes the common
//! CommonMark/GFM block and inline structure itself. Raw HTML passes through like GitHub; the
//! rendering page's content-security policy, not this writer, is what confines it.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

#[cfg(test)]
mod tests;

const STYLE: &str = "body{font:16px/1.5 -apple-system,'Segoe UI',Helvetica,Arial,sans-serif;max-width:900px;margin:24px auto;padding:0 24px;color:#1f2328}code,pre{font-family:ui-monospace,Menlo,Consolas,monospace;background:#f6f8fa;border-radius:6px}code{padding:.15em .35em}pre{padding:12px;overflow:auto}pre code{padding:0}blockquote{margin:0;padding:0 1em;color:#59636e;border-left:.25em solid #d1d9e0}table{border-collapse:collapse}th,td{border:1px solid #d1d9e0;padding:6px 13px}h1,h2{border-bottom:1px solid #d1d9e0;padding-bottom:.3em}img{max-width:100%}";

/// A complete HTML document for `markdown`.
pub(super) fn document(markdown: &str) -> String {
    let mut out = format!("<!doctype html><meta charset=\"utf-8\"><style>{STYLE}</style><body>");
    let parser = Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_FOOTNOTES,
    );
    let mut in_head = false;
    // Inside an image, text becomes the alt attribute rather than visible content.
    let mut image_depth = 0usize;
    for event in parser {
        if image_depth > 0 {
            match event {
                Event::Start(Tag::Image { .. }) => image_depth += 1,
                Event::End(TagEnd::Image) => {
                    image_depth -= 1;
                    if image_depth == 0 {
                        out.push_str("\">");
                    }
                }
                Event::Text(text) | Event::Code(text) => escape(&mut out, &text),
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => out.push_str("<p>"),
                Tag::Heading { level, .. } => out.push_str(&format!("<h{}>", level as usize)),
                Tag::BlockQuote(_) => out.push_str("<blockquote>"),
                Tag::CodeBlock(_) => out.push_str("<pre><code>"),
                Tag::List(Some(1)) => out.push_str("<ol>"),
                Tag::List(Some(start)) => out.push_str(&format!("<ol start=\"{start}\">")),
                Tag::List(None) => out.push_str("<ul>"),
                Tag::Item => out.push_str("<li>"),
                Tag::FootnoteDefinition(label) => {
                    out.push_str("<div class=\"footnote\"><sup>");
                    escape(&mut out, &label);
                    out.push_str("</sup> ");
                }
                Tag::Table(_) => out.push_str("<table>"),
                Tag::TableHead => {
                    in_head = true;
                    out.push_str("<thead><tr>");
                }
                Tag::TableRow => out.push_str("<tr>"),
                Tag::TableCell => out.push_str(if in_head { "<th>" } else { "<td>" }),
                Tag::Emphasis => out.push_str("<em>"),
                Tag::Strong => out.push_str("<strong>"),
                Tag::Strikethrough => out.push_str("<del>"),
                Tag::Link { dest_url, .. } => {
                    out.push_str("<a href=\"");
                    escape(&mut out, &dest_url);
                    out.push_str("\">");
                }
                Tag::Image { dest_url, .. } => {
                    image_depth = 1;
                    out.push_str("<img src=\"");
                    escape(&mut out, &dest_url);
                    out.push_str("\" alt=\"");
                }
                Tag::DefinitionList => out.push_str("<dl>"),
                Tag::DefinitionListTitle => out.push_str("<dt>"),
                Tag::DefinitionListDefinition => out.push_str("<dd>"),
                Tag::HtmlBlock | Tag::MetadataBlock(_) => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => out.push_str("</p>"),
                TagEnd::Heading(level) => out.push_str(&format!("</h{}>", level as usize)),
                TagEnd::BlockQuote(_) => out.push_str("</blockquote>"),
                TagEnd::CodeBlock => out.push_str("</code></pre>"),
                TagEnd::List(true) => out.push_str("</ol>"),
                TagEnd::List(false) => out.push_str("</ul>"),
                TagEnd::Item => out.push_str("</li>"),
                TagEnd::FootnoteDefinition => out.push_str("</div>"),
                TagEnd::Table => out.push_str("</tbody></table>"),
                TagEnd::TableHead => {
                    in_head = false;
                    out.push_str("</tr></thead><tbody>");
                }
                TagEnd::TableRow => out.push_str("</tr>"),
                TagEnd::TableCell => out.push_str(if in_head { "</th>" } else { "</td>" }),
                TagEnd::Emphasis => out.push_str("</em>"),
                TagEnd::Strong => out.push_str("</strong>"),
                TagEnd::Strikethrough => out.push_str("</del>"),
                TagEnd::Link => out.push_str("</a>"),
                TagEnd::DefinitionList => out.push_str("</dl>"),
                TagEnd::DefinitionListTitle => out.push_str("</dt>"),
                TagEnd::DefinitionListDefinition => out.push_str("</dd>"),
                TagEnd::Image | TagEnd::HtmlBlock | TagEnd::MetadataBlock(_) => {}
            },
            Event::Text(text) => escape(&mut out, &text),
            Event::Code(text) => {
                out.push_str("<code>");
                escape(&mut out, &text);
                out.push_str("</code>");
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => escape(&mut out, &text),
            Event::Html(html) | Event::InlineHtml(html) => out.push_str(&html),
            Event::FootnoteReference(label) => {
                out.push_str("<sup>");
                escape(&mut out, &label);
                out.push_str("</sup>");
            }
            Event::SoftBreak => out.push('\n'),
            Event::HardBreak => out.push_str("<br>"),
            Event::Rule => out.push_str("<hr>"),
            Event::TaskListMarker(done) => out.push_str(if done {
                "<input type=\"checkbox\" checked disabled> "
            } else {
                "<input type=\"checkbox\" disabled> "
            }),
        }
    }
    out.push_str("</body>");
    out
}

fn escape(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}
