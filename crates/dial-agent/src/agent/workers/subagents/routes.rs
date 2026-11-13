use super::Job;
use crate::config::subagents::AgentSpec;
use crate::config::types::Config;
use serde_json::Value;
use serde_json::json;

/// Default specialist follows the base agent purpose, including custom definitions.
/// Implementation workers use task; an assigned specialist wins over task in model_routes.
pub(super) fn role_for_agent(spec: &AgentSpec) -> &'static str {
    match spec.name {
        "reviewer" | "test-runner" => "tester",
        "seance" => "main",
        "planner" => "coach",
        _ if spec.mutates => "task",
        _ => "small",
    }
}

/// Resolve missing task roles through configured helpers, never an arbitrary catalogue model.
/// These chains are delegation-only: maintenance requests keep their own role semantics.
fn default_task_role(config: &Config, preferred: &'static str) -> (&'static str, String) {
    let candidates: &[&str] = match preferred {
        "task" => &["task", "coach"],
        "small" => &["small", "task"],
        "tester" => &["tester", "coach", "task"],
        "coach" => &["coach", "task"],
        _ => return (preferred, format!("{preferred} selected")),
    };
    let mut evaluated = Vec::new();
    for &role in candidates {
        if config.role_selection(role).is_some() {
            evaluated.push(format!("{role} assigned, selected"));
            return (role, evaluated.join(" -> "));
        }
        evaluated.push(format!("{role} unassigned"));
    }
    evaluated.push("main selected (missing-assignment default)".into());
    // Retain the requested role in diagnostics when its final fallback is Main.
    (preferred, evaluated.join(" -> "))
}

#[derive(Clone)]
pub(in crate::agent) struct ModelRoute {
    pub role: &'static str,
    pub config: Config,
    pub source: &'static str,
    /// Recorded at selection time, never reconstructed from today's configuration.
    pub explanation: String,
}

impl ModelRoute {
    pub fn metadata(&self) -> Value {
        json!({"role":self.role,"model":self.config.selection().display_name(),"model_source":self.source,"model_explanation":self.explanation})
    }
}

pub(super) fn model_routes(
    config: &Config,
    job: &Job,
    profile: Option<&str>,
) -> Result<Vec<ModelRoute>, String> {
    let definition = config.agents.get(job.kind);
    let selectors = definition.map(|d| d.model.as_slice()).unwrap_or_default();
    // A consultation runs on its explicit or source model only; profiles and definitions do not apply.
    let selectors: Vec<Option<&str>> =
        if let Some(seance) = job.seance.as_ref().filter(|_| job.spec.name == "seance") {
            vec![seance.model.as_deref()]
        } else if selectors.is_empty() || profile.is_some() {
            vec![None]
        } else {
            selectors.iter().map(|s| Some(s.as_str())).collect()
        };
    let mut routes = selectors
        .into_iter()
        .map(|selector| {
            let (mut role, evaluated) = default_task_role(config, job.role);
            let mut suffix_thinking = None;
            let mut selected = config.for_role(role);
            if let Some(selector) = selector {
                let (model, thinking) = selector
                    .rsplit_once(':')
                    .filter(|(_, level)| crate::config::agent_defs::valid_thinking_level(level))
                    .map_or((selector, None), |(model, level)| (model, Some(level)));
                suffix_thinking = thinking;
                if let Some(named) = model.strip_prefix('@') {
                    let named = if named == "default" {
                        "main"
                    } else {
                        crate::llm::model_config::LEGACY_ROLE_MIGRATIONS
                            .iter()
                            .find_map(|(old, new)| (*old == named).then_some(*new))
                            .unwrap_or(named)
                    };
                    role = crate::llm::model_config::ROLES
                        .iter()
                        .find_map(|(key, _)| {
                            (!matches!(*key, "decision" | "grounding" | "image_embedding"))
                                .then_some(*key)
                                .filter(|key| *key == named)
                        })
                        .ok_or_else(|| {
                            format!("subagent model role {named:?} is not a chat role")
                        })?;
                    selected = config.for_role(role);
                } else {
                    selected = config
                        .memory_role_config(role, Some(model))
                        .map_err(|e| format!("subagent model: {e:#}"))?;
                }
            }
            if let Some(profile) = profile {
                selected = selected
                    .with_named_model_profile(profile)
                    .map_err(|e| format!("subagent model profile: {e:#}"))?;
            }
            if let Some(level) = definition
                .and_then(|d| d.thinking_level.as_deref())
                .or(suffix_thinking)
            {
                if level == "ultra" {
                    let mut profile = selected
                        .active_model_profile()
                        .cloned()
                        .ok_or("subagent ultra thinking requires a declared model profile")?;
                    profile.reasoning_effort = Some(crate::llm::profiles::ReasoningEffort::Ultra);
                    selected = crate::config::thinking::install_thinking_profile(selected, profile)
                        .map_err(|e| format!("subagent thinking: {e:#}"))?;
                } else {
                    let level = crate::config::thinking::parse_thinking(if level == "none" {
                        "off"
                    } else {
                        level
                    })
                    .map_err(|e| format!("subagent thinking: {e}"))?;
                    selected = crate::config::thinking::apply_thinking(selected, level)
                        .map_err(|e| format!("subagent thinking: {e:#}"))?;
                }
            }
            let source = if profile.is_some() {
                "profile"
            } else if job.seance.is_some() && selector.is_some() {
                "seance"
            } else if selector.is_some() {
                "definition"
            } else if role == "main" {
                "main"
            } else if config.role_selection(role).is_some() {
                "assigned"
            } else {
                "follows main"
            };
            let reason = if let Some(profile) = profile {
                format!("Explicit model profile {profile} selected")
            } else if let Some(selector) = selector {
                format!(
                    "{} model selector {selector} selected",
                    if job.seance.is_some() {
                        "Consultation"
                    } else {
                        "Agent definition"
                    }
                )
            } else {
                evaluated
            };
            let explanation = format!(
                "{}: {reason}; route {}",
                job.kind,
                selected.selection().display_name()
            );
            Ok(ModelRoute {
                role,
                config: selected,
                source,
                explanation,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    // Configured `[retry.fallback_chains]` for the worker's role extend the route list. `run_child`
    // tries them only while the child has done no work, so a half-finished job is never restarted.
    // An operator-named profile is exact and is not substituted.
    // A consultation names its model; a fallback would answer on a model nobody asked for.
    if profile.is_none()
        && job.seance.is_none()
        && let Some(primary) = routes.first()
    {
        let role = primary.role;
        let selected = primary.config.selection();
        let chained = crate::llm::fallback::fallback_models(config, role, &selected)
            .into_iter()
            .map(|next| ModelRoute {
                role,
                config: primary.config.with_selection(&next),
                source: "fallback",
                explanation: format!(
                    "{}; provider-error fallback selected: {}",
                    primary.explanation,
                    next.display_name()
                ),
            })
            .collect::<Vec<_>>();
        routes.extend(chained);
    }
    Ok(routes)
}
