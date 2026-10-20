//! `/setup` and `/doctor`: list optional capabilities, preview one set-up, and run it.
//!
//! `/setup <id>` shows the exact commands, the install destination and any prerequisites.
//! The interactive picker reviews the same plan and asks for explicit confirmation. Direct
//! `/setup <id> yes` runs a background job; settings are saved only after successful setup.

use super::extensions::spawn_job_streaming;
use crate::agent::Agent;
use crate::app::{App, Shell, Step, Which};
use crate::config::Config;
use crate::config::memory_backend::MemoryBackendId;
use crate::config::settings_store::{Change, Scope};
use crate::setup::deps::{self, Env, plan, recipe, run};
use crate::setup::embeddings;
use crate::setup::memory::{
    BACKENDS, backend_label, backend_missing, parse_backend, selected, status_summary,
};
use crate::ui::overlay::palette::{ChoiceItem, ChoiceSpec};
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const TITLE: &str = "Setup";

const USAGE: &str = "/setup                 guided setup: search, review, then confirm\n/setup list            list capabilities and their status as text\n/setup <id>            preview what would run and where it installs\n/setup <id> yes        set it up (prerequisites first), then save its settings\n/setup providers       connect a model provider\n/setup memory          choose a memory backend: what each needs and whether it is ready\n/setup memory <backend>   switch memory to off, provenance, local, hindsight, sharpshooter or semantic (all projects)\n/setup embeddings      list local embedding models (Ollama)\n/setup embeddings:<model> [yes]   preview, then pull the model and assign it to the embedding role (used by the semantic memory backend)";

pub(super) const COMMANDS: &[super::CommandSpec] = &[super::CommandSpec {
    name: "setup",
    description: "check optional capabilities, set one up, choose memory, or sign in to providers",
    args_hint: "[<id> [yes]]",
    subcommands: &["providers", "memory", "embeddings", "list"],
}];

pub(super) fn run_command(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    if name != "setup" && name != "doctor" {
        return None;
    }
    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.first() == Some(&"choose") {
        return Some(guided_command(shell, app, agent, &parts));
    }
    if parts.first() == Some(&"memory") {
        return Some(memory_command(shell, app, agent, &parts[1..]));
    }
    match parts.as_slice() {
        [] => {
            let env = Env::current(&app.layout.user_dir);
            Some(shell.open(app, agent, Which::Choice(Box::new(setup_choices(&env)))))
        }
        ["providers"] => Some(shell.open(app, agent, Which::Providers)),
        // Everything else was answered by `run_live`, which `commands::run` tries first.
        _ => None,
    }
}

/// Listings, previews, help and the background set-up jobs. They save settings to disk but never
/// rebuild the agent, so they run at once even during a turn. The picker, `providers`, `choose`
/// and `memory` open overlays or switch the live agent and stay with [`run_command`].
pub(super) fn run_live(
    shell: &mut Shell,
    app: &App,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    if name != "setup" && name != "doctor" {
        return None;
    }
    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.first().is_some_and(|id| id.starts_with("embeddings")) {
        embedding_command(shell, app, &parts);
        return Some(Ok(Step::Done));
    }
    let env = Env::current(&app.layout.user_dir);
    match parts.as_slice() {
        [] | ["providers"] | ["choose" | "memory", ..] => return None,
        ["list"] => {
            let config = app.reload().unwrap_or_else(|_| app.config.clone());
            shell.note(
                TITLE,
                format!(
                    "{}\n\n{}",
                    run::list_text(&env),
                    crate::setup::memory::doctor_text(&config)
                ),
            )
        }
        ["help"] => shell.note(TITLE, USAGE),
        [id] => shell.note(TITLE, run::preview_text(id, &env)),
        [id, "yes"] => start(shell, app, id),
        _ => shell.note(TITLE, USAGE),
    }
    Some(Ok(Step::Done))
}

/// The picker emits only review/navigation commands, never the explicit `yes` form.
fn setup_choices(env: &Env) -> ChoiceSpec {
    let mut items = vec![
        ChoiceItem {
            value: "providers".into(),
            label: "Connect a model provider".into(),
            detail: "Sign in or configure an API key or local endpoint; opens Providers".into(),
            current: false,
        },
        ChoiceItem {
            value: "memory".into(),
            label: "Choose a memory backend".into(),
            detail: "See what each backend needs and whether it is ready, then switch".into(),
            current: false,
        },
        ChoiceItem {
            value: "embeddings".into(),
            label: "Set up semantic memory embeddings".into(),
            detail: "Choose a local Ollama model; review download size before confirming".into(),
            current: false,
        },
    ];
    items.extend(deps::report(env).into_iter().map(|row| ChoiceItem {
        value: row.id.to_owned(),
        label: format!("{} ({})", row.title, row.id),
        detail: format!(
            "{} · {} · Review setup plan",
            if row.status == deps::Status::Ready {
                "Installed"
            } else {
                "Not installed"
            },
            row.detail
        ),
        current: false,
    }));
    choice_spec("Setup · choose what you need", items)
}

