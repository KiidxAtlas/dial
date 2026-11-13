use super::seance;
use super::{
    Handoff, ISOLATION_BACKENDS, Job, MAX_BATCH, WorkPacket, model_routes, role_for_agent,
};
use crate::config::subagents::AGENTS;
use crate::config::subagents::builtin;
use crate::config::types::Config;
use crate::tools::Workspace;
use serde_json::Value;
use serde_json::json;

/// Longest task prompt or `output_schema` (bytes).
const MAX_PROMPT: usize = 32 * 1024;
const MAX_SCHEMA: usize = 16 * 1024;
/// Longest shared batch `context` (bytes).
const MAX_CONTEXT: usize = 16 * 1024;
/// Most skills one item may preload.
const MAX_SKILLS: usize = 8;
const MAX_NAME: usize = 40;
/// Budget for all answers in one `task` result; the parent clips tool output beyond this anyway.
const RESULT_BUDGET: usize = 14_000;

pub(in crate::agent) struct Request<'c> {
    pub batch: bool,
    pub jobs: Vec<Job<'c>>,
    pub model_profile: Option<String>,
}

/// Turn raw `task` arguments into validated jobs, or say precisely what is wrong. Nothing runs
/// unless every task in the call is valid.
pub(in crate::agent) fn parse<'c>(args: &Value, config: &'c Config) -> Result<Request<'c>, String> {
    if args.get("contract").is_some() {
        return Err(
            "contracted workflow nodes require the main agent's explicit start path".into(),
        );
    }
    let model_profile = optional_str(args, "model_profile")?.map(str::to_owned);
    if let Some(name) = &model_profile {
        if !config.experimental.profile_matching {
            return Err("`model_profile` requires experimental.profile_matching = true".into());
        }
        if !config.model_profiles.contains_key(name) {
            return Err(format!("unknown configured model profile {name:?}"));
        }
    }
    let (entries, batch) = match (
        crate::tools::present(args, "task"),
        crate::tools::present(args, "tasks"),
    ) {
        (true, true) => return Err("pass either `task` or `tasks`, not both".into()),
        (false, false) => return Err("task needs a non-empty `task` or a `tasks` array".into()),
        (true, false) => (vec![args.clone()], false),
        (false, true) => {
            let tasks = args["tasks"].as_array().ok_or("`tasks` must be an array")?;
            if tasks.is_empty() || tasks.len() > MAX_BATCH {
                return Err(format!(
                    "`tasks` needs 1 to {MAX_BATCH} entries (got {}); split larger batches across calls",
                    tasks.len()
                ));
            }
            (tasks.clone(), true)
        }
    };
    let shared_context = optional_str(args, "context")?;
    if let Some(text) = shared_context
        && text.len() > MAX_CONTEXT
    {
        return Err(format!("`context` exceeds {MAX_CONTEXT} bytes"));
    }
    let default_schema = if batch {
        args.get("output_schema").filter(|value| !value.is_null())
    } else {
        None
    };
    let limit = if batch {
        (RESULT_BUDGET / entries.len()).max(1_500)
    } else {
        RESULT_BUDGET
    };
    let mut jobs: Vec<Job> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            if entry.get("contract").is_some() {
                return Err("contract is a main-agent single-task property, not a batch or nested task property".into());
            }
            let place = if batch {
                format!("tasks[{index}]")
            } else {
                "task".to_owned()
            };
            parse_job(
                index,
                entry,
                shared_context,
                default_schema,
                batch,
                limit,
                config,
            )
            .map_err(|why| format!("{place}: {why}"))
        })
        .collect::<Result<_, _>>()?;
    for job in &mut jobs {
        job.model_routes = model_routes(config, job, model_profile.as_deref())?;
        job.role = job.model_routes[0].role;
    }
    Ok(Request {
        batch,
        jobs,
        model_profile,
    })
}

