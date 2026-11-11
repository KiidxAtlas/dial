//! Interactive controller: owns the terminal, the conversation view, the composer and the overlays, and
//! runs agent turns. Rendering is inline (scrollback-native, see `ui::term`): finished messages are
//! committed to the terminal's own scrollback, and only the live tail and the composer are repainted.
//!
//! Configuration is never edited here. Overlays emit semantic events (`OverlayEvent`); this module
//! persists them through `settings_store` (which validates the merged configuration before writing),
//! reloads the effective config, and applies the result at a safe boundary. External edits retain
//! last-good layers on failure; changes found during a turn are staged. Adoption rebuilds the
//! immutable agent configuration around the same session, task list, permissions and cost.

mod commands;

use crate::agent::{TurnOutcome, TurnReport, Ui};
use crate::app::bridge::Bridge;
use crate::app::core::App;
use crate::app::display::short;
use crate::app::drive::{Step, drive};
use crate::app::overlay::auth::AuthJob;
use crate::app::overlay::{Which, start_streaming_job};
use crate::app::selection::{reasoning_changes, reasoning_profile, thinking_label};
use crate::app::shell::{PAINT_EVERY, Shell};
use crate::cli::print_output;
use crate::config::paths::Layout;
use crate::config::types::{Approval, Config};
use crate::learn::auto;
use crate::repl::{OutputFormat, PrintExit, PrintUi};
use crate::storage::session::{Session, SessionMeta};
use crate::ui::status::Phase;
use anyhow::Result;
use std::time::{Duration, Instant};

mod bridge;
mod bridge_ui;
mod clipboard;
mod command_registry;
pub(crate) use command_registry::builtin_commands;
mod completions;
mod core;
mod discovery;
mod display;
mod drive;
mod git;
mod improve_view;
mod learning;
mod notices;
mod overlay;
mod panel;
mod print_attachments;
mod replay;
mod resources;
mod restore;
mod selection;
mod shell;
mod side;

pub struct Options {
    pub session: Option<Session>,
    pub prompt: String,
    pub interactive: bool,
    /// `--restore`: rebind the resumed session's interrupted workers before the user takes control.
    pub restore: bool,
    /// Command-line flag overrides, layered on top of the config files every time the config is reloaded.
    pub overlay: toml::Table,
    /// Explicit launch notices for the transcript.
    pub notices: Vec<String>,
    /// `--print-thoughts`: print reasoning to stdout in print text mode.
    pub print_thoughts: bool,
    /// CLI launch overrides (`--tools`, `--skills`, system prompt, `--max-time`) applied to every agent in this process.
    pub launch: crate::agent::modes::Launch,
    /// `--add-dir`: extra workspace roots added to every agent in this process.
    pub add_dirs: Vec<std::path::PathBuf>,
    /// `--image`: explicit image files attached to the print turn; empty for text-only turns.
    pub images: Vec<std::path::PathBuf>,
    /// `--image -`: image bytes read from stdin for the print turn; the format is detected from magic bytes.
    pub stdin_image: Option<Vec<u8>>,
    /// `--plan`, `--prewalk`, and the other mode flags; `drive` applies the startup arm to the first agent.
    pub mode_flags: crate::cli::mode_flags::ModeFlags,
    /// `--output-schema`: the parsed schema the final print answer must satisfy (already checked).
    pub output_schema: Option<serde_json::Value>,
    /// `-o/--output-last-message`: file that receives the final answer; untouched when there is none.
    pub output_last_message: Option<std::path::PathBuf>,
    /// `dial review`: the frozen target and guidance to review read-only, instead of a prompt turn.
    pub review: Option<crate::cli::review::Run>,
}

/// Run the inline interface, or answer a single piped message.
pub fn run_interactive(layout: Layout, config: Config, options: Options) -> Result<()> {
    if !options.interactive {
        return run_plain(layout, config, options);
    }
    let mut app = App::new(
        layout,
        config,
        options.overlay,
        options.launch,
        options.add_dirs,
        options.mode_flags,
    )?;
    // Servers connect while the terminal and home screen are set up; the first agent adopts them.
    crate::tools::integrations::mcp::prefetch(&app.config);
    #[cfg(unix)]
    let startup_prompt = options.prompt.clone();
    let mut shell = Shell::new(&app, options.prompt)?;
    for notice in &options.notices {
        shell.transcript.notice("Learning", notice);
    }
    let outcome = drive(&mut app, &mut shell, options.session, options.restore);
    crate::app::commands::maintenance::stop_relay();
    let resume = shell.finish();
    crate::stats::sync_after_session(&app.config);
    #[cfg(unix)]
    if shell.restart_requested && outcome.is_ok() {
        let launch: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        let launch = crate::agent::modes::relaunch::without_prompt(&launch, &startup_prompt);
        let args = crate::agent::modes::relaunch::relaunch_args(&launch, resume.as_deref());
        crate::telemetry::shutdown();
        return Err(crate::agent::modes::relaunch::exec(&args).into());
    }
    if let Some(id) = resume {
        eprintln!("dial: session {} · continue with `dial -c`", short(&id));
    }
    outcome
}

/// Piped text uses the same status contract as explicit print mode.
fn run_plain(layout: Layout, config: Config, options: Options) -> Result<()> {
    let code = run_print(layout, config, options, OutputFormat::Text, Vec::new())?;
    if code != 0 {
        return Err(PrintExit(code).into());
    }
    Ok(())
}

