//! Durable, redacted record of one child job's conversation.
//!
//! Each job writes `<state_dir>/subagents/<parent session>/<job>-<millis>.jsonl` outside the
//! workspace. The first line is a header; every archived message follows as one record, and a
//! `status` record closes the file. Lines are redacted and bounded, and readers only touch the
//! tail of a file so the viewer never loads a whole transcript.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::config::paths::Layout;
use crate::storage::redact::redact_value;

/// Upper bound for one record; larger messages are replaced by a marker line.
pub const MAX_RECORD: usize = 256 * 1024;
/// Bytes read from the end of a transcript when the viewer loads it.
pub const TAIL_BYTES: u64 = 2 * 1024 * 1024;

/// Append-only writer for one child job.
pub struct ChildTranscript {
    path: PathBuf,
    file: Mutex<File>,
}

impl ChildTranscript {
    /// Creates the transcript file and writes its header as the first line.
    pub fn create(layout: &Layout, parent_session: &str, job: &str, header: Value) -> Result<Self> {
        let dir = transcript_dir(layout, parent_session);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let path = dir.join(format!("{}-{millis}.jsonl", safe_component(job)));
        let mut options = OpenOptions::new();
        options.create_new(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        let transcript = Self {
            path,
            file: Mutex::new(file),
        };
        let mut header = header;
        redact_value(&mut header);
        header["type"] = json!("header");
        transcript.write_line(&header)?;
        Ok(transcript)
    }

    /// Appends one archived conversation message, redacted and bounded.
    pub fn record(&self, message: &Value) -> Result<()> {
        let mut value = message.clone();
        redact_value(&mut value);
        let line = serde_json::to_string(&value)?;
        if line.len() > MAX_RECORD {
            let marker = json!({
                "role": message["role"],
                "content": format!("[message truncated: {} bytes]", line.len()),
            });
            return self.write_line(&json!({"type": "message", "message": marker}));
        }
        self.write_line(&json!({"type": "message", "message": value}))
    }

    pub fn execution_contract(&self, tools: &[String], model: &str) -> Result<()> {
        self.write_line(&json!({"type":"execution_contract", "tools":tools, "model":model}))
    }

    /// Closes the transcript with its final status.
    pub fn finish(&self, status: Value) -> Result<()> {
        let mut status = status;
        redact_value(&mut status);
        let mut record = json!({"type": "status"});
        if let (Some(target), Some(source)) = (record.as_object_mut(), status.as_object()) {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
        self.write_line(&record)
    }

    fn write_line(&self, value: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        file.write_all(&line)
            .with_context(|| format!("writing {}", self.path.display()))?;
        file.flush()?;
        // Persist call intent before effects, and completed results before continuation.
        file.sync_data()?;
        Ok(())
    }
}

/// Directory holding transcripts for one parent session.
pub fn transcript_dir(layout: &Layout, parent_session: &str) -> PathBuf {
    layout
        .state_dir
        .join("subagents")
        .join(safe_component(parent_session))
}

/// Header and most recent status of one transcript, read without loading its messages.
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptSummary {
    pub path: PathBuf,
    pub header: Value,
    pub status: Option<Value>,
}

/// Lists transcripts for a parent session, newest first.
pub fn scan(layout: &Layout, parent_session: &str) -> Vec<TranscriptSummary> {
    let Ok(entries) = fs::read_dir(transcript_dir(layout, parent_session)) else {
        return Vec::new();
    };
    let mut found: Vec<(std::time::SystemTime, TranscriptSummary)> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|entry| {
            let path = entry.path();
            let modified = entry.metadata().ok()?.modified().ok()?;
            let header = read_first(&path)?;
            let status = read_status(&path);
            Some((
                modified,
                TranscriptSummary {
                    path,
                    header,
                    status,
                },
            ))
        })
        .collect();
    found.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    found.into_iter().map(|(_, summary)| summary).collect()
}

/// Message records from the tail of a transcript, oldest first. Header and status are omitted.
pub fn messages(path: &Path) -> Result<Vec<Value>> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    if start > 0 {
        // The first line may be cut mid-record; drop it.
        lines.next();
    }
    Ok(lines
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| record["type"] == "message")
        .map(|record| record["message"].clone())
        .collect())
}

/// Every message record from the start of a transcript, oldest first. This is the index space of
/// `#<entry>` deep links: entry `i` is the `i`th message record in the file.
pub fn entries(path: &Path) -> Result<Vec<Value>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if record["type"] == "message" {
            out.push(record["message"].clone());
        }
    }
    Ok(out)
}

fn read_first(path: &Path) -> Option<Value> {
    let file = File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).ok()?;
    let value: Value = serde_json::from_str(&line).ok()?;
    (value["type"] == "header").then_some(value)
}

fn read_status(path: &Path) -> Option<Value> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(4096);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record["type"] == "status")
}

fn safe_component(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "session".to_owned()
    } else {
        cleaned
    }
}

/// One readable line per archived message (`role: text`). Tool and other structured blocks are
/// summarized by kind so the hub shows what happened without dumping raw payloads.
pub fn format_lines(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| {
            let presentation = message.get("dial_tool_presentation");
            let view = presentation.and_then(|value| {
                serde_json::from_value::<crate::runtime::presentation::Presentation>(
                    value["view"].clone(),
                )
                .ok()
            });
            if let Some(view) = view {
                let phase = presentation
                    .and_then(|value| {
                        serde_json::from_value::<crate::runtime::Phase>(value["phase"].clone()).ok()
                    })
                    .unwrap_or(crate::runtime::Phase::Failed);
                let glyph = match phase {
                    crate::runtime::Phase::Completed => "✓",
                    crate::runtime::Phase::Failed | crate::runtime::Phase::Interrupted => "✗",
                    _ => "!",
                };
                let mut facts = view.facts;
                if phase != crate::runtime::Phase::Completed {
                    facts.push(phase.label().into());
                }
                let mut rows = vec![format!(
                    "{glyph} {:<7} {}{}",
                    view.label,
                    view.target,
                    if facts.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", facts.join(" · "))
                    }
                )];
                let evidence = message["dial_child_tool_result"]["text"]
                    .as_str()
                    .or_else(|| message["content"].as_str())
                    .unwrap_or_default();
                rows.extend(evidence.lines().map(|line| format!("   {line}")));
                rows
            } else {
                vec![format_message(message)]
            }
        })
        .collect()
}

fn format_message(message: &Value) -> String {
    let role = message["role"].as_str().unwrap_or("message");
    let body = match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(block_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    format!("{role}: {}", body.replace(['\n', '\r'], " "))
}

fn block_text(block: &Value) -> String {
    match block["type"].as_str() {
        Some("text" | "output_text") | None => {
            block["text"].as_str().unwrap_or_default().to_owned()
        }
        Some(kind) => match block["name"].as_str() {
            Some(name) if !name.is_empty() => format!("[{kind} {name}]"),
            _ => format!("[{kind}]"),
        },
    }
}

#[cfg(test)]
mod tests;
