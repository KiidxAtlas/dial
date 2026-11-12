//! E-mail messages (`.eml`) and mailboxes (`.mbox`) decoded through MIME: headers, the text body
//! (HTML converted to text when there is no plain part) and an attachment list. Attachments are
//! never written out or opened.

use mail_parser::{AddressList, MessageParser};
use std::fmt::Write as _;

#[cfg(test)]
mod tests;

/// Most messages listed from one mailbox.
const MAX_MESSAGES: usize = 10_000;

fn addresses(list: Option<AddressList<'_>>) -> Option<String> {
    let list = list?;
    let shown: Vec<String> = list
        .mailboxes()
        .map(|mailbox| match (mailbox.name(), mailbox.address()) {
            (Some(name), Some(address)) => format!("{name} <{address}>"),
            (None, Some(address)) => address.to_owned(),
            (Some(name), None) => name.to_owned(),
            (None, None) => String::new(),
        })
        .filter(|s| !s.is_empty())
        .collect();
    (!shown.is_empty()).then(|| shown.join(", "))
}

pub(super) fn message(bytes: &[u8]) -> Result<String, String> {
    let message = MessageParser::default()
        .parse(bytes)
        .ok_or("not a parseable e-mail message")?;
    let mut out = String::new();
    for (label, value) in [
        ("From", addresses(message.from())),
        ("To", addresses(message.to())),
        ("Cc", addresses(message.cc())),
        ("Date", message.date().map(|d| d.to_rfc3339())),
        ("Subject", message.subject().map(str::to_owned)),
        ("Message-ID", message.message_id().map(str::to_owned)),
    ] {
        if let Some(value) = value {
            let _ = writeln!(out, "{label}: {value}");
        }
    }
    let attachments: Vec<String> = message
        .attachments()
        .map(|part| {
            let kind = part.content_type().map_or_else(
                || "application/octet-stream".to_owned(),
                |ct| match ct.subtype() {
                    Some(sub) => format!("{}/{sub}", ct.ctype()),
                    None => ct.ctype().to_owned(),
                },
            );
            format!(
                "  {} ({kind}, {} bytes)",
                part.attachment_name().unwrap_or("unnamed"),
                part.decoded_len()
            )
        })
        .collect();
    if !attachments.is_empty() {
        let _ = writeln!(out, "Attachments ({}; not extracted):", attachments.len());
        for line in attachments {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out.push('\n');
    match message.body_text(0) {
        Some(body) => out.push_str(&body),
        None => out.push_str("[no text body]\n"),
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// A listing of the mailbox, or one whole message with `:message=N` (1-based).
pub(super) fn mbox(bytes: &[u8], member: Option<&str>) -> Result<String, String> {
    let selected = member
        .map(|m| {
            m.strip_prefix("message=")
                .and_then(|n| n.parse::<usize>().ok())
                .filter(|n| *n >= 1)
                .ok_or("mbox selector must be :message=N (1-based)")
        })
        .transpose()?;
    let mut out = String::new();
    let mut count = 0;
    for entry in mail_parser::mailbox::mbox::MessageIterator::new(bytes) {
        let entry = entry.map_err(|e| format!("reading mbox: {e}"))?;
        count += 1;
        if let Some(n) = selected {
            if n == count {
                return message(entry.contents()).map(|text| format!("[message {n}]\n{text}"));
            }
            continue;
        }
        if count > MAX_MESSAGES {
            out.push_str("…[more messages omitted]\n");
            break;
        }
        let parsed = MessageParser::default().parse(entry.contents());
        let (date, from, subject) = parsed.as_ref().map_or_else(
            || (String::new(), String::new(), "[unparseable]".to_owned()),
            |m| {
                (
                    m.date().map(|d| d.to_rfc3339()).unwrap_or_default(),
                    addresses(m.from()).unwrap_or_default(),
                    m.subject().unwrap_or("").to_owned(),
                )
            },
        );
        let _ = writeln!(out, "{count}\t{date}\t{from}\t{subject}");
    }
    if let Some(n) = selected {
        return Err(format!("message {n} not found; the mailbox has {count}"));
    }
    Ok(format!(
        "mbox: {count} messages (read one with :message=N)\n#\tdate\tfrom\tsubject\n{out}"
    ))
}
