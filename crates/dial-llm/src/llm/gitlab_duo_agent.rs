//! GitLab Duo Agent Platform (`gitlab-duo-agent`): one workflow per exchange,
//! driven over a WebSocket. Server tool actions become Dial tool calls so the
//! shared dispatch and approval path runs them; the result is sent back on the
//! same socket when the next exchange arrives.

mod transport;
mod wire;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use anyhow::{Context, Result, anyhow, bail, ensure};

use super::profiles::ModelProfile;
use super::transports::{OnText, Route};
use super::{Chat, Interrupted, Reply, ToolCall};
use transport::{Api, Socket};

/// Step-limit restarts allowed in one exchange before the failure surfaces.
const MAX_STEP_LIMIT_RESTARTS: u32 = 4;
/// Generic-processing-error retries allowed in one exchange.
const MAX_GENERIC_ERROR_RETRIES: u32 = 1;
/// Environment variable naming the project (`group/subgroup/project`) to run in.
/// Unset, the first project the token can develop against is used.
const PROJECT_ENV: &str = "GITLAB_DUO_PROJECT";

/// A live workflow socket kept between tool round trips.
struct Session {
    workflow_id: String,
    socket: Socket,
    /// Server `requestID` awaiting a tool result from Dial.
    pending: Option<String>,
    /// Endpoint and PAT the workflow was created with, for its stop call.
    base: String,
    token: String,
}

/// Live sessions keyed by `{base}\0{session_id}\0{model}`.
static SESSIONS: LazyLock<Mutex<HashMap<String, Session>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Result of driving the socket until the turn settles.
enum Step {
    Reply(Box<Reply>),
    Restart(Restart),
}

/// Reason the server asked for a fresh workflow.
enum Restart {
    StepLimit,
    Generic(String),
}

/// Restart counters for one exchange.
#[derive(Default)]
struct Budget {
    step_limit: u32,
    generic: u32,
}

impl Budget {
    fn spend(&mut self, restart: Restart) -> Result<()> {
        match restart {
            Restart::StepLimit => {
                ensure!(
                    self.step_limit < MAX_STEP_LIMIT_RESTARTS,
                    "GitLab Duo Agent workflow hit its step limit {} times in one turn",
                    self.step_limit + 1
                );
                self.step_limit += 1;
            }
            Restart::Generic(message) => {
                ensure!(
                    self.generic < MAX_GENERIC_ERROR_RETRIES,
                    "GitLab Duo Agent workflow failed: {message}"
                );
                self.generic += 1;
            }
        }
        Ok(())
    }
}

pub(super) fn exchange(
    route: &Route<'_>,
    req: &Chat,
    timeout: u64,
    on_text: OnText<'_, '_>,
    interrupt: Arc<AtomicBool>,
    _profile: Option<&ModelProfile>,
    _on_event: &mut dyn FnMut(&super::StreamEvent) -> bool,
) -> Result<Reply> {
    let token = route
        .auth
        .filter(|token| !token.is_empty())
        .context("GitLab Duo Agent requires sign-in or GITLAB_TOKEN; run /providers and sign in")?;
    let base = route.base_url.trim_end_matches('/');
    ensure!(
        base.starts_with("https://") || (route.allow_insecure && base.starts_with("http://")),
        "GitLab Duo Agent requires an https base URL"
    );
    let api = Api {
        base,
        token,
        interrupt: &interrupt,
        timeout,
    };
    let (system, goal) = wire::render_goal(req)?;
    let key = session_key(base, route.session_id.unwrap_or(""), route.model);
    let resume = take_resumable(&key, req, &api);
    run_turn(&api, route, req, &key, &system, &goal, resume, on_text)
}

/// Stop and drop every live workflow whose conversation key is `session_id`.
/// The stop is best-effort and time-bounded; each socket closes after its stop.
pub(super) fn forget(session_id: &str) {
    let parked: Vec<Session> = {
        let mut sessions = SESSIONS.lock().unwrap_or_else(PoisonError::into_inner);
        let keys: Vec<String> = sessions
            .keys()
            .filter(|key| key.split('\0').nth(1) == Some(session_id))
            .cloned()
            .collect();
        keys.iter().filter_map(|key| sessions.remove(key)).collect()
    };
    for session in parked {
        let interrupt = Arc::new(AtomicBool::new(false));
        let api = Api {
            base: &session.base,
            token: &session.token,
            interrupt: &interrupt,
            timeout: transport::STOP_TIMEOUT_SECS,
        };
        api.stop_workflow(&session.workflow_id);
    }
}

fn session_key(base: &str, session_id: &str, model: &str) -> String {
    format!("{base}\0{session_id}\0{model}")
}

