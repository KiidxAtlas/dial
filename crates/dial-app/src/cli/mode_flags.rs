//! Launch-mode flags: validation, normalization, and application.
//!
//! Flags that Dial honors are applied through real subsystems:
//! - `--plan [model]` → plan mode on; a named model becomes the `planner` model role (`ModeFlags::apply_plan_model`).
//! - `--goal <OBJECTIVE>` → fresh interactive session; the goal engine starts with the objective before the first turn.
//! - `--plan-yolo` / `--plan-yolo-into <MODEL>` → plan mode on; the first plan is approved automatically and execution hands off to the target model.
//! - `--advisor` → the `coach` role reviews tool evidence and can request one in-loop repair (launch-scoped).
//! - `--external-thinking` → private `<scratchpad>` reasoning with provider reasoning forced off (launch-scoped).
//! - `--hide-thinking` → config overlay `[ui] show_thinking = false`.
//! - `--prewalk` / `--prewalk-into` → arm the todo-gated handoff to `@small` (or the named target) once per session.
//! - `--no-prewalk` → no startup arm (the default).
//!
//! Every other flag has no Dial subsystem. `resolve` rejects them with an explicit
//! reason instead of accepting and ignoring them.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::agent::Agent;

/// Launch-mode flags as they appear on the Dial command line.
#[derive(Debug, Clone, Default, clap::Args)]
#[command(next_help_heading = crate::cli::help_heading::MODES)]
pub struct ModeArgs {
    /// Start a goal-directed session. Requires a non-empty objective.
    #[arg(long, value_name = "OBJECTIVE")]
    pub goal: Option<String>,

    /// Enable plan mode. Takes an optional model-role name; a bare `--plan` enables plan mode.
    #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
    pub plan: Option<String>,

    /// Auto-approve the plan and hand execution to a target model.
    #[arg(long)]
    pub plan_yolo: bool,

    /// Model used for the plan-yolo handoff (requires `--plan-yolo`).
    #[arg(long, value_name = "MODEL", requires = "plan_yolo")]
    pub plan_yolo_into: Option<String>,

    /// Run a pre-walk pass before the main turn.
    #[arg(long, conflicts_with = "no_prewalk")]
    pub prewalk: bool,

    /// Disable the pre-walk pass.
    #[arg(long, conflicts_with = "prewalk_into")]
    pub no_prewalk: bool,

    /// Model used for the pre-walk pass (requires `--prewalk`).
    #[arg(long, value_name = "MODEL", requires = "prewalk")]
    pub prewalk_into: Option<String>,

    /// Pair a second model that reviews each turn.
    #[arg(long)]
    pub advisor: bool,

    /// Use a private reasoning scratchpad hidden from the user.
    #[arg(long)]
    pub external_thinking: bool,

    /// Hide the thinking block in the transcript.
    #[arg(long)]
    pub hide_thinking: bool,

    /// Load a hook module (repeatable).
    #[arg(long, value_name = "PATH", help_heading = crate::cli::help_heading::EXTENSIONS)]
    pub hook: Vec<String>,
}

/// Launch facts the validators need that are not part of the mode flags.
#[derive(Debug, Clone, Copy, Default)]
pub struct LaunchContext {
    /// Interactive terminal session (not `--print` or other non-interactive modes).
    pub interactive: bool,
    /// Resuming or continuing an existing session.
    pub resumes_session: bool,
    /// A positional prompt was given on the command line.
    pub has_positional_prompt: bool,
    /// Prompt text arrived on stdin.
    pub has_piped_input: bool,
    /// `--no-tools`: the launch runs with an empty tool allowlist.
    pub no_tools: bool,
}

/// Facts about the startup that constrain `--goal`.
#[derive(Debug, Clone, Copy, Default)]
pub struct GoalStartup {
    pub has_positional_prompt: bool,
    pub has_piped_input: bool,
    pub plan_yolo: bool,
    pub plan_starts_on_startup: bool,
    pub no_tools: bool,
}

