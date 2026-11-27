//! Dependency and memory setup diagnostics.
use crate::args::Cli;
use crate::overlay::overlay;
use anyhow::Result;
use dial::config::{paths::Layout, types::Config};

pub(crate) fn run(
    cli: &Cli,
    layout: &Layout,
    fix: &Option<String>,
    yes: bool,
    all: bool,
    json: bool,
) -> Result<()> {
    let env = dial::setup::deps::Env::current(&layout.user_dir);
    match fix {
        None => {
            let memory = match Config::load(layout, Some(overlay(cli)?)) {
                Ok(config) => dial::setup::memory::doctor_text(&config),
                Err(error) => format!("Memory: settings could not be read ({error:#})."),
            };
            if json {
                let value = doctor_json(&env, all, &memory);
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                let capabilities = if all {
                    dial::setup::deps::run::list_text(&env, "dial doctor --fix")
                } else {
                    dial::setup::deps::run::summary_text(&env, "dial doctor --all")
                };
                println!("{capabilities}");
                println!();
                println!("{memory}");
            }
        }
        Some(id) if id.starts_with("embeddings") => {
            let model = dial::setup::embeddings::model_from_id(id)
                .ok_or_else(|| anyhow::anyhow!("unknown embedding model in {id}"))?;
            if yes {
                let cancel = std::sync::atomic::AtomicBool::new(false);
                let summary =
                    dial::setup::embeddings::setup(layout, model, &cancel, &mut |line| {
                        println!("{line}")
                    })?;
                println!("{summary}");
            } else {
                println!(
                    "{}\n\nSet it up with: dial doctor --fix {id} --yes",
                    dial::setup::embeddings::preview_text(model, &layout.user_dir)
                );
            }
        }
        Some(id) if !yes => {
            let preview = dial::setup::deps::run::preview_text(id, &env);
            match dial::setup::deps::recipe(id) {
                None => println!(
                    "Unknown capability `{id}`. Run `dial doctor --all` to list them with their ids."
                ),
                Some(recipe) if dial::setup::deps::is_ready(recipe, &env) => println!("{preview}"),
                Some(_) => println!("{preview}\n\nSet it up with: dial doctor --fix {id} --yes"),
            }
        }
        Some(id) => {
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let done =
                dial::setup::deps::run::install_and_save(layout, id, &cancel, &mut |line| {
                    println!("{line}")
                })?;
            println!("Set up and saved:\n{}", done.join("\n"));
        }
    }
    Ok(())
}

pub(crate) fn doctor_json(
    env: &dial::setup::deps::Env,
    all: bool,
    memory_note: &str,
) -> serde_json::Value {
    use dial::setup::deps::Status;
    let rows = dial::setup::deps::report(env);
    let mut capabilities = Vec::new();
    let mut missing = 0usize;
    for row in rows {
        let installed = matches!(row.status, Status::Ready);
        if !installed {
            missing += 1;
            if !all {
                continue;
            }
        }
        capabilities.push(serde_json::json!({
            "id": row.id,
            "name": row.title,
            "installed": installed,
            "path": installed.then_some(row.detail),
        }));
    }
    let mut notes: Vec<String> = Vec::new();
    if !all && missing > 0 {
        notes.push(format!(
            "Not installed: {missing} optional capabilit{}; `dial doctor --all` lists them with their setup commands.",
            if missing == 1 { "y" } else { "ies" }
        ));
    }
    notes.extend(
        memory_note
            .lines()
            .map(|line| line.trim_end().to_owned())
            .filter(|line| !line.is_empty()),
    );
    serde_json::json!({
        "capabilities": capabilities,
        "notes": notes,
    })
}

#[cfg(test)]
mod tests;
