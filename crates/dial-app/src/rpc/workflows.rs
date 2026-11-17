//! Working modes and side conversations through the same owners as the terminal.
use super::coded;
use crate::agent::Agent;
use crate::agent::modes::btw::SideSnapshot;
use crate::config::paths::Layout;
use crate::side::SideThreadState;
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde::Deserialize;
use serde_json::{Value, json};

fn parse<T: serde::de::DeserializeOwned>(params: &Value) -> Result<T> {
    serde_json::from_value(params.clone()).map_err(|error| coded(ErrorCode::InvalidParams, error))
}

fn nonempty<'a>(text: &'a str, field: &str) -> Result<&'a str> {
    let text = text.trim();
    if text.is_empty() {
        return Err(coded(
            ErrorCode::InvalidParams,
            format!("{field} must not be empty"),
        ));
    }
    Ok(text)
}

/// Session modes, including the persistent goal and handoff coordinators.
pub(super) fn modes(agent: &Agent<'_>) -> Value {
    let config = agent.effective_config();
    json!({"plan":agent.plan_mode(), "modes":agent.modes(),
        "advisor":{"enabled":agent.advisor_enabled(),"completion_only":agent.advisor_completion_only(),"model":agent.advisor_model()},
        "external_thinking":agent.external_thinking_enabled(), "plan_yolo_target":agent.plan_yolo_target(),
        "settings":{"turn_optimizer":config.experimental.turn_optimizer,"fast_mode":config.fast_mode,
            "slow_mode":config.slow_mode,"service_tier":config.service_tier,"extended_context":config.extended_context}})
}