/// Validated, normalized mode flags ready to apply.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModeFlags {
    /// Plan mode requested by `--plan`.
    pub plan_mode: bool,
    /// Model spec from `--plan <model>`, trimmed. `None` for bare `--plan`.
    pub plan_model: Option<String>,
    /// Config-level overrides; merge into `app::Options.overlay`.
    pub overlay: toml::Table,
    /// Startup prewalk target spec (`@small` by default). Armed once per session by `apply_to_agent`.
    pub prewalk: Option<String>,
    /// `--goal <OBJECTIVE>`: starts the goal engine with this objective before the first turn.
    pub goal: Option<String>,
    /// `--plan-yolo` target spec (`@small` by default). Armed once per session by `apply_to_agent`.
    pub plan_yolo: Option<String>,
    /// `--advisor`: evidence-based review and one repair pass with the `coach` role. Launch-scoped.
    pub advisor: bool,
    /// `--external-thinking`: private scratchpad reasoning. Launch-scoped.
    pub external_thinking: bool,
}

/// Normalize a `--goal` objective: trim, then require non-empty.
pub fn normalize_goal(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("--goal requires a non-empty objective.");
    }
    Ok(trimmed.to_string())
}

/// Reject `--goal` in launch modes that cannot host a goal session.
pub fn validate_goal_launch(goal: Option<&str>, ctx: LaunchContext) -> Result<()> {
    if goal.is_none() {
        return Ok(());
    }
    if !ctx.interactive {
        bail!("--goal requires an interactive terminal (not --print or --mode).");
    }
    if ctx.resumes_session {
        bail!("--goal requires a fresh session (no resume, continue, fork, or import).");
    }
    Ok(())
}

/// Reject `--goal` combined with startup inputs or settings that conflict with it.
pub fn validate_goal_startup(goal: Option<&str>, startup: GoalStartup) -> Result<()> {
    if goal.is_none() {
        return Ok(());
    }
    if startup.has_positional_prompt || startup.has_piped_input {
        bail!("--goal cannot be combined with a positional message, @file, or stdin prompt.");
    }
    if startup.plan_yolo {
        bail!("--goal cannot be combined with --plan-yolo.");
    }
    if startup.no_tools {
        bail!("--goal cannot be combined with --no-tools.");
    }
    if startup.plan_starts_on_startup {
        bail!(
            "--goal cannot be combined with plan mode starting on startup; disable startup plan mode first."
        );
    }
    Ok(())
}

pub fn config_overlay(hide_thinking: bool) -> toml::Table {
    let mut overlay = toml::Table::new();
    if hide_thinking {
        let mut ui = toml::Table::new();
        ui.insert("show_thinking".to_string(), toml::Value::from(false));
        overlay.insert("ui".to_string(), toml::Value::Table(ui));
    }
    overlay
}

/// Canonical path of an operator-named module file. It must exist and be a regular file.
fn module_file(path: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|err| anyhow::anyhow!("extension module not found: {} ({err})", path.display()))?;
    if !canonical.is_file() {
        bail!(
            "extension module must be a file, not a directory: {}",
            path.display()
        );
    }
    Ok(canonical)
}

/// Config overlay for the module flags. `--hook` adds one `[[hooks]]` entry per lifecycle event,
/// each running the same program.
fn module_overlay(args: &ModeArgs) -> Result<toml::Table> {
    let mut overlay = toml::Table::new();
    let mut hooks = Vec::new();
    for raw in &args.hook {
        let program = module_file(Path::new(raw.trim()))?;
        for event in crate::config::hooks::EVENTS {
            let mut hook = toml::Table::new();
            hook.insert("event".to_string(), toml::Value::from(*event));
            hook.insert(
                "command".to_string(),
                toml::Value::Array(vec![toml::Value::from(
                    program.to_string_lossy().into_owned(),
                )]),
            );
            hooks.push(toml::Value::Table(hook));
        }
    }
    if !hooks.is_empty() {
        overlay.insert("hooks".to_string(), toml::Value::Array(hooks));
    }
    Ok(overlay)
}

/// Every launch-time config overlay: UI/extension flags plus the module flags.
fn launch_overlay(args: &ModeArgs) -> Result<toml::Table> {
    let mut overlay = config_overlay(args.hide_thinking);
    crate::config::merge_overlay(&mut overlay, module_overlay(args)?);
    Ok(overlay)
}

