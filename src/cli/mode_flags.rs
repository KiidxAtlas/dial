//! Launch-mode flags: validation, normalization, and application.
//!
//! Flags that Dial honors are applied through real subsystems:
//! - `--plan [model]` → plan mode on; a named model becomes the `planner` model role (`ModeFlags::apply_plan_model`).
//! - `--goal <OBJECTIVE>` → fresh interactive session; the goal engine starts with the objective before the first turn.
//! - `--plan-yolo` / `--plan-yolo-into <MODEL>` → plan mode on; the first plan is approved automatically and execution hands off to the target model.
//! - `--advisor` → the `coach` role reviews tool evidence and can request one in-loop repair (launch-scoped).
//! - `--external-thinking` → private `<scratchpad>` reasoning with provider reasoning forced off (launch-scoped).
//! - `--hide-thinking` → config overlay `[ui] show_thinking = false`.
//! - `--no-extensions` → config overlay `[extensions] enabled = false`.
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

    /// Disable all extensions.
    #[arg(long)]
    pub no_extensions: bool,

    /// Load an extension module (`--extension` / `-e`, repeatable).
    #[arg(short = 'e', long = "extension", value_name = "PATH")]
    pub extensions: Vec<String>,

    /// Load a hook module (repeatable).
    #[arg(long, value_name = "PATH")]
    pub hook: Vec<String>,

    /// Load an absolute-path extension that is trusted (repeatable).
    #[arg(long, value_name = "ABS_PATH")]
    pub trusted_extension: Vec<String>,

    /// Add a plugin root directory (repeatable).
    #[arg(long, value_name = "DIR")]
    pub plugin_dir: Vec<PathBuf>,
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

/// Validate `--trusted-extension` paths: non-empty and absolute. Returns them as paths.
pub fn validate_trusted_extensions(
    paths: &[String],
    extension_or_hook_given: bool,
) -> Result<Vec<PathBuf>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    if extension_or_hook_given {
        bail!("--trusted-extension cannot be combined with --extension, -e, or --hook.");
    }
    paths
        .iter()
        .map(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                bail!("--trusted-extension requires a non-empty absolute path.");
            }
            let path = PathBuf::from(trimmed);
            if !path.is_absolute() {
                bail!("--trusted-extension requires an absolute path: {trimmed}");
            }
            Ok(path)
        })
        .collect()
}

