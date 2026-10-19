//! Dependency and memory setup diagnostics.
use crate::args::Cli;
use crate::overlay::overlay;
use anyhow::Result;
use dial::config::{paths::Layout, types::Config};

pub(crate) fn run(cli: &Cli, layout: &Layout, fix: &Option<String>, yes: bool) -> Result<()> {
    let env = dial::setup::deps::Env::current(&layout.user_dir);
    match fix {
        None => {
            println!("{}", dial::setup::deps::run::list_text(&env));
            println!();
            match Config::load(layout, Some(overlay(cli)?)) {
                Ok(config) => println!("{}", dial::setup::memory::doctor_text(&config)),
                Err(error) => {
                    println!("Memory: settings could not be read ({error:#}).")
                }
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
                    "{}\n\nRe-run with --yes to set it up.",
                    dial::setup::embeddings::preview_text(model, &layout.user_dir)
                );
            }
        }
        Some(id) if !yes => println!(
            "{}\n\nRe-run with --yes to set it up.",
            dial::setup::deps::run::preview_text(id, &env)
        ),
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
