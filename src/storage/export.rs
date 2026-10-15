//! Session transcript rendering for `dial sessions export`, `/export` and RPC `export_html`.
//!
//! Every text that leaves the session (title, messages, tool calls and results) passes through
//! `storage::redact` first. HTML embeds verified image attachments as data URIs; Markdown keeps
//! tool payloads in fenced blocks (no raw HTML) and notes images without inlining them.
use super::redact::redact_text;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn content_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A fence longer than any backtick run inside `text`.
fn fence(text: &str) -> String {
    let (mut longest, mut run) = (0usize, 0usize);
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat(longest.max(2) + 1)
}

enum Image {
    Inline { mime: &'static str, data: String },
    Missing(String),
}

fn images(message: &Value) -> Vec<Image> {
    message["dial_images"]
        .as_array()
        .into_iter()
        .flatten()
        .map(
            |reference| match crate::llm::images::verified_bytes(reference) {
                Ok((bytes, mime)) => Image::Inline {
                    mime,
                    data: STANDARD.encode(bytes),
                },
                Err(error) => Image::Missing(format!("{error:#}")),
            },
        )
        .collect()
}

/// Render the active branch. `messages` are chat messages in order (no control records).
pub fn render(title: &str, messages: &[&Value], html: bool) -> String {
    let title = redact_text(title);
    let mut body = String::new();
    if html {
        body.push_str(&format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title><style>body{{font:16px system-ui;max-width:900px;margin:2rem auto;padding:0 1rem;line-height:1.5}}pre{{white-space:pre-wrap;overflow-wrap:anywhere}}details{{border:1px solid #888;padding:.6rem;margin:1rem 0}}h2{{border-bottom:1px solid #888}}img{{max-width:100%;height:auto;border:1px solid #888;margin:.5rem 0}}</style></head><body><h1>{}</h1>",
            escape(&title),
            escape(&title)
        ));
    } else {
        body.push_str(&format!("# {title}\n\n"));
    }
    for message in messages {
        let role = message["role"].as_str().unwrap_or("");
        let content = content_text(message);
        let content = redact_text(&content);
        let calls = message
            .get("tool_calls")
            .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()));
        let attached = images(message);
        if role == "tool" || calls.is_some() {
            if !content.is_empty() && role != "tool" {
                if html {
                    body.push_str(&format!(
                        "<h2>{}</h2><pre>{}</pre>\n\n",
                        escape(role),
                        escape(&content)
                    ));
                } else {
                    body.push_str(&format!("## {role}\n\n{content}\n\n"));
                }
            }
            let payload = match calls {
                Some(calls) => {
                    let mut calls = calls.clone();
                    super::redact::redact_value(&mut calls);
                    serde_json::to_string_pretty(&calls).unwrap_or_default()
                }
                None => content.into_owned(),
            };
            let label = if role == "tool" {
                "Tool result"
            } else {
                "Tool calls"
            };
            if html {
                body.push_str(&format!(
                    "<details><summary>{label}</summary><pre>{}</pre></details>\n\n",
                    escape(&payload)
                ));
            } else {
                let fence = fence(&payload);
                body.push_str(&format!("**{label}**\n\n{fence}\n{payload}\n{fence}\n\n"));
            }
        } else if html {
            body.push_str(&format!(
                "<h2>{}</h2><pre>{}</pre>\n",
                escape(role),
                escape(&content)
            ));
        } else {
            body.push_str(&format!("## {role}\n\n{content}\n\n"));
        }
        for image in attached {
            match image {
                Image::Inline { mime, data } if html => {
                    body.push_str(&format!(
                        "<img alt=\"attached image\" src=\"data:{mime};base64,{data}\">\n"
                    ));
                }
                Image::Inline { mime, data } => {
                    body.push_str(&format!(
                        "*[image attachment: {mime}, {} bytes; export with --html to embed it]*\n\n",
                        data.len() / 4 * 3
                    ));
                }
                Image::Missing(error) if html => {
                    body.push_str(&format!(
                        "<p><em>[Stored image unavailable: {}]</em></p>\n",
                        escape(&error)
                    ));
                }
                Image::Missing(error) => {
                    body.push_str(&format!("*[Stored image unavailable: {error}]*\n\n"));
                }
            }
        }
    }
    if html {
        body.push_str("</body></html>\n");
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn export_redacts_messages_tool_calls_and_results() {
        let key = format!("sk-ant-{}", "Q".repeat(40));
        let user = json!({"role":"user","content":format!("use {key}")});
        let call = json!({"role":"assistant","content":"","tool_calls":[{"id":"c1","type":"function","function":{"name":"bash","arguments":format!("{{\"command\":\"curl -H 'Authorization: Bearer {key}'\"}}")}}]});
        let result = json!({"role":"tool","tool_call_id":"c1","content":format!("echo {key}")});
        for html in [false, true] {
            let text = render(&format!("title {key}"), &[&user, &call, &result], html);
            assert!(!text.contains(&key), "{text}");
            assert!(text.contains("[REDACTED:"), "{text}");
        }
    }

    #[test]
    fn markdown_uses_fences_not_html() {
        let result = json!({"role":"tool","tool_call_id":"c1","content":"```\n<b>x</b>"});
        let text = render("<t>", &[&result], false);
        assert!(text.starts_with("# <t>"));
        assert!(text.contains("````\n```\n<b>x</b>\n````"));
        assert!(!text.contains("<details>"));
    }

    #[test]
    fn html_embeds_verified_images_and_reports_missing_ones() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("pixel.png");
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        std::fs::write(&path, &png).unwrap();
        let reference = crate::llm::images::user_attachment(&root, &path).unwrap();
        let message = json!({"role":"user","content":"look","dial_images":[reference, {"root":"/nonexistent","path":"x.png","sha256":"00"}]});
        let text = render("t", &[&message], true);
        assert!(
            text.contains("<img alt=\"attached image\" src=\"data:image/png;base64,"),
            "{text}"
        );
        assert!(text.contains("Stored image unavailable"), "{text}");
    }
}
