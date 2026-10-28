//! Session transcript rendering for `dial sessions export`, `/export` and RPC `export_html`.
//!
//! Every text that leaves the session (title, messages, tool calls and results) passes through
//! `storage::redact` first. HTML embeds verified image attachments as data URIs; Markdown keeps
//! tool payloads in fenced blocks (no raw HTML) and notes images without inlining them.
//!
//! Subagent (worker) transcripts follow the parent conversation as separate sections, so an export
//! shows what the workers did, not just the parent's summary of it.
use super::redact::redact_text;
use super::subagent::Worker;
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

/// Append chat messages in order (no control records) to `body`.
fn push_messages(body: &mut String, messages: &[&Value], html: bool) {
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
}

/// Render the active branch. `messages` are chat messages in order (no control records).
pub fn render(title: &str, messages: &[&Value], html: bool) -> String {
    render_with_workers(title, messages, &[], html)
}

/// Render the active branch followed by one section per subagent transcript, in the order given.
pub fn render_with_workers(
    title: &str,
    messages: &[&Value],
    workers: &[Worker],
    html: bool,
) -> String {
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
    push_messages(&mut body, messages, html);
    for worker in workers {
        let worker_title = redact_text(&worker.title);
        if html {
            body.push_str(&format!(
                "<hr><h1>Subagent: {}</h1>\n",
                escape(&worker_title)
            ));
        } else {
            body.push_str(&format!("---\n\n# Subagent: {worker_title}\n\n"));
        }
        let refs: Vec<&Value> = worker.messages.iter().collect();
        push_messages(&mut body, &refs, html);
    }
    if html {
        body.push_str("</body></html>\n");
    }
    body
}

#[cfg(test)]
mod tests;