fn embedding_choices() -> ChoiceSpec {
    choice_spec(
        "Setup · choose an embedding model",
        embeddings::CATALOG
            .iter()
            .map(|model| ChoiceItem {
                value: embeddings::setup_id(model.name),
                label: model.name.to_owned(),
                detail: format!(
                    "{} MB · {} dimensions · {}",
                    model.size_mb, model.dims, model.description
                ),
                current: false,
            })
            .collect(),
    )
}

fn choice_spec(title: &str, items: Vec<ChoiceItem>) -> ChoiceSpec {
    ChoiceSpec {
        title: title.to_owned(),
        command: "setup".into(),
        verb: "choose",
        remove: None,
        items,
    }
}

/// Validate before offering installation. Errors retain the existing preview/manual guidance.
/// This reads local state only: no probes, network requests, downloads or settings writes.
fn review_plan(id: &str, env: &Env) -> std::result::Result<String, String> {
    if id.starts_with("embeddings") {
        let model = embeddings::model_from_id(id)
            .ok_or_else(|| format!("Invalid embedding setup id `{id}`. Use /setup embeddings."))?;
        let preview = embeddings::preview_text(model, &env.user_dir);
        let ollama = recipe("ollama").ok_or_else(|| "Ollama setup is unavailable.".to_owned())?;
        if !deps::is_ready(ollama, env) && plan::plan(ollama, env).is_err() {
            return Err(preview);
        }
        return Ok(preview);
    }
    let preview = run::preview_text(id, env);
    let Some(target) = recipe(id) else {
        return Err(preview);
    };
    if deps::is_ready(target, env) || plan::plan(target, env).is_err() {
        return Err(preview);
    }
    Ok(preview)
}

fn guided_command(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    parts: &[&str],
) -> Result<Step> {
    let ["choose", id] = parts else {
        shell.note(TITLE, USAGE);
        return Ok(Step::Done);
    };
    match *id {
        "providers" => return shell.open(app, agent, Which::Providers),
        "embeddings" => {
            return shell.open(app, agent, Which::Choice(Box::new(embedding_choices())));
        }
        "memory" => return memory_command(shell, app, agent, &[]),
        _ => {}
    }
    if let Some(backend) = id.strip_prefix("memory:") {
        return set_memory_backend(shell, app, agent, backend);
    }
    let env = Env::current(&app.layout.user_dir);
    let preview = match review_plan(id, &env) {
        Ok(preview) => preview,
        Err(message) => {
            shell.note(TITLE, message);
            return Ok(Step::Done);
        }
    };
    // Keep the full plan in the transcript as well as in the scrollable confirmation.
    shell.note(TITLE, &preview);
    if shell.ask(
        "Review setup plan",
        &preview,
        &[
            ("Cancel — change nothing", 'c'),
            ("Install and save settings", 'i'),
        ],
    ) != Some(1)
    {
        shell.note(TITLE, "Setup cancelled. Nothing was installed or changed.");
        return Ok(Step::Done);
    }
    if embeddings::model_from_id(id).is_some() {
        embedding_command(shell, app, &[id, "yes"]);
    } else {
        start(shell, app, id);
    }
    Ok(Step::Done)
}

fn start(shell: &mut Shell, app: &App, id: &str) {
    let env = Env::current(&app.layout.user_dir);
    let Some(target) = recipe(id) else {
        shell.note(TITLE, format!("Unknown capability `{id}`."));
        return;
    };
    if let Err(blocked) = plan::plan(target, &env) {
        shell.note(TITLE, blocked.to_string());
        return;
    }
    let layout = app.layout.clone();
    let id = id.to_owned();
    spawn_job_streaming(shell, "Setup", move |cancel: Arc<AtomicBool>, progress| {
        let done = run::install_and_save(&layout, &id, &cancel, progress)?;
        Ok(format!("Set up and saved:\n{}", done.join("\n")))
    });
}

/// `/setup embeddings` lists the catalog; `embeddings:<model>` previews, and `... yes` pulls it
/// through Ollama in the background and assigns it to the embedding role.
fn embedding_command(shell: &mut Shell, app: &App, parts: &[&str]) {
    let Some(model) = parts.first().and_then(|id| embeddings::model_from_id(id)) else {
        shell.note(TITLE, USAGE);
        return;
    };
    match parts {
        [id] if *id == "embeddings" => shell.note(TITLE, embeddings::catalog_text()),
        [_] => shell.note(TITLE, embeddings::preview_text(model, &app.layout.user_dir)),
        [_, "yes"] => {
            let layout = app.layout.clone();
            let model = model.to_owned();
            spawn_job_streaming(shell, "Setup", move |cancel: Arc<AtomicBool>, progress| {
                embeddings::setup(&layout, &model, &cancel, progress)
            });
        }
        _ => shell.note(TITLE, USAGE),
    }
}

