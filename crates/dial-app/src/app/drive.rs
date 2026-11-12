//! The startup and turn driver: runs the first-run onboarding, then the shell loop until exit.

use crate::agent::make_backend;
use crate::app::core::App;
use crate::app::notices::memory_notice_due;
use crate::app::overlay::{OverlayOutcome, Which, overlay_destination};
use crate::app::shell::Shell;
use crate::config::types::Config;
use crate::storage::session::{Session, SessionMeta};
use anyhow::Result;
use serde_json::Value;

// ---------------------------------------------------------------------------
// The session loop: build an agent, run the shell until it quits or the config demands a rebuild.
// ---------------------------------------------------------------------------

pub(super) enum Step {
    Done,
    Quit,
    Rebuild(Box<Config>),
}

pub(super) fn drive(
    app: &mut App,
    shell: &mut Shell,
    session: Option<Session>,
    restore: bool,
) -> Result<()> {
    if !onboard(app, shell, session.as_ref())? {
        return Ok(());
    }
    let resumed = session.is_some();
    // `--restore` runs once, for the first agent of the resumed session, before the user has control.
    let mut restore_pending = restore && resumed;
    let mut session = match session {
        Some(session) => session,
        None => Session::create(
            &app.layout,
            SessionMeta {
                model: app.config.model.clone(),
            },
        )?,
    };
    let mut todo: Option<Value> = None;
    let mut approval = app.config.approval;
    let mut plan_mode = app.mode_flags.plan_mode;
    let mut startup_flags = Some(app.mode_flags.clone());
    let mut permission_rules = None;
    let mut session_cost = None;
    let mut runtime_accounting = None;
    let mut plan_carry: (Option<String>, Option<String>) = Default::default();
    shell.begin(app, &session, resumed);
    // Draw the home before the first agent is built: that waits for MCP servers to connect.
    let _ = shell.paint(None);
    Shell::warm_session_index(&app.layout);
    // Typing stays live while configured MCP servers finish connecting for the first agent.
    shell.edit_while(crate::tools::integrations::mcp::prefetch_pending);
    // The backend and its open error last reported, so the paused notice repeats only on change.
    let mut memory_reported: Option<(String, Option<String>)> = None;
    // `session_end` is handed from one agent to the next across same-session rebuilds.
    let mut lifecycle_open = false;
    // A local model server survives a same-session rebuild; the new backend reuses it when the
    // changed settings still match what it loaded.
    let mut resident = None;
    let mut coordination = None;
    // Connected MCP servers survive a same-session rebuild; the next agent adopts them when the
    // changed settings leave the servers' configuration alone.
    let mut mcp_handoff = None;
    // Running background jobs and armed watches survive a same-project rebuild instead of being
    // killed with the old agent's tools.
    let mut live_processes = None;
    loop {
        let (step, layout) = {
            let mut agent = app.make_agent(session)?;
            if let Some(carry) = coordination.take() {
                agent.restore_coordination(carry);
            }
            if let Some(processes) = live_processes.take() {
                agent.adopt_live_processes(processes);
            }
            if let Some(server) = resident.take() {
                agent.adopt_resident_server(server);
            }
            if lifecycle_open {
                agent.continue_session_lifecycle();
            } else {
                agent.begin_session_lifecycle();
            }
            if std::mem::take(&mut restore_pending) {
                super::restore::restore_interrupted(app, shell, &mut agent)?;
            }
            // Re-checked after every rebuild: a settings change can switch to a backend that
            // cannot open (a missing role, a half override), which must not go silent.
            if let Some(notice) = memory_notice_due(
                &mut memory_reported,
                app.config.memory.backend.as_str(),
                agent.memory_open_error(),
            ) {
                shell.transcript.notice("Memory", &notice);
            }
            if let Some(snapshot) = &todo {
                agent.restore_todo(snapshot)?;
            }
            agent.set_approval(approval);
            agent.set_session_cost(session_cost);
            if let Some(accounting) = runtime_accounting.take() {
                agent.restore_runtime_accounting(accounting)?;
            }
            agent.set_plan_mode(plan_mode);
            agent.restore_plan_carry(std::mem::take(&mut plan_carry));
            if let Some(flags) = startup_flags.take() {
                for notice in flags.apply_to_agent(&mut agent)? {
                    shell.transcript.notice("Mode", &notice);
                }
                // `--goal` excludes a positional prompt, so the objective is the first prompt.
                if let Some(objective) = &flags.goal {
                    shell.startup = Some(objective.clone());
                }
            }
            if let Some(rules) = permission_rules.take() {
                agent.set_permission_rules(rules);
            }
            let step = shell.run(app, &mut agent);
            if matches!(step, Ok(Step::Rebuild(_))) {
                agent.hand_off_session_lifecycle();
                lifecycle_open = true;
                resident = agent.take_resident_server();
                coordination = agent.take_coordination();
                mcp_handoff = agent.mcp_handoff();
                live_processes = agent.take_live_processes();
            } else {
                agent.end_session_lifecycle();
            }
            agent.cancel_background_compaction();
            runtime_accounting = Some(agent.runtime_accounting());
            session = std::mem::replace(agent.session_mut(), Session::ephemeral());
            todo = Some(agent.todo().snapshot());
            approval = agent.approval();
            plan_mode = agent.plan_mode();
            plan_carry = agent.plan_carry();
            permission_rules = Some(agent.permission_rules().to_vec());
            session_cost = agent.session_cost();
            (step, agent.layout().cloned())
        };
        shell.set_session_id(session.id());
        shell.poll_input_recovery(app);
        match step? {
            Step::Quit | Step::Done => break,
            Step::Rebuild(config) => {
                let project_changed =
                    app.adopt_project(layout.unwrap_or_else(|| app.layout.clone()), *config);
                if project_changed {
                    // A new project root revokes watches and its jobs end with the old agent.
                    live_processes = None;
                    shell.after_project_change(app);
                }
                shell.after_adopt(app);
                // Removing a model/provider layer can leave a valid but unconfigured setup.
                // Select a usable replacement without discarding the held session snapshots.
                if !onboard(app, shell, Some(&session))? {
                    break;
                }
                // Rebuilds reconnect MCP only when its configuration changed: `prefetch` keeps the
                // handed-over servers for an equal configuration and replaces them otherwise. Do
                // not freeze the composer for a reconnect's startup budget.
                if let Some(connecting) = mcp_handoff.take() {
                    crate::tools::integrations::mcp::retain(connecting);
                }
                crate::tools::integrations::mcp::prefetch(&app.config);
                if crate::tools::integrations::mcp::prefetch_pending() {
                    shell.note("Tools", "Reconnecting configured MCP servers. You can keep typing; commands wait until tools are ready.");
                    shell.paint(None)?;
                }
                shell.edit_while(crate::tools::integrations::mcp::prefetch_pending);
            }
        }
    }
    shell.checkpoint_input(true);
    Ok(())
}

