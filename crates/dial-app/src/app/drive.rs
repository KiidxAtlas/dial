//! The startup and turn driver: runs the first-run onboarding, then the shell loop until exit.

use crate::agent::make_backend;
use crate::app::core::App;
use crate::app::notices::memory_notice_due;
use crate::app::overlay::{OverlayOutcome, Which, overlay_destination};
use crate::app::shell::Shell;
use crate::config::types::Config;
use crate::storage::session::{Session, SessionMeta};
use crate::ui::keys::Action;
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

pub(super) fn drive(app: &mut App, shell: &mut Shell, session: Option<Session>) -> Result<()> {
    if !onboard(app, shell, session.as_ref())? {
        return Ok(());
    }
    let resumed = session.is_some();
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
    loop {
        let step = {
            let mut agent = app.make_agent(session)?;
            if let Some(server) = resident.take() {
                agent.adopt_resident_server(server);
            }
            if lifecycle_open {
                agent.continue_session_lifecycle();
            } else {
                agent.begin_session_lifecycle();
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
            step
        };
        shell.session_id = session.id().to_owned();
        match step? {
            Step::Quit | Step::Done => break,
            Step::Rebuild(config) => {
                app.adopt(*config);
                shell.after_adopt(app);
                // Removing a model/provider layer can leave a valid but unconfigured setup.
                // Select a usable replacement without discarding the held session snapshots.
                if !onboard(app, shell, Some(&session))? {
                    break;
                }
            }
        }
    }
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
        } else {
            OnboardingFlow::Unsupported(action.to_owned())
        }
    } else if outcome.latest.is_some() {
        OnboardingFlow::Saved
    } else {
        OnboardingFlow::Cancel
    }
}

/// First run (or a provider that cannot be used): let the user pick a model before any agent exists.
fn onboard(app: &mut App, shell: &mut Shell, session: Option<&Session>) -> Result<bool> {
    // First run starts on provider setup (sign in or add a key for a hosted model); Esc there
    // moves to the discovered local model list once before quitting.
    let mut providers_first = app.config.model.trim().is_empty();
    let mut auth_warned = false;
    let mut destination = None;
    loop {
        let ready = !app.config.model.trim().is_empty()
            && match session {
                Some(session) => crate::agent::make_session_backend(&app.config, session).is_ok(),
                None => make_backend(&app.config).is_ok(),
            };
        if ready && destination.is_none() {
            return Ok(true);
        }
        shell.note("Welcome to dial", if app.config.model.trim().is_empty() {
            "Sign in to a provider or add an API key to start with a hosted model. Esc shows local models instead: running LM Studio or Ollama servers (and MLX models on Apple Silicon) are found automatically."
        } else {
            "The selected provider cannot be used yet (missing credentials?). Set it up in Providers."
        });
        let which = destination.take().unwrap_or(if providers_first {
            Which::Providers
        } else {
            Which::Models
        });
        let secondary_panel = !providers_first && !matches!(which, Which::Models);
        let outcome = shell.run_overlay(app, None, which)?;
        let flow = onboarding_flow(&outcome);
        if let Some(config) = outcome.latest {
            app.adopt(config);
            shell.after_adopt(app);
        }
        match flow {
            OnboardingFlow::Navigate(which) => {
                // Once the user explicitly navigates, Esc returns to models rather than
                // replaying the initial provider prompt or quitting a secondary panel.
                providers_first = false;
                destination = Some(which);
            }
            OnboardingFlow::TestConnection(id) => {
                shell.catalog.refresh(app);
                shell.note(
                    "Connection test",
                    format!("Checking {id}. Its status appears in the provider list when the check finishes."),
                );
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
                        shell.note(label, format!("{label} is still waiting for your browser. Esc again quits and cancels it; select a model after sign-in completes."));
                        continue;
                    }
                    if let Some(job) = shell.auth_job.take() {
                        job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                if secondary_panel {
                    destination = Some(Which::Models);
                    continue;
                }
                if providers_first {
                    providers_first = false;
                    shell.note(
                        "No model yet",
                        "Pick a discovered local model here (LM Studio, Ollama, MLX). Esc again quits.",
                    );
                    continue;
                }
                // `dial setup -m` installs the MLX runtime, which exists only on Apple Silicon Macs.
                let local = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                    ", or download a local model with `dial setup -m <mlx-model>`"
                } else {
                    ""
                };
                shell.note(
                    "No model selected",
                    format!(
                        "Run dial again to pick a model or sign in to a provider{local}. Inside dial, {} opens the model list and /providers sets up sign-in and API keys.",
                        shell.keys.label(Action::ModelSelect)
                    ),
                );
                shell.flush()?;
                return Ok(false);
            }
        }
    }
}
