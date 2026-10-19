//! Scheduled task commands.
use crate::args::{Cli, ScheduleCommand};
use crate::overlay::overlay;
use anyhow::Result;
use dial::config::{paths::Layout, types::Config};

pub(crate) fn run(cli: &Cli, layout: &Layout, action: &ScheduleCommand) -> Result<()> {
    use dial::schedule::{self, Entry, Store};
    let store = Store::new(layout)?;
    match action {
        ScheduleCommand::Add {
            prompt,
            every,
            watch,
            allow,
            id,
        } => {
            let entry = Entry::new(
                id.clone(),
                prompt.clone(),
                every.as_deref(),
                watch.clone(),
                allow.clone(),
                cli.yes,
            )?;
            println!("added schedule {}", store.add(entry)?);
        }
        ScheduleCommand::List => {
            let state = store.last_runs();
            let entries = store.load()?;
            if entries.is_empty() {
                println!("no schedules; add one with `dial schedule add`");
            }
            for entry in &entries {
                println!(
                    "{}",
                    schedule::describe(entry, state.get(&entry.id).copied())
                );
            }
        }
        ScheduleCommand::Remove { id } => {
            store.remove(id)?;
            println!("removed {id}");
        }
        ScheduleCommand::Log { id } => match std::fs::read_to_string(store.log_path(id)) {
            Ok(text) => print!("{text}"),
            Err(_) => println!("no runs logged for {id}"),
        },
        ScheduleCommand::Run { once } => {
            let config = Config::load(layout, Some(overlay(cli)?))?;
            let stop = dial::platform::interrupt::install_sigint();
            schedule::run(layout, *once, config.worker_timeout_secs, &stop)?;
        }
    }
    Ok(())
}
