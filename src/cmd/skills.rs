//! Skill installation and management commands.
use crate::args::{Cli, SkillCommand};
use crate::overlay::overlay;
use anyhow::Result;
use dial::config::{paths::Layout, types::Config};

pub(crate) fn run(cli: &Cli, layout: &Layout, action: &SkillCommand) -> Result<()> {
    use dial::storage::skills::{self, Scope};
    // Package actions share the slash-command path, so the CLI and `/skills` behave identically.
    let run_package = |words: Vec<String>| -> Result<()> {
        let config = Config::load(layout, Some(overlay(cli)?))?;
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let text = skills::management::command(
            layout,
            &config.skills.registries,
            words,
            &cancel,
            &mut |line: &str| eprintln!("{line}"),
        )?;
        println!("{text}");
        Ok(())
    };
    match action {
        SkillCommand::List => {
            println!("{}", skills::management::list(layout)?);
        }
        SkillCommand::Sources => println!("{}", skills::management::sources(layout)),
        SkillCommand::Enable { name, user } | SkillCommand::Disable { name, user } => {
            let enabled = matches!(action, SkillCommand::Enable { .. });
            skills::management::set_enabled(
                layout,
                name,
                enabled,
                if *user { Scope::User } else { Scope::Project },
            )?;
            println!("{}", skills::management::list(layout)?);
        }
        SkillCommand::Import {
            source,
            user,
            dry_run,
            update,
        } => {
            let source = if source.is_absolute() {
                source.clone()
            } else {
                layout.project_root.join(source)
            };
            for line in skills::import::run(
                layout,
                &source,
                if *user { Scope::User } else { Scope::Project },
                *dry_run,
                *update,
            )? {
                println!("{line}");
            }
        }
        SkillCommand::Install { spec, user } => {
            let mut words = vec!["install".to_owned(), spec.clone()];
            if *user {
                words.push("--user".to_owned());
            }
            run_package(words)?;
        }
        SkillCommand::Update { name } => {
            run_package(
                std::iter::once("update".to_owned())
                    .chain(name.clone())
                    .collect(),
            )?;
        }
        SkillCommand::Remove { name } => run_package(vec!["remove".to_owned(), name.clone()])?,
        SkillCommand::Installed => run_package(vec!["installed".to_owned()])?,
        SkillCommand::Search { query } => {
            run_package(
                std::iter::once("search".to_owned())
                    .chain(query.iter().cloned())
                    .collect(),
            )?;
        }
    }
    Ok(())
}
