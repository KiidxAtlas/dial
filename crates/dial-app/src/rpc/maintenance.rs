//! Session maintenance and project schedules through the same owners as the terminal.
use super::coded;
use crate::agent::Agent;
use crate::config::{paths::Layout, types::Config};
use crate::schedule::{Entry, Store};
use crate::storage::{archive::Archive, session::pack as session_pack};
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicBool};

fn invalid(error: impl std::fmt::Display) -> anyhow::Error {
    coded(ErrorCode::InvalidParams, error)
}

fn owner_error(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<super::RequestError>().is_some() {
        return error;
    }
    let code = match error
        .downcast_ref::<std::io::Error>()
        .map(std::io::Error::kind)
    {
        Some(std::io::ErrorKind::AlreadyExists) => ErrorCode::Conflict,
        Some(std::io::ErrorKind::WouldBlock) => ErrorCode::Busy,
        _ => ErrorCode::Failed,
    };
    coded(code, error)
}

fn request<T: DeserializeOwned>(params: &Value) -> Result<T> {
    serde_json::from_value(if params.is_null() {
        json!({})
    } else {
        params.clone()
    })
    .map_err(invalid)
}

fn consent(confirmed: bool) -> Result<()> {
    if !confirmed {
        return Err(invalid("ask the user, then send confirm: true"));
    }
    Ok(())
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ArchiveRequest {
    view: ArchiveView,
    project: Option<PathBuf>,
    query: Option<String>,
    limit: Option<usize>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ArchiveView {
    #[default]
    Projects,
    Sessions,
    Prompts,
}

/// Read user-wide history; this never moves or deletes a session.
pub(super) fn archive(layout: &Layout, params: &Value) -> Result<Value> {
    let params: ArchiveRequest = request(params)?;
    let limit = params.limit.unwrap_or(30);
    if !(1..=crate::storage::archive::MAX_LIMIT).contains(&limit) {
        return Err(invalid("limit must be between 1 and 500"));
    }
    let project = params.project.map(|path| layout.project_root.join(path));
    let archive = Archive::new(&layout.user_dir);
    match params.view {
        ArchiveView::Projects => {
            if project.is_some() || params.query.is_some() {
                return Err(invalid("projects does not accept project or query"));
            }
            Ok(serde_json::to_value(archive.projects(limit)?)?)
        }
        ArchiveView::Sessions => {
            if params.query.is_some() {
                return Err(invalid("sessions does not accept query"));
            }
            Ok(serde_json::to_value(
                archive.sessions(project.as_deref(), limit)?,
            )?)
        }
        ArchiveView::Prompts => Ok(serde_json::to_value(archive.prompts(
            params.query.as_deref(),
            project.as_deref(),
            limit,
        )?)?),
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PackRequest {
    path: Option<PathBuf>,
}

/// Pack the active, actor-owned journal, preserving the owner's no-overwrite guarantee.
pub(super) fn pack(agent: &Agent<'_>, params: &Value) -> Result<Value> {
    let params: PackRequest = request(params)?;
    let layout = agent
        .layout()
        .ok_or_else(|| invalid("agent has no project layout"))?;
    let source = agent
        .session()
        .path()
        .ok_or_else(|| invalid("session has no journal"))?;
    let output =
        layout.project_root.join(params.path.unwrap_or_else(|| {
            PathBuf::from(format!("session-{}.dialpack", agent.session().id()))
        }));
    let report = session_pack::pack_session(layout, source, &output).map_err(owner_error)?;
    Ok(
        json!({"session_id":report.session_id,"path":output,"members":report.members,
        "artifacts":report.artifacts,"subagents":report.subagents,"bytes":report.bytes}),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BranchRequest {
    turns: usize,
    #[serde(default)]
    confirm: bool,
}

/// Rewind conversation only, with the shared session-change gate and abandoned-branch summary.
pub(super) fn branch(
    config: &Config,
    interrupt: &Arc<AtomicBool>,
    agent: &mut Agent<'_>,
    params: &Value,
) -> Result<Value> {
    let params: BranchRequest = request(params)?;
    consent(params.confirm)?;
    let available = agent.session().branchable_user_messages().len();
    if params.turns == 0 || params.turns > available {
        return Err(invalid(format!("turns must be between 1 and {available}")));
    }
    let approval = crate::rewind::preflight_rewind(agent, params.turns)
        .map_err(|error| coded(ErrorCode::Refused, error))?;
    agent.cancel_background_compaction();
    let result = crate::rewind::rewind_approved(config, interrupt, agent, approval)?;
    Ok(
        json!({"session_id":agent.session().id(),"turns":params.turns,
        "dropped":result.dropped,"note":result.note,"files_restored":false}),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelRequest {
    label: String,
}

/// Label the active leaf; an empty label clears its existing label.
pub(super) fn label(agent: &mut Agent<'_>, params: &Value) -> Result<Value> {
    let params: LabelRequest = request(params)?;
    let target = agent
        .session_mut()
        .label_leaf(&params.label)
        .map_err(invalid)?;
    let label = params
        .label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    Ok(json!({"session_id":agent.session().id(),"entry_id":target,"label":label}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveRequest {
    path: String,
    #[serde(default)]
    confirm: bool,
}

/// Move using the terminal's transactional relocation owner, validating settings before mutation.
/// The actor must rebuild with the returned configuration and adopt `agent.layout()` afterward.
pub(super) fn move_session(
    agent: &mut Agent<'_>,
    overlay: &toml::Table,
    params: &Value,
) -> Result<Config> {
    let params: MoveRequest = request(params)?;
    consent(params.confirm)?;
    if params.path.trim().is_empty() {
        return Err(invalid("path must not be empty"));
    }
    let mut config = None;
    let outcome = crate::app::move_session_checked(agent, &params.path, |target| {
        let next = crate::config::settings_store::load_session_config(target, overlay, None)
            .map_err(invalid)?
            .for_coding();
        next.require_model().map_err(invalid)?;
        super::make_backend(&next).map_err(invalid)?;
        config = Some(next);
        Ok(())
    })
    .map_err(owner_error)?;
    match outcome {
        crate::app::MoveOutcome::Refused(reason) => Err(invalid(reason)),
        crate::app::MoveOutcome::Moved(_) => config.ok_or_else(|| {
            coded(
                ErrorCode::InternalError,
                "moved project has no validated configuration",
            )
        }),
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct EmptyRequest {}

/// List persisted schedule definitions with their most recently recorded run timestamp.
pub(super) fn schedule_list(layout: &Layout, params: &Value) -> Result<Value> {
    let _: EmptyRequest = request(params)?;
    let store = Store::new(layout)?;
    let runs = store.last_runs()?;
    let entries: Vec<Value> = store.load()?.into_iter().map(|entry| {
        json!({"last_run":runs.get(&entry.id),"access":entry.access_description(),"entry":entry})
    }).collect();
    Ok(json!({"schedules":entries}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddScheduleRequest {
    id: Option<String>,
    prompt: String,
    every: Option<String>,
    #[serde(default)]
    watch: Vec<String>,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    approve_all: bool,
    #[serde(default)]
    confirm: bool,
}

/// Save a schedule with explicit consent. Read-only is the default, not blanket approval.
pub(super) fn schedule_add(layout: &Layout, params: &Value) -> Result<Value> {
    let params: AddScheduleRequest = request(params)?;
    consent(params.confirm)?;
    let entry = Entry::new(
        params.id,
        params.prompt,
        params.every.as_deref(),
        params.watch,
        params.allow,
        params.approve_all,
    )
    .map_err(invalid)?;
    // Store::add assigns an omitted id and validates under its update lock.
    let access = entry.access_description();
    let id = Store::new(layout)?.add(entry).map_err(invalid)?;
    Ok(json!({"id":id,"saved":true,"access":access,"requires_runner":true}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveScheduleRequest {
    id: String,
    #[serde(default)]
    confirm: bool,
}

/// Remove a persisted definition, never interrupting an already running child.
pub(super) fn schedule_remove(layout: &Layout, params: &Value) -> Result<Value> {
    let params: RemoveScheduleRequest = request(params)?;
    consent(params.confirm)?;
    let store = Store::new(layout)?;
    if !store.load()?.iter().any(|entry| entry.id == params.id) {
        return Err(coded(ErrorCode::NotFound, "schedule id not found"));
    }
    store.remove(&params.id)?;
    Ok(json!({"id":params.id,"removed":true}))
}

/// Report scheduler lease ownership, without launching unattended work.
pub(super) fn schedule_status(layout: &Layout, params: &Value) -> Result<Value> {
    let _: EmptyRequest = request(params)?;
    let store = Store::new(layout)?;
    Ok(
        json!({"running":store.running()?,"entries":store.load()?.len(),
        "last_runs":store.last_runs()?}),
    )
}

#[cfg(test)]
mod tests;
