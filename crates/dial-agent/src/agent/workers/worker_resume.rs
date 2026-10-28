//! Explicit continuation of a worker. History is never replayed as tool execution.
use crate::config::paths::Layout;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader},
    path::Path,
};

pub fn identity(root: &Path) -> Result<String> {
    Ok(format!(
        "{}:{:?}",
        root.canonicalize()?.display(),
        crate::platform::fs::path_identity(root)?
    ))
}

pub fn lease(layout: &Layout, session: &str, id: &str) -> Result<File> {
    ensure!(
        !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'),
        "invalid worker id"
    );
    let transcripts = crate::agent::workers::child_transcript::transcript_dir(layout, session);
    let dir = layout
        .state_dir
        .join("subagent-leases")
        .join(transcripts.file_name().context("invalid parent session")?);
    crate::platform::fs::create_dir_owner_only(std::fs::DirBuilder::new().recursive(true), &dir)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(dir.join(format!("{id}.lock")))?;
    fs2::FileExt::try_lock_exclusive(&file)
        .context("worker is still owned by another running process")?;
    Ok(file)
}

pub struct Resume {
    pub task: Value,
    pub messages: Vec<Value>,
    pub tools: Vec<String>,
    pub model: String,
    pub definition: Value,
    /// The last settings the worker applied at a request boundary; `model` stays the launch model.
    pub settings: Option<super::live_tasks::control::WorkerSettings>,
    pub lease: File,
}

pub fn load(
    layout: &Layout,
    session: &str,
    root: &Path,
    id: &str,
    message: &str,
) -> Result<Resume> {
    ensure!(
        !message.trim().is_empty() && message.len() <= 32768,
        "resume needs a message of 1..32768 bytes"
    );
    let lease = lease(layout, session, id)?;
    let entry = crate::agent::workers::child_transcript::scan(layout, session)
        .into_iter()
        .find(|entry| entry.header["id"] == id)
        .context("no saved worker in this parent session")?;
    let contract = &entry.header["resume_contract"];
    ensure!(
        contract["version"] == 1,
        "worker predates resumable execution contracts"
    );
    ensure!(
        contract["workspace"] == identity(root)?,
        "worker workspace identity changed; inspect its retained transcript before starting new work"
    );
    ensure!(
        contract["task"]["isolation"].is_null(),
        "isolated workers remain transcript-only after release; start a new isolated worker"
    );
    let file = File::open(&entry.path)?;
    ensure!(
        file.metadata()?.len() <= 8 * 1024 * 1024,
        "worker history exceeds safe resume limit"
    );
    let mut messages = Vec::new();
    let mut execution = None;
    let mut settings: Option<super::live_tasks::control::WorkerSettings> = None;
    for line in BufReader::new(file).lines() {
        let value: Value = serde_json::from_str(&line?)?;
        if value["type"] == "execution_contract" {
            execution = Some(value.clone());
        }
        if value["type"] == "worker_settings" {
            settings = Some(serde_json::from_value(value.clone())?);
        }
        if value["type"] == "message" {
            messages.push(value["message"].clone());
        }
    }
    validate_history(&messages)?;
    let execution = execution.context("worker has no recorded execution contract")?;
    ensure!(
        execution["model"] == contract["model"],
        "worker used a fallback model; start a new worker with that model explicitly"
    );
    let mut task = contract["task"].clone();
    task["original_task"] = contract
        .get("original_task")
        .cloned()
        .unwrap_or_else(|| task["task"].take());
    task["task"] = json!(message);
    task["action"] = json!("start");
    Ok(Resume {
        task,
        messages,
        lease,
        tools: serde_json::from_value(execution["tools"].clone())?,
        model: contract["model"]
            .as_str()
            .context("missing worker model")?
            .into(),
        definition: contract["definition"].clone(),
        settings,
    })
}

fn validate_history(messages: &[Value]) -> Result<()> {
    ensure!(!messages.is_empty(), "worker has no saved conversation");
    let mut pending = HashSet::new();
    for message in messages {
        ensure!(
            message["dial_effects"] != "unknown",
            "worker has uncertain effects; inspect affected state before starting new work"
        );
        ensure!(
            !message["content"]
                .as_str()
                .is_some_and(|text| text.starts_with("[message truncated:")),
            "worker history was truncated"
        );
        if message["role"] == "assistant" {
            ensure!(
                pending.is_empty(),
                "worker history contains unresolved tool effects"
            );
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                let id = call["id"].as_str().context("tool call has no identity")?;
                ensure!(pending.insert(id.to_owned()), "duplicate tool identity");
            }
        } else if message["role"] == "tool" {
            ensure!(
                message["tool_call_id"]
                    .as_str()
                    .is_some_and(|id| pending.remove(id)),
                "unpaired tool result"
            );
        }
    }
    ensure!(
        pending.is_empty(),
        "worker stopped with unknown tool effects; inspect them before starting new work (no calls were replayed)"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