/// How long `dial -p` waits, after its turn, for one selected-memory maintenance pass.
const PRINT_MEMORY_BUDGET: Duration = Duration::from_secs(20);

/// Run one turn without terminal control sequences; JSON modes reserve stdout for their protocol.
/// Print mode has no human input channel (`PrintUi::ask_questions` always answers Unsupported),
/// so offering `ask` only adds its schema to every request and invites a wasted call.
fn print_config(mut config: Config) -> Config {
    config.ask.enabled = false;
    config
}

pub fn run_print(
    layout: Layout,
    config: Config,
    options: Options,
    format: OutputFormat,
    allowed: Vec<String>,
) -> Result<i32> {
    let started = Instant::now();
    let mut config = print_config(config);
    if options.review.is_some() {
        // `dial review` never edits: the approval policy is read-only for the whole run.
        config.approval = Approval::Readonly;
    }
    let mut ui = PrintUi::new(format, allowed);
    ui.set_print_thoughts(options.print_thoughts);
    ui.set_structured_mode(options.output_schema.is_some());
    config.require_model()?;
    // Connect MCP servers while the session, backend and agent are set up; the registry adopts
    // the result instead of waiting for each server on the first request.
    crate::tools::integrations::mcp::prefetch(&config);
    let _compute = if config.requires_compute_lock() {
        Some(auto::compute_lock(&layout)?)
    } else {
        None
    };
    let _owner = if _compute.is_some() {
        Some(crate::learn::compute::owner_record(
            &layout.user_dir,
            "session",
            &layout.project_root,
            &config.state_dir,
            &config.model,
        )?)
    } else {
        None
    };
    let mut launch = options.launch;
    launch.unattended = true;
    let app = App::new(
        layout,
        config,
        options.overlay,
        launch,
        options.add_dirs,
        options.mode_flags,
    )?;
    let image_count = options.images.len() + usize::from(options.stdin_image.is_some());
    if image_count > 0 {
        let backend = crate::agent::make_backend(&app.config)?;
        print_attachments::ensure_image_input(backend.accepts_image_input(), image_count)?;
    }
    // Print runs schedule no detached improvement workers: a one-shot command must
    // not leave processes behind that compete for local compute after it exits.
    let session = match options.session {
        Some(session) => session,
        None => Session::create(
            &app.layout,
            SessionMeta {
                model: app.config.model.clone(),
            },
        )?,
    };
    let session_id = session.id().to_owned();
    let mut agent = app.make_agent(session)?;
    agent.begin_session_lifecycle();
    ui.set_interrupt(app.interrupt.clone());
    ui.system(
        &session_id,
        &app.config.model,
        &app.layout.project_root,
        &agent.tool_names(),
    );
    for notice in &options.notices {
        ui.notice(notice);
    }
    ui.check_output()?;
    let completed_before = agent.metrics().turns_completed;
    let result = if let Some(review) = &options.review {
        agent
            .review_target(&review.target, &review.guidance, &mut ui)
            .map(|text| TurnReport {
                text,
                outcome: TurnOutcome::Completed,
                steps: 0,
                interrupted: false,
                trajectory: None,
            })
    } else if image_count == 0 {
        agent.turn(options.prompt.trim(), &mut ui)
    } else {
        match print_attachments::prepare_turn(
            &app.layout.project_root,
            options.prompt.trim(),
            &options.images,
            options.stdin_image.as_deref(),
            &app.interrupt,
        ) {
            Ok(turn) => {
                for warning in &turn.warnings {
                    ui.notice(warning);
                }
                agent.turn_prepared_preserving_interrupt(&turn.text, turn.message, &mut ui)
            }
            Err(error) => Err(error),
        }
    };
    let result = match (result, options.output_schema.as_ref()) {
        (Ok(report), Some(schema)) => {
            let retries = app.config.task.max_schema_retries;
            match print_output::enforce(report, schema, retries, |message| {
                agent.turn(message, &mut ui)
            }) {
                Ok(print_output::Checked::Valid(report, value)) => {
                    ui.set_structured(value);
                    Ok(report)
                }
                Ok(print_output::Checked::Rejected(report, why)) => {
                    eprintln!("error: the final answer does not match --output-schema: {why}");
                    Ok(report)
                }
                // A failed correction turn still reaches `ui.finish`, so the result event is emitted.
                Err(error) => Err(error),
            }
        }
        (result, _) => result,
    };
    if let (Ok(report), Some(path)) = (&result, &options.output_last_message) {
        print_output::write_last_message(path, &report.text)?;
    }
    let completed = if options.review.is_some() {
        result.is_ok()
    } else {
        agent.metrics().turns_completed > completed_before
    };
    ui.set_completed(completed);
    // A one-shot run has no idle loop: give the selected memory one bounded pass to retain this
    // turn before exit. Success prints nothing.
    if completed && result.is_ok() && options.review.is_none() {
        agent.finish_memory_before_exit(&mut ui, PRINT_MEMORY_BUDGET);
    }
    agent.end_local_session();
    let (total, turn) = agent.usage();
    ui.set_usage_breakdown(total, turn);
    match result {
        Ok(report) => ui.finish(Some(&report), None, Some(&session_id), started.elapsed()),
        Err(error) => {
            // The recovery guidance in the chain is addressed to the model, not to a person.
            let error = crate::agent::user_failure_text(&error);
            eprintln!("error: {error}");
            ui.finish(None, Some(&error), Some(&session_id), started.elapsed())
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod retry_notice_tests;

#[cfg(test)]
mod group_digits_tests;

#[cfg(test)]
mod command_registry_tests;