/// The `/setup memory` picker: one row per backend with what it needs and whether it is ready.
fn memory_choices(config: &Config) -> ChoiceSpec {
    let current = selected(config);
    choice_spec(
        "Setup · choose a memory backend (saved for all projects)",
        BACKENDS
            .iter()
            .map(|(id, label, needs)| {
                let (blocking, optional) = backend_missing(config, *id);
                let status = status_summary(&blocking, &optional);
                ChoiceItem {
                    value: format!("memory:{}", id.as_str()),
                    label: if current == Some(*id) {
                        format!("{label} (current)")
                    } else {
                        (*label).to_owned()
                    },
                    detail: format!("{needs} · {status}"),
                    current: current == Some(*id),
                }
            })
            .collect(),
    )
}

/// `/setup memory [<backend>|offer-semantic]`.
fn memory_command(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    args: &[&str],
) -> Result<Step> {
    match args {
        [] => {
            let config = app.reload().unwrap_or_else(|_| app.config.clone());
            shell.open(app, agent, Which::Choice(Box::new(memory_choices(&config))))
        }
        [offer] if *offer == embeddings::OFFER_SEMANTIC_ARG => offer_semantic(shell, app, agent),
        [backend] => set_memory_backend(shell, app, agent, backend),
        _ => {
            shell.note(TITLE, USAGE);
            Ok(Step::Done)
        }
    }
}

/// Asks once, with a single key, whether to switch memory to semantic now that an embedding model
/// is set up or assigned. Declining changes nothing.
fn offer_semantic(shell: &mut Shell, app: &App, agent: &mut Agent<'_>) -> Result<Step> {
    let config = app.reload()?;
    if config.memory.enabled && config.memory.backend == MemoryBackendId::Semantic {
        return Ok(Step::Done);
    }
    let current = if config.memory.enabled {
        backend_label(config.memory.backend)
    } else {
        "Off"
    };
    let body = format!(
        "The embedding model is used only by the semantic memory backend; memory is currently {current}.\nSwitching selects semantic memory for all projects. Facts kept by {current} stay on disk but are no longer recalled."
    );
    let keep = format!("Keep {current}");
    if shell.ask(
        "Use semantic memory?",
        &body,
        &[("Switch to semantic memory", 's'), (keep.as_str(), 'k')],
    ) == Some(0)
    {
        return set_memory_backend(shell, app, agent, MemoryBackendId::Semantic.as_str());
    }
    shell.note(
        TITLE,
        format!(
            "Kept {current}. Switch later with /setup memory semantic or in /settings → Memory."
        ),
    );
    Ok(Step::Done)
}

/// Saves `memory.backend` for all projects through the settings save path, then reports what the
/// backend still needs. Hindsight without its URL is not saved: validation would reject it.
fn set_memory_backend(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    name: &str,
) -> Result<Step> {
    let Some(backend) = parse_backend(name) else {
        shell.note(
            TITLE,
            format!("Unknown memory backend `{name}`. Use /setup memory to choose one."),
        );
        return Ok(Step::Done);
    };
    let config = app.reload()?;
    if backend == MemoryBackendId::Hindsight && config.memory.hindsight.endpoint.trim().is_empty() {
        shell.note(
            TITLE,
            "Hindsight needs its Service URL first: open /settings → Memory, choose Hindsight service, and fill the fields it asks for.",
        );
        return Ok(Step::Done);
    }
    let saved = app
        .save_settings(
            Scope::Global,
            &[Change::Set {
                path: "memory.backend".into(),
                value: toml::Value::String(backend.as_str().into()),
            }],
            None,
        )
        .and_then(|()| app.reload());
    let fresh = match saved {
        Ok(fresh) => fresh,
        Err(error) => {
            shell.note(TITLE, format!("Memory backend not changed: {error:#}"));
            return Ok(Step::Done);
        }
    };
    shell.note(TITLE, switch_report(&fresh, backend));
    Ok(shell.config_changed(app, agent, fresh))
}

/// What a switch did and what is still missing, from the config after the save.
fn switch_report(fresh: &Config, backend: MemoryBackendId) -> String {
    let mut lines = vec![format!(
        "Memory backend: {} (saved for all projects).",
        backend_label(backend)
    )];
    if !fresh.memory.enabled || fresh.memory.backend != backend {
        lines.push(format!(
            "This project's settings still select {}; change it in /settings → Memory with \"This project\" scope.",
            if fresh.memory.enabled {
                backend_label(fresh.memory.backend)
            } else {
                "Off"
            }
        ));
    }
    let (blocking, optional) = backend_missing(fresh, backend);
    if blocking.is_empty() && optional.is_empty() {
        lines.push("Ready: nothing else to set up.".to_owned());
    }
    lines.extend(
        blocking
            .into_iter()
            .map(|line| format!("Still needed: {line}")),
    );
    lines.extend(optional.into_iter().map(|line| format!("Optional: {line}")));
    lines.join("\n")
}

#[cfg(test)]
mod tests;