/// Esc while a sign-in still runs: the first Esc keeps setup open with a warning; the next gives up on the sign-in.
pub(super) fn sign_in_still_pending(warned: &mut bool) -> bool {
    let first = !*warned;
    *warned = true;
    first
}

/// Only an actual cancellation should leave first-run setup. Navigation and connection tests
/// close their panel too, but must be handled before considering whether any settings changed.
pub(super) enum OnboardingFlow {
    Navigate(Which),
    TestConnection(String),
    VerifySelectedModel,
    VerifyCodingModel,
    Unsupported(String),
    Saved,
    Cancel,
}

pub(super) fn onboarding_flow(outcome: &OverlayOutcome) -> OnboardingFlow {
    if let Some(action) = outcome.next.as_deref() {
        if let Some(destination) = overlay_destination(action) {
            OnboardingFlow::Navigate(destination)
        } else if let Some(id) = action.strip_prefix("test:") {
            OnboardingFlow::TestConnection(id.to_owned())
        } else if action == "command:setup choose verify-model" {
            OnboardingFlow::VerifySelectedModel
        } else if action == "command:setup choose verify-coding" {
            OnboardingFlow::VerifyCodingModel
        } else {
            OnboardingFlow::Unsupported(action.to_owned())
        }
    } else if outcome.latest.is_some() {
        OnboardingFlow::Saved
    } else {
        OnboardingFlow::Cancel
    }
}

