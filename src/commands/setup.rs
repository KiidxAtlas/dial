//! `/setup` and `/doctor`: list optional capabilities, preview one set-up, and run it.
//!
//! `/setup <id>` shows the exact commands, the install destination and any prerequisites.
//! The interactive picker reviews the same plan and asks for explicit confirmation. Direct
//! `/setup <id> yes` runs a background job; settings are saved only after successful setup.

use super::extensions::spawn_job_streaming;
use crate::agent::Agent;
use crate::app::{App, Shell, Step, Which};
use crate::setup::deps::{self, Env, plan, recipe, run};
use crate::setup::embeddings;
use crate::ui::overlay::palette::{ChoiceItem, ChoiceSpec};
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const TITLE: &str = "Setup";

const USAGE: &str = "/setup                 guided setup: search, review, then confirm\n/setup list            list capabilities and their status as text\n/setup <id>            preview what would run and where it installs\n/setup <id> yes        set it up (prerequisites first), then save its settings\n/setup providers       connect a model provider\n/setup embeddings      list local embedding models (Ollama)\n/setup embeddings:<model> [yes]   preview, then pull the model and assign it to semantic memory";

pub(super) const COMMANDS: &[super::CommandSpec] = &[super::CommandSpec {
    name: "setup",
    description: "check optional capabilities, set one up, or sign in to providers",
    args_hint: "[<id> [yes]]",
    subcommands: &["providers", "embeddings", "list"],
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
    if parts.first().is_some_and(|id| id.starts_with("embeddings")) {
        embedding_command(shell, app, &parts);
        return Some(Ok(Step::Done));
    }
    let env = Env::current(&app.layout.user_dir);
    match parts.as_slice() {
        [] => {
            return Some(shell.open(app, agent, Which::Choice(Box::new(setup_choices(&env)))));
        }
        ["list"] => shell.note(TITLE, run::list_text(&env)),
        ["help"] => shell.note(TITLE, USAGE),
        ["providers"] => return Some(shell.open(app, agent, Which::Providers)),
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
        _ => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_env(root: &std::path::Path) -> Env {
        Env {
            path: Vec::new(),
            user_dir: root.join("user"),
        }
    }

    #[test]
    fn choices_only_dispatch_review_and_cover_every_capability() {
        let temp = tempfile::tempdir().unwrap();
        let spec = setup_choices(&empty_env(temp.path()));
        assert_eq!(spec.command, "setup");
        assert_eq!(spec.verb, "choose");
        assert!(spec.remove.is_none());
        assert_eq!(spec.items.len(), deps::RECIPES.len() + 2);
        for id in ["providers", "embeddings"]
            .into_iter()
            .chain(deps::RECIPES.iter().map(|r| r.id))
        {
            assert_eq!(spec.items.iter().filter(|item| item.value == id).count(), 1);
        }
        assert!(
            spec.items
                .iter()
                .all(|item| !item.value.contains(char::is_whitespace))
        );
        assert!(!temp.path().join("user").exists());
    }

    #[test]
    fn embedding_choices_use_catalog_and_never_install_directly() {
        let spec = embedding_choices();
        assert_eq!(spec.verb, "choose");
        assert!(spec.remove.is_none());
        assert_eq!(spec.items.len(), embeddings::CATALOG.len());
        for (item, model) in spec.items.iter().zip(embeddings::CATALOG) {
            assert_eq!(embeddings::model_from_id(&item.value), Some(model.name));
            assert!(item.detail.contains(&format!("{} MB", model.size_mb)));
            assert!(item.detail.contains(model.description));
        }
    }

    #[test]
    fn review_rejects_unknown_malformed_and_manual_targets_without_writes() {
        let temp = tempfile::tempdir().unwrap();
        let env = empty_env(temp.path());
        assert!(
            review_plan("unknown-capability", &env)
                .unwrap_err()
                .contains("Unknown capability")
        );
        for id in [
            "embeddings:",
            "embeddings:-bad",
            "embeddings:bad;command",
            "embeddings-other",
        ] {
            assert!(
                review_plan(id, &env)
                    .unwrap_err()
                    .contains("Invalid embedding")
            );
        }
        assert_eq!(
            review_plan("hindsight", &env).unwrap_err(),
            run::preview_text("hindsight", &env)
        );
        assert!(review_plan("embeddings:nomic-embed-text", &env).is_err());
        assert!(!env.user_dir.exists());
    }

    #[test]
    fn review_preserves_full_existing_plan_and_skips_installed_targets() {
        let temp = tempfile::tempdir().unwrap();
        let env = empty_env(temp.path());
        // A fake Homebrew entry is enough for pure planning; never execute it.
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("brew"), "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(bin.join("brew"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let env = Env {
            path: vec![bin],
            ..env
        };
        assert_eq!(
            review_plan("ollama", &env).unwrap(),
            run::preview_text("ollama", &env)
        );
        let artifact = deps::install_dir(&env.user_dir, "bun").join("node_modules/.bin/bun");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(artifact, "").unwrap();
        assert!(
            review_plan("bun", &env)
                .unwrap_err()
                .contains("already set up")
        );
    }
}