fn parse_job<'c>(
    index: usize,
    entry: &Value,
    shared_context: Option<&str>,
    default_schema: Option<&Value>,
    batch: bool,
    limit: usize,
    config: &'c Config,
) -> Result<Job<'c>, String> {
    let prompt = entry
        .get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or("needs a non-empty `task`")?;
    if prompt.len() > MAX_PROMPT {
        return Err(format!("`task` exceeds {MAX_PROMPT} bytes"));
    }
    let solution_space = optional_str(entry, "solution_space")?
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("needs a non-empty `solution_space`: how open-ended the problem is")?;
    let skills = parse_skills(entry.get("skills"))?;
    let requested = optional_str(entry, "agent")?.unwrap_or("scout");
    let mut allowed_tools = None;
    let mut custom_steps = None;
    let mut autoload: &[String] = &[];
    let (spec, preset, kind) = match builtin(requested) {
        Some(spec) => (spec, None, spec.name),
        None => match config.subagents.get_key_value(requested) {
            Some((name, text)) => (
                builtin("scout").ok_or("scout agent missing")?,
                Some(text.as_str()),
                name.as_str(),
            ),
            None if config.agents.contains_key(requested) => {
                let (name, agent) = config
                    .agents
                    .get_key_value(requested)
                    .ok_or("custom agent missing")?;
                allowed_tools = agent.tools.as_deref();
                custom_steps = agent.max_steps;
                autoload = agent.autoload_skills.as_slice();
                (
                    builtin(&agent.base).ok_or("custom agent base is not a built-in agent")?,
                    Some(agent.prompt.as_str()),
                    name.as_str(),
                )
            }
            None => {
                let known: Vec<&str> = AGENTS
                    .iter()
                    .map(|a| a.name)
                    .chain(config.subagents.keys().map(String::as_str))
                    .chain(config.agents.keys().map(String::as_str))
                    .collect();
                return Err(format!(
                    "unknown agent {requested:?}; available: {}",
                    known.join(", ")
                ));
            }
        },
    };
    let max_steps = match entry.get("max_steps") {
        None | Some(Value::Null) => custom_steps.unwrap_or(spec.max_steps),
        Some(n) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or("`max_steps` must be a nonnegative 32-bit integer; 0 means unlimited")?,
    };
    let label = optional_str(entry, "name")?
        .map(|name| {
            name.chars()
                .filter(|c| !c.is_control())
                .take(MAX_NAME)
                .collect::<String>()
        })
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| {
            if batch {
                format!("{kind}#{}", index + 1)
            } else {
                kind.to_owned()
            }
        });
    let schema = entry
        .get("output_schema")
        .filter(|value| !value.is_null())
        .or(default_schema)
        .cloned();
    if let Some(schema) = &schema {
        if schema.to_string().len() > MAX_SCHEMA {
            return Err(format!("`output_schema` exceeds {MAX_SCHEMA} bytes"));
        }
        crate::agent::window::output_schema::check_schema(schema)
            .map_err(|why| format!("invalid `output_schema`: {why}"))?;
    }
    let isolation_backend = optional_str(entry, "isolation_backend")?
        .map(parse_isolation_backend)
        .transpose()?;
    let mut isolated = match optional_str(entry, "isolation")? {
        None if isolation_backend.is_none() => false,
        None | Some("copy") if spec.mutates => true,
        None | Some("copy") => {
            return Err(
                "`isolation` and `isolation_backend` apply only to workers; read-only agents never change the project"
                    .into(),
            );
        }
        Some(other) => return Err(format!("unknown isolation {other:?}; use \"copy\"")),
    };
    let acceptance_checks = match entry.get("acceptance_checks") {
        None => Vec::new(),
        Some(value) => {
            let ids: Vec<String> = serde_json::from_value(value.clone())
                .map_err(|_| "acceptance_checks must be an array of configured check names")?;
            crate::agent::verify::worker_acceptance::validate_contract(&ids, &config.verification)?;
            if spec.mutates {
                isolated = true;
            }
            ids
        }
    };
    let work_packet = match entry.get("work_packet") {
        None | Some(Value::Null) => None,
        Some(_) if !config.experimental.work_packet_contracts => {
            return Err("`work_packet` requires experimental.work_packet_contracts = true".into());
        }
        Some(value) => Some(parse_work_packet(value)?),
    };
    let seance = if spec.name == "seance" || entry.get("source_session").is_some() {
        if spec.mutates {
            return Err("source_session is a read-only audit capability".into());
        }
        let source = optional_str(entry, "source_session")?
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("agent seance needs a non-empty `source_session`: a session id prefix or a worker id from this session")?;
        if spec.name == "seance" && !skills.is_empty() {
            return Err(
                "agent seance reads only the record it is given; `skills` does not apply".into(),
            );
        }
        let model = optional_str(entry, "model")?
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_owned);
        if spec.name != "seance" && model.is_some() {
            return Err(
                "model is a seance-only override; other auditors keep their configured role".into(),
            );
        }
        Some(seance::Seance::new(source.to_owned(), model))
    } else {
        if optional_str(entry, "source_session")?.is_some()
            || optional_str(entry, "model")?.is_some()
        {
            return Err("`source_session` and `model` apply only to agent seance".into());
        }
        None
    };
    let mut autoload_skills = autoload.to_vec();
    for skill in skills {
        if !autoload_skills.contains(&skill) {
            autoload_skills.push(skill);
        }
    }
    Ok(Job {
        depth: 1,
        resume_messages: Vec::new(),
        resume_tools: None,
        index,
        label,
        kind,
        spec,
        role: if entry.get("agent").is_none() {
            "main"
        } else {
            role_for_agent(spec)
        },
        model_routes: Vec::new(),
        preset,
        allowed_tools,
        prompt: prompt.to_owned(),
        solution_space: solution_space.to_owned(),
        shared_context: shared_context.map(str::to_owned),
        handoff: Handoff::default(),
        max_steps,
        schema,
        isolated,
        isolation_backend,
        limit,
        work_packet,
        acceptance_checks,
        promotion: None,
        autoload_skills,
        preloaded: String::new(),
        seance,
    })
}

