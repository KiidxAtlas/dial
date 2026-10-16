//! Explicit continuation of a worker. History is never replayed as tool execution.
use crate::storage::paths::Layout;
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
    let transcripts = super::child_transcript::transcript_dir(layout, session);
    let dir = layout
        .state_dir
        .join("subagent-leases")
        .join(transcripts.file_name().context("invalid parent session")?);
    crate::platform::fs::create_dir_owner_only(&std::fs::DirBuilder::new().recursive(true), &dir)?;
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
    let entry = super::child_transcript::scan(layout, session)
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
    for line in BufReader::new(file).lines() {
        let value: Value = serde_json::from_str(&line?)?;
        if value["type"] == "execution_contract" {
            execution = Some(value.clone());
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
mod tests {
    use super::*;
    #[test]
    fn persisted_resume_checks_workspace_lease_and_exact_execution_contract() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
        let sink = super::super::child_transcript::ChildTranscript::create(&layout, "parent", "task-1", json!({"id":"task-1", "resume_contract":{
            "version":1,"workspace":identity(project.path()).unwrap(), "task":{"task":"original","solution_space":"s"},
            "tools":["read","write"], "model":"test/model", "definition":null
        }})).unwrap();
        sink.execution_contract(&["read".into()], "test/model")
            .unwrap();
        sink.record(&json!({"role":"user","content":"original"}))
            .unwrap();
        sink.record(&json!({"role":"assistant","content":"finding"}))
            .unwrap();
        let held = lease(&layout, "parent", "task-1").unwrap();
        assert!(load(&layout, "parent", project.path(), "task-1", "follow-up").is_err());
        drop(held);
        let other = tempfile::tempdir().unwrap();
        assert!(load(&layout, "parent", other.path(), "task-1", "follow-up").is_err());
        // A flock is shared with any child forked (not yet exec'd) by a concurrently running test, so
        // the release after `drop` can lag by a few milliseconds.
        let saved = (0..100)
            .find_map(|_| {
                load(&layout, "parent", project.path(), "task-1", "follow-up")
                    .ok()
                    .or_else(|| {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        None
                    })
            })
            .expect("lease is released once the holder is dropped");
        assert_eq!(saved.tools, ["read"]);
        assert_eq!(saved.messages.len(), 2);
        assert_eq!(saved.task["task"], "follow-up");
        drop(saved);
        sink.record(&json!({"role":"assistant","tool_calls":[{"id":"uncertain-write"}]}))
            .unwrap();
        assert!(load(&layout, "parent", project.path(), "task-1", "follow-up").is_err());
    }

    #[test]
    fn continuation_requires_complete_effect_records() {
        let call = json!({"role":"assistant","tool_calls":[{"id":"write-1"}]});
        assert!(validate_history(&[call.clone()]).is_err());
        assert!(
            validate_history(&[
                call,
                json!({"role":"tool","tool_call_id":"write-1","content":"done"})
            ])
            .is_ok()
        );
        assert!(validate_history(&[json!({"role":"tool","tool_call_id":"missing"})]).is_err());
        assert!(
            validate_history(&[
                json!({"role":"assistant","content":"[message truncated: 300000 bytes]"})
            ])
            .is_err()
        );
    }
}
