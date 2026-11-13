use super::seance;
use super::{
    Approver, Direct, Env, Handoff, Job, Lanes, ModelRoute, Outcome, Rebase,
    check_isolation_backends, parse, render, run_concurrent, run_isolated, run_serial,
};
use crate::agent::Ui;
use crate::agent::make_backend;
use crate::agent::safety::approval::PermissionRule;
use crate::agent::window::prompt;
use crate::agent::workers::child;
use crate::config::paths::Layout;
use crate::config::types::Approval;
use crate::config::types::Config;
use crate::config::types::Harness;
use crate::llm::provider::Backend;
use crate::runtime::tool_output::ToolOutput;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::Ordering;

/// The parent's pieces a `task` call needs, borrowed out of the agent so the call can run while the
/// agent's other state (todo list, session, …) stays untouched.
pub(in crate::agent) struct Parent<'a> {
    pub env: Env<'a>,
    pub approval: Approval,
    pub always: &'a mut Vec<PermissionRule>,
    pub cost: &'a mut Option<f64>,
    pub main: &'a mut Backend,
    /// Cached local backend and the model it serves, for a role that has its own model.
    pub side: &'a mut Option<(String, Backend)>,
    /// LoRA adapter of the active version; only used when children share the main local backend.
    pub adapter: Option<&'a Path>,
    /// Parent goal, open todos and working state; each child receives them in its first message.
    pub handoff: Handoff,
}

/// Run a `task` call to completion and format the result for the model.
pub(in crate::agent) fn run(
    parent: Parent<'_>,
    session_id: Option<&str>,
    args: &Value,
    ui: &mut dyn Ui,
) -> ToolOutput {
    let Parent {
        env,
        approval,
        always,
        cost,
        main,
        side,
        adapter,
        handoff,
    } = parent;
    if !env.config.task.can_spawn_at(0) {
        return ToolOutput::err("task spawning is disabled by task.max_recursion_depth");
    }
    let config = with_file_agents(env.config);
    let mut request = match parse(args, &config).and_then(|request| {
        check_isolation_backends(&request.jobs, env.workspace, &config).map(|()| request)
    }) {
        Ok(request) => request,
        Err(why) => {
            return ToolOutput {
                execution: Default::default(),
                data: None,
                text: why,
                is_error: true,
            };
        }
    };
    if let Err(why) = seance::attach(
        &mut request.jobs,
        env.layout,
        session_id,
        &config,
        env.workspace.artifacts(),
    ) {
        return ToolOutput {
            execution: Default::default(),
            data: None,
            text: why,
            is_error: true,
        };
    }
    handoff.apply(&mut request.jobs);
    if let Err(why) = preload_skills(&mut request.jobs, env.layout) {
        return ToolOutput {
            execution: Default::default(),
            data: None,
            text: why,
            is_error: true,
        };
    }
    let main_model = env.config.selection().display_label();
    // Every job runs on its role's model. All roles are resolved before any job starts, so a bad
    // model profile fails the whole call before anything runs.
    let mut plans: Vec<(&str, Config, Harness, Vec<&Job>)> = Vec::new();
    for job in &request.jobs {
        let role = job.model_routes[0].config.clone();
        let controls = serde_json::to_value(&role).expect("validated model controls serialize");
        if let Some((_, _, _, jobs)) = plans.iter_mut().find(|(key, config, _, _)| {
            *key == job.role && serde_json::to_value(config).ok().as_ref() == Some(&controls)
        }) {
            jobs.push(job);
            continue;
        }
        let harness = match role.effective_harness(env.harness) {
            Ok(harness) => harness,
            Err(e) => return ToolOutput::err(format!("subagent model profile: {e:#}")),
        };
        plans.push((job.role, role, harness, vec![job]));
    }
    for (_, _, _, jobs) in &mut plans {
        jobs.sort_by_key(|job| job.spec.mutates);
    }
    plans.sort_by_key(|(_, _, _, jobs)| jobs.iter().any(|job| job.spec.mutates));
    let total = request.jobs.len();
    let mut outcomes: Vec<Option<Outcome>> = request.jobs.iter().map(|_| None).collect();
    // Read-only and isolated jobs on a remote endpoint run concurrently across roles when there are several.
    // Everything else (in-place writers, local models) runs in order, one role group at a time.
    let parallel = plans
        .iter()
        .filter(|(_, role, _, _)| role.endpoint.is_some())
        .flat_map(|(_, _, _, jobs)| jobs.iter())
        .filter(|job| !job.spec.mutates || job.isolated)
        .count()
        > 1;
    let mut parallel_jobs: Vec<(&Job, Env)> = Vec::new();
    let mut serial: Vec<Vec<&Job>> = Vec::with_capacity(plans.len());
    for (key, role, harness, jobs) in &plans {
        ui.role(key, &role.selection().display_label());
        let env = Env {
            config: role,
            harness,
            ..env
        };
        let (concurrent, rest): (Vec<&Job>, Vec<&Job>) = jobs.iter().copied().partition(|job| {
            parallel && role.endpoint.is_some() && (!job.spec.mutates || job.isolated)
        });
        parallel_jobs.extend(concurrent.into_iter().map(|job| (job, env)));
        serial.push(rest);
    }
    run_concurrent(
        env,
        &parallel_jobs,
        config.task.max_concurrency.max(1),
        Direct {
            approval,
            always: &mut *always,
            root: env.workspace.root(),
            extra_roots: env.workspace.additional_roots(),
            layout: env.layout,
            plan_mode: env.plan_mode,
            cost: &mut *cost,
            ui: &mut *ui,
        },
        total,
        &mut outcomes,
    );
    for ((_key, role, harness, _), rest) in plans.iter().zip(serial) {
        if rest.is_empty() {
            continue;
        }
        let assigned = role.selection() != env.config.selection();
        let env = Env {
            config: role,
            harness,
            ..env
        };
        // A local model server is one shared resource; remote endpoints take concurrent requests.
        let shared = role.endpoint.is_none();
        let lanes = Lanes {
            main: &mut *main,
            side: &mut *side,
            adapter,
            shared,
            assigned,
        };
        run_serial(
            env,
            role,
            &rest,
            lanes,
            Direct {
                approval,
                always: &mut *always,
                root: env.workspace.root(),
                extra_roots: env.workspace.additional_roots(),
                layout: env.layout,
                plan_mode: env.plan_mode,
                cost: &mut *cost,
                ui: &mut *ui,
            },
            total,
            &mut outcomes,
        );
    }
    ui.role("main", &main_model);
    let models: std::collections::BTreeMap<usize, String> = request
        .jobs
        .iter()
        .map(|job| {
            (
                job.index,
                job.model_routes[0].config.selection().display_name(),
            )
        })
        .collect();
    render(&request, outcomes, &models, Some(env.workspace))
}

