//! `list_jobs` and `stop_job`: the session's background processes, served by the controller from
//! the `bg` owner's control handle so a client can watch and stop jobs while a turn runs. These
//! are the rows and the stop action of the terminal's processes panel; PTY attach stays terminal-only.
use super::coded;
use crate::tools::integrations::ext::Tool;
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) fn handle(
    background: Option<&Arc<dyn Tool>>,
    method: &str,
    params: &Value,
) -> Result<Value> {
    let background = background.ok_or_else(|| {
        coded(
            ErrorCode::NotFound,
            "background job control is unavailable in this session",
        )
    })?;
    match method {
        "list_jobs" => Ok(list(background.as_ref())),
        "stop_job" => {
            let name = params["name"]
                .as_str()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| coded(ErrorCode::InvalidParams, "stop_job requires params.name"))?;
            let stopped = background
                .kill_job(name)
                .map_err(|error| coded(ErrorCode::NotFound, error))?;
            Ok(json!({"name": name, "stopped": stopped}))
        }
        _ => Err(super::unknown_method()),
    }
}

fn list(background: &dyn Tool) -> Value {
    let jobs: Vec<Value> = background
        .jobs_view(None)
        .map(|view| view.jobs)
        .unwrap_or_default()
        .into_iter()
        .map(|job| {
            json!({
                "name": job.name,
                "command": job.command,
                "status": job.status,
                "running": job.running,
                "stop_requested": job.stop_requested,
                "terminal": job.terminal,
            })
        })
        .collect();
    json!({"jobs": jobs})
}
