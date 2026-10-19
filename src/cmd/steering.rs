//! Steering vector management commands.
use crate::args::{Cli, SteeringCommand};
use crate::overlay::overlay;
use anyhow::Result;
use dial::config::{paths::Layout, types::Config};

pub(crate) fn run(cli: &Cli, layout: &Layout, action: &SteeringCommand) -> Result<()> {
    let config = Config::load(layout, Some(overlay(cli)?))?;
    match action {
        SteeringCommand::Status { json } => {
            let value = dial::steering::status(&config);
            if *json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("{}", dial::steering::render(&value));
            }
        }
        SteeringCommand::Extract {
            pairs,
            layer,
            adapter,
        } => println!(
            "{}",
            dial::steering::run(&config, "extract", pairs, Some(*layer), adapter.as_deref())?
        ),
        SteeringCommand::Evaluate {
            cases,
            adapter,
            pending,
        } => {
            let path = match pending {
                Some(name) => dial::steering::pending_path(&config, name)?,
                None => dial::llm::steering::vector_path(&config),
            };
            println!(
                "{}",
                dial::steering::run_at(
                    &config,
                    "evaluate",
                    cases,
                    None,
                    adapter.as_deref(),
                    &path
                )?
            )
        }
        SteeringCommand::Export { out } => println!(
            "{}",
            serde_json::to_string_pretty(&dial::pack::export_steering(&config, out)?)?
        ),
        SteeringCommand::Import { pack, replace } => println!(
            "{}",
            serde_json::to_string_pretty(&dial::pack::import_steering(&config, pack, *replace)?)?
        ),
        SteeringCommand::Promote { name, replace } => println!(
            "{}",
            serde_json::to_string_pretty(&dial::steering::promote(&config, name, *replace)?)?
        ),
    }
    Ok(())
}