/// Only explicit spawn permissions expose delegation to a child.
pub(in crate::agent) fn spawn_policy(
    config: &Config,
    job: &Job,
) -> crate::agent::base::agent_defs::Spawns {
    crate::agent::base::agent_defs::Spawns::from_list(
        config
            .agents
            .get(job.kind)
            .and_then(|agent| agent.spawns.clone())
            .unwrap_or_else(|| job.spec.spawns.iter().map(|s| (*s).to_owned()).collect()),
    )
}

pub(in crate::agent) fn nested(
    env: Env,
    parent: &Job,
    args: &Value,
    names: &[String],
    approver: &mut dyn Approver,
) -> ToolOutput {
    if args["action"].as_str().unwrap_or("run") != "run" {
        return ToolOutput::err("descendants support synchronous task run only");
    }
    if !env.config.task.can_spawn_at(parent.depth) {
        return ToolOutput::err("task recursion depth reached");
    }
    if let Err(reason) = crate::agent::safety::hooks::gate(
        &env.config.hooks,
        env.workspace.root(),
        "before_subagent_spawn",
        json!({"args":args,"agent":parent.kind,"depth":parent.depth}),
        env.interrupt,
        &mut |notice| approver.notice(notice),
    ) {
        return ToolOutput::err(reason).with_status(crate::tools::ToolStatus::Blocked);
    }
    let config = with_file_agents(env.config);
    let mut request = match parse(args, &config).and_then(|request| {
        check_isolation_backends(&request.jobs, env.workspace, &config).map(|()| request)
    }) {
        Ok(request) => request,
        Err(error) => return ToolOutput::err(error),
    };
    let policy = spawn_policy(&config, parent);
    if request
        .jobs
        .iter()
        .any(|job| !policy.allows(job.kind) || (!parent.spec.mutates && job.spec.mutates))
    {
        return ToolOutput::err(
            "descendant role is not permitted by the parent's spawn or write policy",
        );
    }
    let mut harness = env.harness.clone();
    harness.tools.retain(|name| names.contains(name));
    if let Err(error) = preload_skills(&mut request.jobs, env.layout) {
        return ToolOutput::err(error);
    }
    let mut outcomes = Vec::new();
    let mut models = BTreeMap::new();
    for job in &mut request.jobs {
        job.depth = parent.depth + 1;
        models.insert(
            job.index,
            job.model_routes[0].config.selection().display_name(),
        );
        let role = &job.model_routes[0].config;
        let mut role_harness = match role.effective_harness(&harness) {
            Ok(value) => value,
            Err(error) => return ToolOutput::err(format!("descendant model profile: {error:#}")),
        };
        role_harness.tools.retain(|name| names.contains(name));
        let mut descendant = Rebase {
            inner: approver,
            from: env.workspace.root(),
            to: env.workspace.root(),
            descendant: Some(&job.label),
        };
        outcomes.push(Some(run_isolated(
            Env {
                config: role,
                harness: &role_harness,
                ..env
            },
            role,
            job,
            &mut descendant,
        )));
    }
    render(&request, outcomes, &models, Some(env.workspace))
}