/// First-run choices, most likely first; Cancel last. Order matches [`onboarding_destination`].
const ONBOARDING_CHOICES: &[(&str, char)] = &[
    ("Local model", 'l'),
    ("Hosted provider", 'h'),
    ("Quit setup", 'q'),
];

/// Escape or Quit is cancellation, never an implicit model selection.
fn onboarding_destination(choice: Option<usize>) -> Option<Which> {
    match choice {
        Some(0) => Some(Which::Models),
        Some(1) => Some(Which::Providers),
        _ => None,
    }
}

/// What first launch can say about local models before the user picks a path.
#[derive(Debug, Default)]
pub(super) struct LocalHint {
    /// Labels of local inference servers that accept connections right now.
    pub(super) running: Vec<&'static str>,
    /// A suggested MLX coding model that fits this Mac's RAM, offered only when no server runs.
    pub(super) suggestion: Option<&'static str>,
}

impl LocalHint {
    /// A connection check of local servers, then (Apple Silicon only) a RAM-tier suggestion.
    /// Neither loads, downloads or selects a model.
    fn detect() -> Self {
        let running =
            crate::llm::catalog::running_local_servers(std::time::Duration::from_millis(200));
        let suggestion =
            if running.is_empty() && cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                crate::llm::local_fit::physical_memory_bytes()
                    .and_then(crate::cli::models_recommend::first_launch_suggestion)
                    .map(|suggestion| suggestion.model)
            } else {
                None
            };
        Self {
            running,
            suggestion,
        }
    }

    fn points_to_local(&self) -> bool {
        !self.running.is_empty() || self.suggestion.is_some()
    }
}

/// The first-run prompt body. It names local servers that are already running or, when none is,
/// a local model this Mac can hold, so the local path is obvious. This is a hint only: the user
/// still chooses, and nothing is selected or downloaded for them.
pub(super) fn onboarding_prompt(hint: &LocalHint) -> String {
    let base = "Nothing is downloaded or installed until you confirm. Esc cancels.";
    if !hint.running.is_empty() {
        format!(
            "Running on this machine: {}. Local model uses it with no API key.\n{base}",
            hint.running.join(", ")
        )
    } else if let Some(model) = hint.suggestion {
        format!(
            "This Mac has enough memory for {model}: Local model → Get models downloads it (only after you confirm). Hosted provider signs in or takes an API key.\n{base}"
        )
    } else {
        format!(
            "Local model uses Ollama, LM Studio or downloaded weights; Hosted provider signs in or takes an API key.\n{base}"
        )
    }
}

/// The highlighted first-run choice: "Local model" when there is a local path to point at, else
/// "Hosted provider". Either only opens a panel; Enter still confirms and Esc still cancels.
pub(super) fn onboarding_default(hint: &LocalHint) -> usize {
    if hint.points_to_local() { 0 } else { 1 }
}