/// Build the config overlay for the flags Dial honors.
pub fn config_overlay(hide_thinking: bool, no_extensions: bool) -> toml::Table {
    let mut overlay = toml::Table::new();
    if hide_thinking {
        let mut ui = toml::Table::new();
        ui.insert("show_thinking".to_string(), toml::Value::from(false));
        overlay.insert("ui".to_string(), toml::Value::Table(ui));
    }
    if no_extensions {
        let mut ext = toml::Table::new();
        ext.insert("enabled".to_string(), toml::Value::from(false));
        overlay.insert("extensions".to_string(), toml::Value::Table(ext));
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

fn plugin_entry(program: &Path) -> toml::Value {
    let mut entry = toml::Table::new();
    entry.insert(
        "program".to_string(),
        toml::Value::from(program.to_string_lossy().into_owned()),
    );
    toml::Value::Table(entry)
}

fn plugin_name(prefix: &str, index: usize, program: &Path) -> String {
    let file = program
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{prefix}-{index}-{file}")
}

/// Config overlay for the module flags. `--extension` and `--trusted-extension` become
/// `[extensions.plugins]` stdio programs. `--trusted-extension` is an exact allowlist, so it also
/// turns off ambient discovery. `--hook` adds one `[[hooks]]`
/// entry per lifecycle event, each running the same program. `--plugin-dir` adds every regular
/// file directly inside the directory as a plugin program.
fn module_overlay(args: &ModeArgs) -> Result<toml::Table> {
    let mut overlay = toml::Table::new();
    let mut plugins = toml::Table::new();
    for (index, raw) in args.extensions.iter().enumerate() {
        let program = module_file(Path::new(raw.trim()))?;
        plugins.insert(plugin_name("cli", index, &program), plugin_entry(&program));
    }
    let trusted = validate_trusted_extensions(&args.trusted_extension, false)?;
    for (index, raw) in trusted.iter().enumerate() {
        let program = module_file(raw)?;
        plugins.insert(
            plugin_name("trusted", index, &program),
            plugin_entry(&program),
        );
    }
    for (index, dir) in args.plugin_dir.iter().enumerate() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|err| {
                anyhow::anyhow!("--plugin-dir cannot be read: {} ({err})", dir.display())
            })?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_file())
            .collect();
        if files.is_empty() {
            bail!(
                "--plugin-dir contains no plugin programs: {}",
                dir.display()
            );
        }
        files.sort();
        for file in &files {
            let program = module_file(file)?;
            plugins.insert(plugin_name("dir", index, &program), plugin_entry(&program));
        }
    }
    if !plugins.is_empty() {
        let mut extensions = toml::Table::new();
        extensions.insert("plugins".to_string(), toml::Value::Table(plugins));
        if !trusted.is_empty() {
            extensions.insert("discover".to_string(), toml::Value::from(false));
        }
        overlay.insert("extensions".to_string(), toml::Value::Table(extensions));
    }
    let mut hooks = Vec::new();
    for raw in &args.hook {
        let program = module_file(Path::new(raw.trim()))?;
        for event in crate::agent::hooks::EVENTS {
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
    let module_given = !args.extensions.is_empty()
        || !args.hook.is_empty()
        || !args.trusted_extension.is_empty()
        || !args.plugin_dir.is_empty();
    if args.no_extensions && module_given {
        bail!(
            "--no-extensions cannot be combined with -e/--extension, --trusted-extension, --hook, or --plugin-dir."
        );
    }
    let mut overlay = config_overlay(args.hide_thinking, args.no_extensions);
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

    let extension_or_hook_given = !args.extensions.is_empty() || !args.hook.is_empty();
    validate_trusted_extensions(&args.trusted_extension, extension_or_hook_given)?;

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
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        mode: ModeArgs,
    }

    fn parse(args: &[&str]) -> ModeArgs {
        let mut argv = vec!["dial"];
        argv.extend_from_slice(args);
        TestCli::try_parse_from(argv).unwrap().mode
    }

    fn interactive() -> LaunchContext {
        LaunchContext {
            interactive: true,
            ..Default::default()
        }
    }

    #[test]
    fn goal_objective_is_trimmed_and_required() {
        assert_eq!(normalize_goal("  ship it  ").unwrap(), "ship it");
        let err = normalize_goal("   ").unwrap_err().to_string();
        assert_eq!(err, "--goal requires a non-empty objective.");
    }

    #[test]
    fn goal_launch_rejects_non_interactive_and_resume() {
        let print = LaunchContext {
            interactive: false,
            ..Default::default()
        };
        assert!(validate_goal_launch(Some("x"), print).is_err());
        let resumed = LaunchContext {
            interactive: true,
            resumes_session: true,
            ..Default::default()
        };
        assert!(validate_goal_launch(Some("x"), resumed).is_err());
        assert!(validate_goal_launch(Some("x"), interactive()).is_ok());
        assert!(validate_goal_launch(None, LaunchContext::default()).is_ok());
    }

    #[test]
    fn goal_startup_rejects_conflicting_inputs() {
        let ok_base = GoalStartup::default();
        assert!(validate_goal_startup(Some("x"), ok_base).is_ok());
        let prompt = GoalStartup {
            has_positional_prompt: true,
            ..ok_base
        };
        assert!(
            validate_goal_startup(Some("x"), prompt)
                .unwrap_err()
                .to_string()
                .contains("positional")
        );
        let yolo = GoalStartup {
            plan_yolo: true,
            ..ok_base
        };
        assert!(
            validate_goal_startup(Some("x"), yolo)
                .unwrap_err()
                .to_string()
                .contains("--plan-yolo")
        );
        let plan = GoalStartup {
            plan_starts_on_startup: true,
            ..ok_base
        };
        assert!(
            validate_goal_startup(Some("x"), plan)
                .unwrap_err()
                .to_string()
                .contains("plan mode")
        );
    }

    #[test]
    fn trusted_extensions_must_be_absolute_and_exclusive() {
        let abs = vec!["/opt/ext.js".to_string()];
        assert_eq!(
            validate_trusted_extensions(&abs, false).unwrap(),
            vec![PathBuf::from("/opt/ext.js")]
        );
        let rel = vec!["ext.js".to_string()];
        assert!(
            validate_trusted_extensions(&rel, false)
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );
        assert!(validate_trusted_extensions(&[" ".to_string()], false).is_err());
        assert!(
            validate_trusted_extensions(&abs, true)
                .unwrap_err()
                .to_string()
                .contains("cannot be combined")
        );
        assert!(validate_trusted_extensions(&[], true).unwrap().is_empty());
    }

    #[test]
    fn overlay_sets_only_requested_keys() {
        assert!(config_overlay(false, false).is_empty());
        let o = config_overlay(true, true);
        assert_eq!(o["ui"]["show_thinking"].as_bool(), Some(false));
        assert_eq!(o["extensions"]["enabled"].as_bool(), Some(false));
        let mut base = toml::Table::new();
        crate::config::merge(&mut base, config_overlay(true, false));
        assert_eq!(base["ui"]["show_thinking"].as_bool(), Some(false));
        assert!(base.get("extensions").is_none());
    }

    #[test]
    fn parses_bare_and_named_plan_and_repeatable_flags() {
        let bare = parse(&["--plan"]);
        assert_eq!(bare.plan.as_deref(), Some(""));
        let named = parse(&["--plan", "smart"]);
        assert_eq!(named.plan.as_deref(), Some("smart"));
        let ext = parse(&["-e", "a.js", "--extension", "b.js"]);
        assert_eq!(ext.extensions, vec!["a.js", "b.js"]);
    }

    #[test]
    fn plan_flag_enables_plan_mode() {
        let flags = resolve(&parse(&["--plan"]), interactive()).unwrap();
        assert!(flags.plan_mode);
        assert_eq!(flags.plan_model, None);
        assert!(!resolve(&parse(&[]), interactive()).unwrap().plan_mode);
    }

    #[test]
    fn plan_model_resolves_aliases_into_the_planner_role() {
        let mut config = crate::config::types::Config::default();
        config.model_roles.insert(
            "small".to_owned(),
            crate::llm::model_config::ModelSelection {
                provider: "openai".to_owned(),
                model: "gpt-small".to_owned(),
                ..Default::default()
            },
        );
        let aliased = resolve(&parse(&["--plan", "@small"]), interactive()).unwrap();
        assert!(aliased.plan_mode);
        assert_eq!(aliased.plan_model.as_deref(), Some("@small"));
        let applied = aliased.apply_plan_model(config.clone()).unwrap();
        assert_eq!(applied.model_roles["planner"].model, "gpt-small");
        let bare = resolve(&parse(&["--plan"]), interactive()).unwrap();
        assert_eq!(
            bare.apply_plan_model(config.clone())
                .unwrap()
                .model_roles
                .get("planner"),
            None
        );
        let err = resolve(&parse(&["--plan", "no-such-model"]), interactive())
            .unwrap()
            .apply_plan_model(config)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "Model \"no-such-model\" not found. Run \"dial models\" to see available models."
        );
    }

    #[test]
    fn honored_flags_produce_overlay() {
        let flags = resolve(
            &parse(&["--hide-thinking", "--no-extensions"]),
            interactive(),
        )
        .unwrap();
        assert_eq!(flags.overlay["ui"]["show_thinking"].as_bool(), Some(false));
        assert_eq!(
            flags.overlay["extensions"]["enabled"].as_bool(),
            Some(false)
        );
        let mut target = toml::Table::new();
        flags.merge_into_overlay(&mut target);
        assert_eq!(target, flags.overlay);
    }

    #[test]
    fn no_prewalk_is_accepted_as_existing_behavior() {
        assert!(resolve(&parse(&["--no-prewalk"]), interactive()).is_ok());
    }

    #[test]
    fn prewalk_defaults_to_small_and_into_names_the_target() {
        assert_eq!(
            resolve(&parse(&["--prewalk"]), interactive())
                .unwrap()
                .prewalk
                .as_deref(),
            Some("@small")
        );
        let into = parse(&["--prewalk", "--prewalk-into", "@main"]);
        assert_eq!(
            resolve(&into, interactive()).unwrap().prewalk.as_deref(),
            Some("@main")
        );
        assert_eq!(resolve(&parse(&[]), interactive()).unwrap().prewalk, None);
    }

    #[test]
    fn goal_flag_resolves_to_trimmed_objective() {
        let flags = resolve(&parse(&["--goal", "  ship it  "]), interactive()).unwrap();
        assert_eq!(flags.goal.as_deref(), Some("ship it"));
        assert!(!flags.plan_mode);
        assert_eq!(flags.plan_yolo, None);
    }

    #[test]
    fn goal_flag_rejects_positional_prompt_and_plan_yolo() {
        let positional = LaunchContext {
            has_positional_prompt: true,
            ..interactive()
        };
        assert!(resolve(&parse(&["--goal", "ship"]), positional).is_err());
        assert!(resolve(&parse(&["--goal", "ship", "--plan-yolo"]), interactive()).is_err());
    }

    #[test]
    fn plan_yolo_resolves_target_and_plan_mode() {
        let bare = resolve(&parse(&["--plan-yolo"]), interactive()).unwrap();
        assert!(bare.plan_mode);
        assert_eq!(bare.plan_yolo.as_deref(), Some("@small"));
        let into = resolve(
            &parse(&["--plan-yolo", "--plan-yolo-into", "@main"]),
            interactive(),
        )
        .unwrap();
        assert_eq!(into.plan_yolo.as_deref(), Some("@main"));
        let plain = resolve(&parse(&["--plan"]), interactive()).unwrap();
        assert_eq!(plain.plan_yolo, None);
    }

    #[test]
    fn advisor_and_external_thinking_resolve_launch_scoped() {
        let flags = resolve(&parse(&["--advisor", "--external-thinking"]), interactive()).unwrap();
        assert!(flags.advisor);
        assert!(flags.external_thinking);
        assert!(
            flags.overlay.is_empty(),
            "launch-scoped flags never reach the config overlay"
        );
        let plain = resolve(&parse(&[]), interactive()).unwrap();
        assert!(!plain.advisor && !plain.external_thinking);
    }

    #[test]
    fn plan_yolo_into_requires_plan_yolo() {
        let mut argv = vec!["dial", "--plan-yolo-into", "m"];
        assert!(TestCli::try_parse_from(argv.clone()).is_err());
        argv.push("--plan-yolo");
        assert!(TestCli::try_parse_from(argv).is_ok());
    }

    #[test]
    fn prewalk_conflicts_with_no_prewalk() {
        assert!(TestCli::try_parse_from(["dial", "--prewalk", "--no-prewalk"]).is_err());
    }

    fn temp_module_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("dial-mode-flags-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn extension_flag_lands_in_plugins_overlay() {
        let dir = temp_module_dir("extension");
        let module = dir.join("ext.js");
        std::fs::write(&module, "").unwrap();
        let flags = resolve(&parse(&["-e", module.to_str().unwrap()]), interactive()).unwrap();
        let program = flags.overlay["extensions"]["plugins"]["cli-0-ext.js"]["program"]
            .as_str()
            .unwrap();
        assert_eq!(
            program,
            std::fs::canonicalize(&module).unwrap().to_str().unwrap()
        );
        assert!(flags.overlay["extensions"].get("discover").is_none());
        let missing = resolve(&parse(&["-e", "/no/such/module.js"]), interactive())
            .unwrap_err()
            .to_string();
        assert!(missing.contains("not found"), "{missing}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn trusted_extension_flag_is_an_exact_allowlist_without_discovery() {
        let dir = temp_module_dir("trusted");
        let module = dir.join("policy.js");
        std::fs::write(&module, "").unwrap();
        let flags = resolve(
            &parse(&["--trusted-extension", module.to_str().unwrap()]),
            interactive(),
        )
        .unwrap();
        assert_eq!(
            flags.overlay["extensions"]["discover"].as_bool(),
            Some(false)
        );
        assert!(
            flags.overlay["extensions"]["plugins"]
                .as_table()
                .unwrap()
                .contains_key("trusted-0-policy.js")
        );
        assert!(resolve(&parse(&["--trusted-extension", "rel.js"]), interactive()).is_err());
        let mixed = [
            "--trusted-extension",
            module.to_str().unwrap(),
            "-e",
            module.to_str().unwrap(),
        ];
        assert!(
            resolve(&parse(&mixed), interactive())
                .unwrap_err()
                .to_string()
                .contains("cannot be combined")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hook_flag_lands_in_hooks_overlay_for_every_event() {
        let dir = temp_module_dir("hook");
        let module = dir.join("hook.sh");
        std::fs::write(&module, "").unwrap();
        let flags = resolve(&parse(&["--hook", module.to_str().unwrap()]), interactive()).unwrap();
        let hooks = flags.overlay["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), crate::agent::hooks::EVENTS.len());
        let program = std::fs::canonicalize(&module)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        for (hook, event) in hooks.iter().zip(crate::agent::hooks::EVENTS) {
            assert_eq!(hook["event"].as_str(), Some(*event));
            assert_eq!(hook["command"][0].as_str(), Some(program.as_str()));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hook_overlay_appends_to_configured_hooks_instead_of_replacing_them() {
        let dir = temp_module_dir("hook-append");
        let module = dir.join("hook.sh");
        std::fs::write(&module, "").unwrap();
        let flags = resolve(&parse(&["--hook", module.to_str().unwrap()]), interactive()).unwrap();
        let project_hook = toml::Value::Table(
            toml::from_str("event = 'pre_tool_use'\ncommand = ['project-policy']").unwrap(),
        );
        let mut target = toml::Table::new();
        target.insert(
            "hooks".into(),
            toml::Value::Array(vec![project_hook.clone()]),
        );
        flags.merge_into_overlay(&mut target);
        let hooks = target["hooks"].as_array().unwrap();
        assert_eq!(hooks[0], project_hook, "the configured hook is kept first");
        assert_eq!(hooks.len(), 1 + crate::agent::hooks::EVENTS.len());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn plugin_dir_flag_lands_every_plugin_program_in_overlay() {
        let dir = temp_module_dir("plugin-dir");
        std::fs::write(dir.join("a.js"), "").unwrap();
        std::fs::write(dir.join("b.sh"), "").unwrap();
        let flags = resolve(
            &parse(&["--plugin-dir", dir.to_str().unwrap()]),
            interactive(),
        )
        .unwrap();
        let plugins = flags.overlay["extensions"]["plugins"].as_table().unwrap();
        assert_eq!(plugins.len(), 2);
        assert!(plugins.contains_key("dir-0-a.js") && plugins.contains_key("dir-0-b.sh"));
        let empty = temp_module_dir("plugin-dir-empty");
        let err = resolve(
            &parse(&["--plugin-dir", empty.to_str().unwrap()]),
            interactive(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no plugin programs"), "{err}");
        assert!(resolve(&parse(&["--plugin-dir", "/no/such/dir"]), interactive()).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&empty).unwrap();
    }
}