/// Loads each job's autoloaded skills through the same `skill` body loader the model uses, so a
/// spawned agent starts with that guidance in its system prompt. A missing skill fails the spawn.
pub(in crate::agent) fn preload_skills(
    jobs: &mut [Job<'_>],
    layout: Option<&Layout>,
) -> Result<(), String> {
    for job in jobs
        .iter_mut()
        .filter(|job| !job.autoload_skills.is_empty())
    {
        let layout = layout.ok_or("autoload_skills need a Dial layout to find skills")?;
        let mut bodies = Vec::new();
        for name in &job.autoload_skills {
            let output = crate::storage::skills::skill_tool::call(
                layout,
                &serde_json::json!({ "name": name }),
            );
            if output.is_error {
                return Err(format!("agent {}: {}", job.kind, output.text));
            }
            bodies.push(output.text);
        }
        job.preloaded = bodies.join("\n\n");
    }
    Ok(())
}

/// Adds file-defined agents (project and user definition directories) to a per-spawn copy of the
/// config as `[agents.<name>]`-shaped entries, so they get the same base, prompt, tool narrowing,
/// writer approval and autoloaded skills as configured agents. Built-in, preset and configured names
/// keep their precedence.
pub(in crate::agent) fn with_file_agents(config: &Config) -> Config {
    use crate::agent::base::agent_defs::{Catalog, Source, Spawns};
    use crate::config::types::AgentConfig;
    let mut merged = config.clone();
    for def in Catalog::load(config).iter() {
        if matches!(
            def.source,
            Source::Builtin | Source::Preset | Source::Config
        ) || merged.agents.contains_key(&def.name)
            || merged.subagents.contains_key(&def.name)
        {
            continue;
        }
        let spawns = match &def.spawns {
            Spawns::None => Vec::new(),
            Spawns::All => vec!["*".to_owned()],
            Spawns::Only(list) => list.clone(),
        };
        merged.agents.insert(
            def.name.clone(),
            AgentConfig {
                base: if def.mutates { "worker" } else { "scout" }.into(),
                description: def.description.clone(),
                prompt: def.prompt.clone(),
                tools: def.restrict.clone(),
                max_steps: (def.max_steps > 0).then_some(def.max_steps),
                spawns: Some(spawns),
                model: def.model.clone(),
                thinking_level: def.thinking.clone(),
                output: def.output.clone(),
                autoload_skills: def.autoload_skills.clone(),
            },
        );
    }
    merged
}

/// Text-only surfaces (print, RPC without a runtime tree) learn about workers from these notices; an
/// interactive transcript draws them inside the task card instead.
pub(super) fn announce_start(ui: &mut dyn Ui, job: &Job, total: usize) {
    if ui.renders_workers() {
        return;
    }
    ui.notice(&format!(
        "subagent {} ({}/{total}): {}",
        job.label,
        job.index + 1,
        prompt::truncate_chars(&job.prompt, 70)
    ));
}

pub(super) fn worker_body(
    ui: &mut dyn Ui,
    job: &Job,
    role: &Config,
) -> Option<crate::runtime::Body> {
    let tree = ui.runtime_tree()?;
    let parent = tree
        .bodies("tool")
        .into_iter()
        .rev()
        .find(|b| b.read(|n| n.label == "task" && n.result.phase == crate::runtime::Phase::Running))
        .map(|b| b.read(|n| n.id.clone()));
    let body = tree.start("worker", &job.label, parent);
    let started_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    body.settle(String::new(),Some(json!({"name":job.label,"agent":job.kind,"role":job.role,"model":role.selection().display_name(),"model_source":job.model_routes[0].source,"model_explanation":job.model_routes[0].explanation,"status":"running","phase":"starting","steps":0,"tools":0,"started_ms":started_ms,"task":prompt::truncate_chars(&job.prompt, 120)})),crate::runtime::Phase::Running);
    ui.result_node(&body);
    Some(body)
}

pub(in crate::agent) fn update_model_route(body: &crate::runtime::Body, route: &Value) {
    let (text, mut data) = body.read(|node| {
        (
            node.result.text.clone(),
            node.result.data.clone().unwrap_or(json!({})),
        )
    });
    if let Some(fields) = route.as_object() {
        for (key, value) in fields {
            data[key] = value.clone();
        }
    }
    body.settle(text, Some(data), crate::runtime::Phase::Running);
}

/// Fallbacks are safe only before a child has performed any work. Never restart a
/// partially executed job: its owner receives the failure and retained evidence.
pub(in crate::agent) fn run_child(
    env: Env,
    job: &Job,
    backend: &mut Backend,
    adapter: Option<&Path>,
    approver: &mut dyn Approver,
) -> Outcome {
    if let Some(route) = job.model_routes.first() {
        approver.model_route(&route.metadata());
    }
    let mut outcome = child::run(env, job, backend, adapter, approver);
    let mut route_used = job.model_routes.first().map(ModelRoute::metadata);
    let mut attempts = outcome.steps;
    for route in job.model_routes.iter().skip(1) {
        if !job.resume_messages.is_empty()
            || !outcome.retryable_provider
            || outcome.steps > 1
            || env.interrupt.load(Ordering::SeqCst)
        {
            break;
        }
        backend.release();
        let mut harness = match route.config.effective_harness(env.harness) {
            Ok(harness) => harness,
            Err(error) => {
                return Outcome::failed(attempts, format!("subagent fallback: {error:#}"));
            }
        };
        harness
            .tools
            .retain(|name| env.harness.tools.contains(name));
        let mut alternate = match make_backend(&route.config) {
            Ok(backend) => backend,
            Err(error) => {
                return Outcome::failed(attempts, format!("subagent fallback: {error:#}"));
            }
        };
        let mut metadata = route.metadata();
        if route.source != "fallback" {
            metadata["model_explanation"] = json!(format!(
                "Provider-error alternate route selected (before worker progress): {}",
                route.explanation
            ));
        }
        approver.model_route(&metadata);
        route_used = Some(metadata);
        let earlier_inspections = outcome.inspections;
        let earlier_scheduling = outcome.scheduling;
        outcome = child::run(
            Env {
                config: &route.config,
                harness: &harness,
                ..env
            },
            job,
            &mut alternate,
            None,
            approver,
        );
        alternate.release();
        outcome.inspections.extend(earlier_inspections);
        outcome.scheduling.extend(earlier_scheduling);
        attempts = attempts.saturating_add(outcome.steps);
    }
    outcome.steps = attempts;
    outcome.model_route = route_used;
    outcome
}

pub(super) fn finish_worker(body: &Option<crate::runtime::Body>, outcome: &Outcome) {
    if let Some(body) = body {
        let phase = outcome.status.phase;
        let text = outcome.status.text.clone();
        let mut data = body.read(|n| n.result.data.clone().unwrap_or(json!({})));
        data["steps"] = json!(outcome.steps);
        if let Some(started) = data["started_ms"].as_u64() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(started, |d| d.as_millis() as u64);
            data["elapsed_ms"] = json!(now.saturating_sub(started));
        }
        data["result"] = outcome.data.clone().unwrap_or_else(|| json!(text));
        data["acceptance"] = json!(outcome.acceptance);
        if let Some(rules) = &outcome.portable_rules {
            data["portable_rules"] = rules.clone();
        }
        if let Some(isolation) = &outcome.isolation {
            data["isolation"] = json!(isolation);
        }
        data["status"] = json!(outcome.status.wire());
        body.settle(text, Some(data), phase);
    }
}

pub(super) fn announce_end(ui: &mut dyn Ui, job: &Job, outcome: &Outcome) {
    if ui.renders_workers() {
        return;
    }
    let verdict = if outcome.status.phase == crate::runtime::Phase::Completed {
        "finished".to_owned()
    } else {
        format!(
            "{}: {}",
            outcome.status.phase.label(),
            prompt::truncate_chars(&outcome.status.text, 80)
        )
    };
    ui.notice(&format!(
        "subagent {} {verdict} after {} step(s)",
        job.label, outcome.steps
    ));
}