/// A first run that never chose an approval mode starts in `write` ("Edit freely"): edits and
/// non-destructive commands run, destructive or out-of-project actions ask. It is saved once as
/// the user's own choice, so headless runs, protocols, benchmarks and existing users (who already
/// run with the `auto` default) keep their contracts. Returns the notice to show, if any.
pub(super) fn adopt_first_run_approval(app: &mut App) -> Option<String> {
    if app.config.setting_sources.get("approval") != Some(&"default") {
        return None;
    }
    let change = crate::config::settings_store::Change::Insert {
        path: "approval".into(),
        value: "write".into(),
    };
    match crate::config::settings_store::save(
        &app.layout,
        crate::config::settings_store::Scope::Global,
        &[change],
    ) {
        Ok(()) => {
            app.config.approval = crate::config::types::Approval::Write;
            app.config
                .setting_sources
                .insert("approval".into(), "global");
            Some("Edit freely: edits and safe commands run without asking; destructive or out-of-project actions ask. Shift+Tab cycles ask, write and read-only; /approval auto removes all prompts.".into())
        }
        Err(error) => Some(format!(
            "Could not save the starting approval mode ({error:#}); this session keeps the configured default."
        )),
    }
}

/// First run (or a provider that cannot be used): let the user pick a model before any agent exists.
fn onboard(app: &mut App, shell: &mut Shell, session: Option<&Session>) -> Result<bool> {
    let mut auth_warned = false;
    let mut destination = None;
    let mut local: Option<LocalHint> = None;
    loop {
        let backend = match session {
            Some(session) => crate::agent::make_session_backend(&app.config, session),
            None => make_backend(&app.config),
        };
        if backend.is_ok() && destination.is_none() {
            // Only a real first run (the welcome prompt was shown) chooses a starting mode.
            if local.is_some()
                && let Some(notice) = adopt_first_run_approval(app)
            {
                shell.note("Approval", notice);
            }
            return Ok(true);
        }
        if !app.config.model.trim().is_empty() {
            let message = backend
                .as_ref()
                .err()
                .map(crate::app::shell::model_verification::failure_hint)
                .unwrap_or(
                    "Model configuration is ready; response verification is separate in /doctor.",
                );
            shell.note("Model setup", message);
        }
        let which = if let Some(which) = destination.take() {
            which
        } else {
            let local = local.get_or_insert_with(LocalHint::detect);
            match onboarding_destination(shell.ask_selected(
                "Welcome to dial · where should the model run?",
                &onboarding_prompt(local),
                ONBOARDING_CHOICES,
                onboarding_default(local),
            )) {
                Some(which) => which,
                None => {
                    if let Some(job) = shell.auth_job.take() {
                        job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    return Ok(false);
                }
            }
        };
        let outcome = shell.run_overlay(app, None, which)?;
        let flow = onboarding_flow(&outcome);
        if let Some(config) = outcome.latest {
            app.adopt(config);
            shell.after_adopt(app);
        }
        match flow {
            OnboardingFlow::Navigate(which) => {
                destination = Some(which);
            }
            OnboardingFlow::TestConnection(id) => {
                shell.catalog.refresh(app);
                shell.note(
                    "Metadata connection check",
                    format!("Checking {id}. Cached endpoint metadata appears in Providers; this does not verify a model response."),
                );
                destination = Some(Which::Providers);
            }
            OnboardingFlow::VerifySelectedModel => {
                shell.verify_selected_model(app, None)?;
                destination = Some(Which::Providers);
            }
            OnboardingFlow::VerifyCodingModel => {
                shell.verify_coding_model(app, None)?;
                destination = Some(Which::Providers);
            }
            OnboardingFlow::Unsupported(action) => {
                shell.note("Setup", format!("Select a model before using {action}."));
            }
            OnboardingFlow::Saved => {}
            OnboardingFlow::Cancel => {
                if shell.auth_job.is_some() {
                    if sign_in_still_pending(&mut auth_warned) {
                        let label = shell.auth_job.as_ref().map_or("Sign-in", |job| job.label);
                        shell.note(label, format!("{label} is still waiting for your browser. Esc backs out to model-path choice; Cancel there quits and cancels sign-in. Select a model after sign-in completes."));
                        continue;
                    }
                    if let Some(job) = shell.auth_job.take() {
                        job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                // Escape backs out of setup; choosing Cancel at the path picker exits.
                continue;
            }
        }
    }
}

#[cfg(test)]
mod tests;