/// Per-item `skills`: names of skills to preload before the child starts.
fn parse_skills(value: Option<&Value>) -> Result<Vec<String>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or("`skills` must be an array of skill names")?;
    if items.len() > MAX_SKILLS {
        return Err(format!("`skills` takes at most {MAX_SKILLS} names"));
    }
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| "`skills` entries must be non-empty strings".to_owned())
        })
        .collect()
}

fn parse_work_packet(value: &Value) -> Result<WorkPacket, String> {
    let object = value.as_object().ok_or("`work_packet` must be an object")?;
    if object.len() != 3
        || !["scope", "interface", "acceptance"]
            .iter()
            .all(|key| object.contains_key(*key))
    {
        return Err(
            "`work_packet` must contain only `scope`, `interface`, and `acceptance`".into(),
        );
    }
    let text = |key: &str, max: usize| -> Result<String, String> {
        let value = object
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("`work_packet.{key}` must be a non-empty string"))?;
        if value.len() > max {
            return Err(format!("`work_packet.{key}` exceeds {max} bytes"));
        }
        Ok(value.to_owned())
    };
    let acceptance = object
        .get("acceptance")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty() && items.len() <= 16)
        .ok_or("`work_packet.acceptance` needs 1 to 16 conditions")?
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let value = value
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    format!("`work_packet.acceptance[{index}]` must be a non-empty string")
                })?;
            if value.len() > 512 {
                return Err(format!(
                    "`work_packet.acceptance[{index}]` exceeds 512 bytes"
                ));
            }
            Ok(value.to_owned())
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(WorkPacket {
        scope: text("scope", 2048)?,
        interface: text("interface", 2048)?,
        acceptance,
    })
}

/// Per-task `isolation_backend`: one of the `[isolation] backend` names.
fn parse_isolation_backend(name: &str) -> Result<crate::config::isolation::Preference, String> {
    serde_json::from_value(json!(name)).map_err(|_| {
        format!(
            "unknown isolation_backend {name:?}; use one of {}",
            ISOLATION_BACKENDS.join(", ")
        )
    })
}

/// Refuse the whole call before any worker starts when a task names an isolation backend that
/// cannot run for this project. Tasks without `isolation_backend` keep the configured backend,
/// which is still checked when the worker's copy is made.
pub(in crate::agent) fn check_isolation_backends(
    jobs: &[Job],
    workspace: &Workspace,
    config: &Config,
) -> Result<(), String> {
    for job in jobs {
        let Some(preference) = job.isolation_backend else {
            continue;
        };
        crate::agent::workers::isolation::backend::check_requested(
            preference,
            workspace.root(),
            config.task.isolation_source_files,
        )
        .map_err(|why| {
            format!(
                "{}: {why}; no worker was started. Choose another isolation_backend, or omit it to use the configured [isolation] backend",
                job.label
            )
        })?;
    }
    Ok(())
}

fn optional_str<'a>(entry: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match entry.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a string")),
    }
}