/// Take the parked session when this request answers its pending tool call.
/// A session that cannot be resumed is stopped and dropped.
fn take_resumable(key: &str, req: &Chat, api: &Api<'_>) -> Option<(Session, String)> {
    let parked = {
        let mut sessions = SESSIONS.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.remove(key)
    }?;
    let last = req.messages.last();
    let answered = last
        .filter(|message| message.get("role").and_then(serde_json::Value::as_str) == Some("tool"))
        .and_then(|message| {
            let id = message
                .get("tool_call_id")
                .and_then(serde_json::Value::as_str)?;
            (parked.pending.as_deref() == Some(id)).then(|| {
                (
                    parked.pending.clone().unwrap_or_default(),
                    wire::content_text(message.get("content")),
                )
            })
        });
    match answered {
        Some((_, content)) => Some((parked, content)),
        None => {
            api.stop_workflow(&parked.workflow_id);
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_turn(
    api: &Api<'_>,
    route: &Route<'_>,
    req: &Chat,
    key: &str,
    system: &str,
    goal: &str,
    resume: Option<(Session, String)>,
    on_text: OnText<'_, '_>,
) -> Result<Reply> {
    let mut session = match resume {
        Some((mut session, content)) => {
            let request_id = session
                .pending
                .take()
                .context("GitLab Duo Agent resume lost its pending request")?;
            if let Err(error) = transport::send(
                &mut session.socket,
                &wire::action_response(&request_id, &content),
            ) {
                api.stop_workflow(&session.workflow_id);
                return Err(error);
            }
            session
        }
        None => start(api, route, req, system, goal)?,
    };
    let mut budget = Budget::default();
    let mut shown = String::new();
    loop {
        match drive(&mut session, api.interrupt, on_text, &mut shown) {
            Ok(Step::Reply(reply)) => {
                if reply.tool_calls.is_empty() {
                    api.stop_workflow(&session.workflow_id);
                } else {
                    let mut sessions = SESSIONS.lock().unwrap_or_else(PoisonError::into_inner);
                    sessions.insert(key.to_owned(), session);
                }
                return Ok(*reply);
            }
            Ok(Step::Restart(restart)) => {
                api.stop_workflow(&session.workflow_id);
                budget.spend(restart)?;
                session = start(api, route, req, system, goal)?;
            }
            Err(error) => {
                api.stop_workflow(&session.workflow_id);
                return Err(error);
            }
        }
    }
}

/// Create a workflow, open its socket, and send the start request.
fn start(
    api: &Api<'_>,
    route: &Route<'_>,
    req: &Chat,
    system: &str,
    goal: &str,
) -> Result<Session> {
    let explicit = std::env::var(PROJECT_ENV).ok();
    let project = api.project_for(
        explicit
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty()),
    )?;
    let root = project.root_namespace_id.clone();
    let access = api.direct_access(&root, &project.id)?;
    let workflow_id = api.create_workflow(&project.id, goal)?;
    let mut socket = match transport::connect(api, &access, &project, route.model) {
        Ok(socket) => socket,
        Err(error) => {
            api.stop_workflow(&workflow_id);
            return Err(error);
        }
    };
    let start = wire::Start {
        workflow_id: &workflow_id,
        goal,
        system,
        tools: &req.tools,
        model: route.model,
        project_id: &project.id,
        namespace_id: &project.namespace_id,
        root_namespace_id: &root,
    };
    let payload = wire::start_request(&start)?;
    if let Err(error) = transport::send(&mut socket, &payload) {
        api.stop_workflow(&workflow_id);
        return Err(error);
    }
    Ok(Session {
        workflow_id,
        socket,
        pending: None,
        base: api.base.to_owned(),
        token: api.token.to_owned(),
    })
}

/// Read events until the turn settles: text, a tool action, completion, or a
/// restart request. Text streams as it arrives; `shown` is everything the turn
/// has streamed so far, so a restarted workflow does not repeat it.
fn drive(
    session: &mut Session,
    interrupt: &AtomicBool,
    on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
    shown: &mut String,
) -> Result<Step> {
    loop {
        let event = transport::next_event(&mut session.socket, interrupt)?;
        if let Some(checkpoint) = wire::checkpoint_text(&event) {
            let delta = unseen(shown, checkpoint);
            if !delta.is_empty() {
                if let Some(callback) = on_text.as_deref_mut()
                    && !callback(delta)
                {
                    return Err(Interrupted.into());
                }
                shown.push_str(delta);
            }
        }
        match wire::status_of(&event) {
            wire::Status::Complete => {
                return Ok(Step::Reply(Box::new(Reply {
                    text: shown.clone(),
                    finish_reason: "stop".to_owned(),
                    ..Reply::default()
                })));
            }
            wire::Status::Failed(message) => {
                if wire::is_step_limit(&message) {
                    return Ok(Step::Restart(Restart::StepLimit));
                }
                if wire::is_generic_error(&message) {
                    return Ok(Step::Restart(Restart::Generic(message)));
                }
                return Err(anyhow!("GitLab Duo Agent workflow failed: {message}"));
            }
            wire::Status::Approval | wire::Status::Running => {}
        }
        // An approval event that carries a tool action is surfaced as that tool
        // call, so Dial's approval gate decides; the decision returns on the socket
        // as the matching `actionResponse`.
        if let Some(action) = wire::action_of(&event)? {
            session.pending = Some(action.request_id.clone());
            return Ok(Step::Reply(Box::new(Reply {
                text: shown.clone(),
                tool_calls: vec![ToolCall {
                    id: action.request_id,
                    name: action.name,
                    arguments: action.arguments,
                    malformed_arguments: false,
                }],
                finish_reason: "tool_calls".to_owned(),
                ..Reply::default()
            })));
        }
        if wire::status_of(&event) == wire::Status::Approval {
            bail!("GitLab Duo Agent requested an approval that carries no tool call to gate");
        }
    }
}

/// The part of `checkpoint` the turn has not streamed yet. Checkpoints carry the
/// whole text so far, so a replay of streamed text yields nothing. On divergence
/// only the text after the shared prefix is new.
fn unseen<'a>(shown: &str, checkpoint: &'a str) -> &'a str {
    let mut cut = shown
        .bytes()
        .zip(checkpoint.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !checkpoint.is_char_boundary(cut) {
        cut -= 1;
    }
    &checkpoint[cut..]
}
