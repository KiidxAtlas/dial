use crate::args::{Cli, Command};
use crate::chat::chat;
use crate::cmd;
use crate::exit_with;
use crate::overlay::overlay;
use anyhow::{Result, bail};
use clap::CommandFactory;
use dial::config::paths::Layout;
use dial::config::types::Config;
use dial::repl::OutputFormat;
use dial::setup;
use std::io::IsTerminal;

pub(crate) fn real_main(mut cli: Cli) -> Result<()> {
    if let Some(Command::Completions { shell }) = &cli.command {
        clap_complete::generate(*shell, &mut Cli::command(), "dial", &mut std::io::stdout());
        return Ok(());
    }
    if let Some(Command::Update { version }) = &cli.command {
        return cmd::update::run(version.as_deref());
    }
    if let Some(mode) =
        dial::cli::cli_modes::resolve(cli.mode.as_deref(), cli.no_ui).map_err(anyhow::Error::msg)?
    {
        cli.output_format = mode.output;
    }
    let cwd = match &cli.dir {
        Some(dir) => dir.clone(),
        None => std::env::current_dir()?,
    };
    let layout = Layout::discover(&cwd)?;
    match &cli.command {
        Some(Command::Stats {
            json,
            serve,
            port,
            range,
        }) => {
            let config = Config::load(&layout, Some(overlay(&cli)?))?;
            return dial::stats::cli(
                &layout.user_dir,
                &config.stats,
                *json,
                *serve,
                *port,
                Some(range),
            );
        }
        Some(Command::Relay { bind }) => return dial::collab::server::serve(*bind),
        Some(Command::Share {
            id,
            endpoint,
            gist,
            whole_journal,
        }) => {
            return cmd::sessions::share(&cli, &layout, id, endpoint, *gist, *whole_journal);
        }
        Some(Command::Reliability { action }) => {
            return cmd::inspection::reliability(&cli, &layout, action);
        }
        Some(Command::Policy { prompt, json }) => {
            return cmd::inspection::policy(&cli, &layout, prompt, *json);
        }
        Some(Command::Steering { action }) => {
            return cmd::steering::run(&cli, &layout, action);
        }
        Some(Command::Experience { json }) => {
            let report = dial::storage::experience::Experience::open(&layout)?.inspect()?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", dial::storage::experience::render(&report));
            }
            return Ok(());
        }
        Some(Command::Understanding { query, json }) => {
            let report = dial::tools::codebase::Codebase::open(&layout)?.understanding(
                query.as_deref().unwrap_or(""),
                &std::sync::atomic::AtomicBool::new(false),
            )?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "{}",
                    dial::tools::codebase::render_understanding(&report, usize::MAX)
                );
            }
            return Ok(());
        }
        Some(Command::Skills { action }) => {
            return cmd::skills::run(&cli, &layout, action);
        }
        Some(Command::Laya { action }) => {
            return cmd::laya::run(&cli, &layout, action);
        }
        Some(Command::Schedule { action }) => {
            return cmd::schedule::run(&cli, &layout, action);
        }
        Some(Command::Update { version }) => return cmd::update::run(version.as_deref()),
        Some(Command::Setup) => {
            return setup::run(
                &layout,
                cli.model.as_deref(),
                dial::learn::worker::capabilities,
            );
        }
        Some(Command::Doctor { fix, yes }) => {
            return cmd::doctor::run(&cli, &layout, fix, *yes);
        }
        Some(Command::Sessions { all, action }) => {
            return cmd::sessions::run(&layout, *all, action);
        }
        Some(Command::Annotate(args)) => {
            return dial::cli::annotate::run(&layout, args);
        }
        Some(Command::Config(args)) => {
            return exit_with(dial::cli::config_models::run_config_with_overlay(
                &cwd,
                args,
                Some(overlay(&cli)?),
            )?);
        }
        Some(Command::Commit { args }) => {
            let config = Config::load(&layout, Some(overlay(&cli)?))?;
            return exit_with(dial::cli::commit::run(
                &config,
                &cwd,
                args,
                cli.model.as_deref(),
            )?);
        }
        Some(Command::Models(args)) => {
            return exit_with(dial::cli::config_models::run_models(
                &cwd,
                args,
                Some(overlay(&cli)?),
            )?);
        }
        Some(Command::Images(args)) => {
            return exit_with(dial::cli::images::run(&layout, args.clone())?);
        }
        Some(Command::Gc(args)) => {
            return exit_with(dial::cli::gc::run(&layout, args.clone())?);
        }
        Some(Command::BrowserRelay(args)) => {
            return exit_with(dial::cli::browser_relay::run(&layout, args.clone())?);
        }
        Some(Command::Anonymize(args)) => {
            return exit_with(dial::cli::anonymize::run(&layout, args.clone())?);
        }
        Some(Command::Ps(args)) => {
            return exit_with(dial::cli::ps::run(&layout.user_dir, args.clone())?);
        }
        Some(Command::Plugin(args)) => {
            return exit_with(dial::cli::plugin::run(args.clone(), &layout)?);
        }
        Some(Command::Search(args)) => {
            let config = Config::load(&layout, Some(overlay(&cli)?))?;
            return exit_with(dial::cli::search::run(&layout, &config, args)?);
        }
        _ => {}
    }
    let mut table = overlay(&cli)?;
    let mode = dial::cli::mode_flags::resolve(
        &cli.mode_args,
        dial::cli::mode_flags::LaunchContext {
            interactive: std::io::stdin().is_terminal()
                && std::io::stdout().is_terminal()
                && !cli.print
                && cli.output_format == OutputFormat::Text,
            resumes_session: cli.continue_session || cli.resume.is_some(),
            has_positional_prompt: !cli.prompt.is_empty(),
            has_piped_input: !std::io::stdin().is_terminal(),
            no_tools: cli.tool_flags.no_tools,
        },
    )?;
    mode.merge_into_overlay(&mut table);
    let base = Config::load(&layout, Some(table.clone()))?;
    let tool_settings =
        dial::cli::tool_flags::apply(&cli.tool_flags, &mut table, &base.integrations, &[])?;
    if !tool_settings.blocked.is_empty() {
        let reasons: Vec<String> = tool_settings
            .blocked
            .iter()
            .map(|b| format!("{}: {}", b.flag, b.reason))
            .collect();
        bail!("unsupported flags: {}", reasons.join("; "));
    }
    let launch = dial::cli::launch::agent_launch(&tool_settings, &cli.model_flags);
    for note in tool_settings
        .partial
        .iter()
        .chain(tool_settings.notes.iter())
    {
        eprintln!("note: {}: {}", note.flag, note.reason);
    }
    let config = Config::load(&layout, Some(table.clone()))?;
    let before_model_flags = config.clone();
    let config = dial::cli::model_scope::apply(&layout, config, &cli.model_flags)?;
    let config = dial::cli::model_flags::apply(config, &cli.model_flags)?;
    let config = mode.apply_plan_model(config)?;
    dial::cli::model_flags::capture_overlay(&mut table, &before_model_flags, &config)?;
    let route = dial::cli::cli_modes::resolve(cli.mode.as_deref(), cli.no_ui)
        .map_err(anyhow::Error::msg)?
        .map(|mode| mode.route);
    if matches!(cli.command, Some(Command::Rpc)) || route == Some(dial::cli::cli_modes::Route::Rpc)
    {
        return dial::rpc::run(layout, config.for_coding());
    }
    if matches!(cli.command, Some(Command::Acp)) || route == Some(dial::cli::cli_modes::Route::Acp)
    {
        return dial::acp::run(layout, config.for_coding());
    }
    if let Some(Command::Benchmark {
        manifest,
        baseline,
        report,
        profiles,
        checkpoint,
        resume,
        ablate_assistance,
        allow_remote,
    }) = cli.command.as_ref()
    {
        if *ablate_assistance {
            anyhow::ensure!(
                baseline.is_none() && profiles.is_none() && checkpoint.is_none(),
                "--ablate-assistance cannot be combined with --baseline, --profiles or --checkpoint"
            );
            return cmd::benchmark::run_assist(
                &layout,
                &config,
                manifest
                    .as_deref()
                    .unwrap_or(std::path::Path::new("examples/benchmarks/efficiency.toml")),
                report.as_deref(),
                *allow_remote,
            );
        }
        let manifest = manifest.as_deref().ok_or_else(|| {
            anyhow::anyhow!("benchmark requires a suite manifest (or --ablate-assistance)")
        })?;
        return cmd::benchmark::run(
            &layout,
            &config,
            manifest,
            baseline.as_deref(),
            report.as_deref(),
            profiles.as_deref(),
            checkpoint.as_deref(),
            *resume,
        );
    }
    if let Some(Command::Improve(cmd)) = cli.command {
        return cmd::improve::run(cmd, &layout, config);
    }
    chat(
        cli,
        layout,
        config,
        table,
        launch,
        tool_settings.add_dirs,
        mode,
    )
}