/// Validate and normalize the mode flags. Rejects any flag Dial cannot honor.
pub fn resolve(args: &ModeArgs, ctx: LaunchContext) -> Result<ModeFlags> {
    let goal = match args.goal.as_deref() {
        Some(raw) => Some(normalize_goal(raw)?),
        None => None,
    };
    validate_goal_launch(goal.as_deref(), ctx)?;
    validate_goal_startup(
        goal.as_deref(),
        GoalStartup {
            has_positional_prompt: ctx.has_positional_prompt,
            has_piped_input: ctx.has_piped_input,
            no_tools: ctx.no_tools,
            plan_yolo: args.plan_yolo,
            plan_starts_on_startup: args.plan.is_some(),
        },
    )?;

    if args.prewalk && args.no_prewalk {
        bail!("--prewalk and --no-prewalk cannot be used together.");
    }

    let plan_mode = args.plan.is_some() || args.plan_yolo;
    let plan_model = args
        .plan
        .as_deref()
        .map(str::trim)
        .filter(|spec| !spec.is_empty())
        .map(str::to_owned);
    let prewalk = (args.prewalk || args.prewalk_into.is_some()).then(|| {
        args.prewalk_into
            .clone()
            .unwrap_or_else(|| "@small".to_owned())
    });
    let plan_yolo = args.plan_yolo.then(|| {
        args.plan_yolo_into
            .clone()
            .unwrap_or_else(|| "@small".to_owned())
    });
    Ok(ModeFlags {
        plan_mode,
        plan_model,
        prewalk,
        plan_yolo,
        goal,
        advisor: args.advisor,
        external_thinking: args.external_thinking,
        overlay: launch_overlay(args)?,
    })
}

impl ModeFlags {
    /// Protocol actors do not implement interactive startup modes. Reject intent rather than
    /// silently losing it when dispatch hands off only the process-scoped `Launch` restrictions.
    pub fn validate_protocol(&self, protocol: &str) -> Result<()> {
        let mut unsupported = Vec::new();
        if self.plan_mode || self.plan_model.is_some() || self.plan_yolo.is_some() {
            unsupported.push("--plan/--plan-yolo");
        }
        if self.prewalk.is_some() {
            unsupported.push("--prewalk");
        }
        if self.goal.is_some() {
            unsupported.push("--goal");
        }
        if self.advisor {
            unsupported.push("--advisor");
        }
        if self.external_thinking {
            unsupported.push("--external-thinking");
        }
        if !unsupported.is_empty() {
            bail!(
                "{} cannot be used with {protocol}; use an interactive session for launch modes",
                unsupported.join(", ")
            );
        }
        Ok(())
    }

    /// Merge this flag set's config overlay into an existing overlay table.
    pub fn merge_into_overlay(&self, overlay: &mut toml::Table) {
        crate::config::merge_overlay(overlay, self.overlay.clone());
    }

    /// Apply the launch modes to a live agent: plan mode, the plan-yolo and goal startups, the
    /// launch-scoped advisor and external-thinking flags, then the prewalk arm. Returns notices to show.
    pub fn apply_to_agent(&self, agent: &mut Agent<'_>) -> Result<Vec<String>> {
        if self.plan_mode {
            agent.set_plan_mode(true);
        }
        agent.set_launch_modes(self.advisor, self.external_thinking);
        if let Some(spec) = &self.plan_yolo {
            agent.plan_yolo_startup(spec)?;
        }
        if let Some(objective) = &self.goal {
            agent.goal_mutate(|goal| goal.start(objective, None))?;
        }
        match &self.prewalk {
            Some(spec) => agent.prewalk_startup(spec),
            None => Ok(Vec::new()),
        }
    }

    /// Apply `--plan <model>` as the in-memory `planner` model role.
    /// The spec resolves through the `/switch` selector, so `@role`, `provider/id`, and bare ids all work.
    /// Bare `--plan` and no `--plan` return the config unchanged.
    pub fn apply_plan_model(
        &self,
        mut config: crate::config::types::Config,
    ) -> Result<crate::config::types::Config> {
        let Some(spec) = self.plan_model.as_deref() else {
            return Ok(config);
        };
        let selection = crate::agent::modes::switch::resolve(&config, spec).map_err(|_| {
            anyhow::anyhow!(
                "Model \"{spec}\" not found. Run \"dial models\" to see available models."
            )
        })?;
        config.model_roles.insert("planner".to_owned(), selection);
        config.validate()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests;