#[derive(Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum ModeRequest {
    Vibe {
        enabled: bool,
    },
    Skillful {
        enabled: Option<bool>,
    },
    Plan {
        enabled: bool,
    },
    Prewalk {
        action: PrewalkAction,
    },
    Advisor {
        state: AdvisorState,
    },
    ExternalThinking {
        enabled: bool,
    },
    PlanYolo {
        enabled: bool,
        target: Option<String>,
    },
    Drafter {
        enabled: bool,
    },
    Fast {
        state: FastState,
    },
    Slow {
        enabled: bool,
    },
    ExtendedContext {
        enabled: bool,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PrewalkAction {
    Arm,
    Restart,
    Off,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AdvisorState {
    On,
    Completion,
    Off,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FastState {
    On,
    Ultra,
    Off,
}

/// Plan persistent preferences without publishing them. The actor must refuse active workers,
/// save through settings_store and adopt through its normal settings rebuild boundary.
pub(super) fn mode_settings(
    config: &crate::config::Config,
    params: &Value,
) -> Result<
    Option<(
        crate::config::settings_store::Scope,
        Vec<crate::config::settings_store::Change>,
    )>,
> {
    use crate::config::settings_store::{Change, Scope};
    use crate::config::working_modes::{fast_plan, slow_plan};
    let boolean = |path: &str, enabled| {
        vec![Change::Set {
            path: path.into(),
            value: toml::Value::Boolean(enabled),
        }]
    };
    Ok(match parse::<ModeRequest>(params)? {
        ModeRequest::Drafter { enabled } => Some((
            Scope::Project,
            boolean("experimental.turn_optimizer", enabled),
        )),
        ModeRequest::ExtendedContext { enabled } => {
            Some((Scope::Global, boolean("extended_context", enabled)))
        }
        ModeRequest::Fast { state } => {
            let action = match state {
                FastState::On => "on",
                FastState::Ultra => "ultra",
                FastState::Off => "off",
            };
            let provider = config.selection().provider;
            let plan = fast_plan(config, &provider, action)?;
            if plan.changes.is_empty() && !matches!(state, FastState::Off) {
                return Err(coded(ErrorCode::Refused, plan.note));
            }
            Some((Scope::Global, plan.changes))
        }
        ModeRequest::Slow { enabled } => {
            let provider = config.selection().provider;
            let plan = slow_plan(config, &provider, if enabled { "on" } else { "off" })?;
            if enabled && plan.changes.is_empty() {
                return Err(coded(ErrorCode::Refused, plan.note));
            }
            Some((Scope::Global, plan.changes))
        }
        _ => None,
    })
}

/// Idle-only mode transition; no persistent settings or approval policy are changed.
pub(super) fn set_mode(agent: &mut Agent<'_>, params: &Value) -> Result<Value> {
    let notices = match parse::<ModeRequest>(params)? {
        ModeRequest::Advisor { state } => {
            match state {
                AdvisorState::On => agent.set_advisor(true),
                AdvisorState::Off => agent.set_advisor(false),
                AdvisorState::Completion => agent.set_advisor_completion_only(),
            }
            Vec::new()
        }
        ModeRequest::ExternalThinking { enabled } => {
            agent.set_external_thinking(enabled);
            Vec::new()
        }
        ModeRequest::PlanYolo { enabled, target } => {
            if enabled {
                let default = if agent.prewalk_resolve("@small").is_ok() {
                    "@small"
                } else {
                    "@main"
                };
                let target = target
                    .as_deref()
                    .map(|value| nonempty(value, "target"))
                    .transpose()?;
                agent
                    .plan_yolo_arm(target.unwrap_or(default))
                    .map_err(|error| coded(ErrorCode::Refused, error))?;
            } else {
                if target.is_some() {
                    return Err(coded(
                        ErrorCode::InvalidParams,
                        "target requires enabled=true",
                    ));
                }
                agent
                    .plan_yolo_disarm()
                    .map_err(|error| coded(ErrorCode::Refused, error))?;
            }
            Vec::new()
        }
        ModeRequest::Drafter { .. }
        | ModeRequest::Fast { .. }
        | ModeRequest::Slow { .. }
        | ModeRequest::ExtendedContext { .. } => {
            return Err(coded(
                ErrorCode::Refused,
                "persistent modes require settings adoption",
            ));
        }
        ModeRequest::Vibe { enabled } => {
            agent.set_vibe(enabled)?;
            Vec::new()
        }
        ModeRequest::Skillful { enabled } => {
            agent.set_skillful(enabled)?;
            Vec::new()
        }
        ModeRequest::Plan { enabled } => {
            agent.set_plan_mode(enabled);
            Vec::new()
        }
        ModeRequest::Prewalk { action } => match action {
            PrewalkAction::Arm => {
                let target = agent.prewalk_resolve("@small")?;
                agent.prewalk_arm(target)?.1
            }
            PrewalkAction::Restart => agent.prewalk_restart("@small")?.notices,
            PrewalkAction::Off => agent.prewalk_disarm()?,
        },
    };
    Ok(json!({"state":modes(agent), "notices":notices}))
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum GoalRequest {
    Get {},
    Set {
        objective: String,
        token_budget: Option<u64>,
    },
    Guided {
        idea: String,
    },
    Pause {},
    Resume {},
    Drop {},
    Budget {
        token_budget: Option<u64>,
    },
}

/// Changes the persistent goal. The actor must execute any returned prompt through its normal
/// turn lifecycle before replying, including compute admission, cancellation and projections.
pub(super) fn goal(agent: &mut Agent<'_>, params: &Value) -> Result<(Value, Option<String>)> {
    let request = parse::<GoalRequest>(params)?;
    if matches!(request, GoalRequest::Get {}) {
        return Ok((json!({"goal":agent.modes().goal}), None));
    }
    let mut next = agent.modes().goal.clone();
    let prompt = apply_goal(&mut next, &request)?;
    // Validate against a copy first: refusals must not persist partial state.
    agent.goal_mutate(|state| *state = next)?;
    Ok((json!({"goal":agent.modes().goal}), prompt))
}

/// Validate and preview goal admission without publishing a transition. Read-only and paused
/// goal edits need no model lease; prompt-producing edits must acquire it before persistence.
pub(super) fn goal_has_prompt(agent: &Agent<'_>, params: &Value) -> Result<bool> {
    let request = parse::<GoalRequest>(params)?;
    let mut next = agent.modes().goal.clone();
    Ok(apply_goal(&mut next, &request)?.is_some())
}

fn apply_goal(
    state: &mut crate::agent::modes::goal::State,
    request: &GoalRequest,
) -> Result<Option<String>> {
    use crate::agent::modes::goal::Status;
    let refused = |error: &str| coded(ErrorCode::Refused, error);
    let budget = |value: Option<u64>| -> Result<Option<u64>> {
        if value == Some(0) {
            return Err(coded(
                ErrorCode::InvalidParams,
                "token_budget must be positive or null",
            ));
        }
        Ok(value)
    };
    Ok(match request {
        GoalRequest::Get {} => None,
        GoalRequest::Set {
            objective,
            token_budget,
        } => {
            let objective = nonempty(objective, "objective")?;
            state.start(objective, budget(*token_budget)?);
            Some(objective.to_owned())
        }
        GoalRequest::Guided { idea } => {
            let idea = nonempty(idea, "idea")?;
            if state.goal.as_ref().is_some_and(|goal| {
                matches!(
                    goal.status,
                    Status::Active | Status::Paused | Status::BudgetLimited
                )
            }) {
                return Err(refused("a goal already exists; drop it first"));
            }
            state.interview = true;
            Some(idea.to_owned())
        }
        GoalRequest::Pause {} => {
            state.pause().map_err(refused)?;
            None
        }
        GoalRequest::Resume {} => {
            state.resume().map_err(refused)?;
            Some("/goal resume".into())
        }
        GoalRequest::Drop {} => {
            state.tool(&json!({"op":"drop"})).map_err(refused)?;
            None
        }
        GoalRequest::Budget { token_budget } => {
            let value = budget(*token_budget)?;
            match state.goal.as_ref().map(|goal| goal.status) {
                None | Some(Status::Dropped) => return Err(refused("no active goal")),
                Some(Status::Complete) => return Err(refused("goal is already complete")),
                Some(_) => {}
            }
            state.set_budget(value);
            state.running().then(|| match value {
                Some(tokens) => format!("/goal budget {tokens}"),
                None => "/goal budget off".into(),
            })
        }
    })
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum SideRequest {
    List {},
    Ask {
        question: String,
        #[serde(default)]
        new: bool,
    },
    Open {
        thread_id: String,
    },
    Answer {},
    Cancel {},
}

/// Side operations may run while the main agent is busy, but only with its frozen snapshot.
/// The caller owns session binding and drains events on a regular tick even with no requests.
pub(super) fn side(
    state: &mut SideThreadState,
    snapshot: Option<&SideSnapshot>,
    params: &Value,
) -> Result<Value> {
    match parse::<SideRequest>(params)? {
        SideRequest::List {} => Ok(side_projection(state)),
        SideRequest::Ask { question, new } => {
            let question = nonempty(&question, "question")?;
            if state.busy() {
                return Err(coded(ErrorCode::Busy, "a side answer is still streaming"));
            }
            let snapshot =
                snapshot.ok_or_else(|| coded(ErrorCode::Busy, "no side snapshot available"))?;
            if snapshot.requires_compute_lock() {
                return Err(coded(
                    ErrorCode::Refused,
                    "RPC side inference requires an independent foreground compute lease for this configuration; local side inference with compute locks is not supported",
                ));
            }
            let message = state
                .start(snapshot, question, new)
                .map_err(|error| coded(ErrorCode::Refused, error))?;
            Ok(json!({"message":message,"state":side_projection(state)}))
        }
        SideRequest::Open { thread_id } => {
            let id = nonempty(&thread_id, "thread_id")?;
            if !state.threads().iter().any(|thread| thread.id == id) {
                return Err(coded(ErrorCode::NotFound, "side thread not found"));
            }
            state.open(id)?;
            Ok(side_projection(state))
        }
        SideRequest::Answer {} => Ok(
            json!({"answer":state.last_answer().map_err(|error| coded(ErrorCode::NotFound, error))?}),
        ),
        SideRequest::Cancel {} => Ok(json!({"cancelled":state.cancel()})),
    }
}

pub(super) fn side_projection(state: &SideThreadState) -> Value {
    json!({"open_thread_id":state.open_thread(), "threads":state.threads(), "streaming":state.streaming().map(|(thread,question,answer)| json!({"thread_id":thread,"question":question,"answer":answer}))})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromoteRequest {
    thread_id: String,
}

/// Forks an answered side thread at its saved ancestry without changing the main session.
pub(super) fn promote(
    state: &mut SideThreadState,
    agent: &Agent<'_>,
    layout: &Layout,
    params: &Value,
) -> Result<Value> {
    let request = parse::<PromoteRequest>(params)?;
    let id = nonempty(&request.thread_id, "thread_id")?;
    state.bind(agent.session())?;
    if state.busy() {
        return Err(coded(
            ErrorCode::Busy,
            "wait for the side answer before promotion",
        ));
    }
    if !state.threads().iter().any(|thread| thread.id == id) {
        return Err(coded(ErrorCode::NotFound, "side thread not found"));
    }
    let session = state
        .promote(agent, layout, id)
        .map_err(|error| coded(ErrorCode::Refused, error))?;
    Ok(json!({"thread_id":id, "session_id":session}))
}

#[cfg(test)]
mod tests;
