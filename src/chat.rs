use crate::args::Cli;
use anyhow::{Result, bail};
use dial::app::{self, Options};
use dial::config::paths::Layout;
use dial::config::types::Config;
use dial::repl::{OutputFormat, PrintExit};
use dial::storage::session::Session;
use std::io::{IsTerminal, Read};

pub(crate) fn chat(
    cli: Cli,
    layout: Layout,
    config: Config,
    overlay: toml::Table,
    launch: dial::agent::modes::Launch,
    add_dirs: Vec<std::path::PathBuf>,
    mode: dial::cli::mode_flags::ModeFlags,
) -> Result<()> {
    let tty = std::io::stdin().is_terminal();
    let interactive_capable = tty && std::io::stdout().is_terminal();
    if cli.print || !interactive_capable {
        config.require_model()?;
    }
    let config = config.for_coding();
    let notices: Vec<String> = Vec::new();

    let resume_prefix = cli.resume.clone();
    let flag_session = cli.session_flags.resolve(
        &layout.project_root,
        dial::cli::session_flags::Context {
            continue_or_resume: cli.continue_session || resume_prefix.is_some(),
            no_session: cli.no_session,
            protocol_mode: false,
        },
    )?;
    let session = if let Some(imported) = flag_session {
        Some(imported)
    } else if cli.no_session {
        Some(Session::ephemeral())
    } else if cli.continue_session || resume_prefix.is_some() {
        let wanted = resume_prefix.unwrap_or_default();
        let catalogue = Session::list_by_activity(&layout, usize::MAX)?;
        let found = if wanted.is_empty() {
            // `-c` resumes the most recently active conversation; empty sessions never count.
            let latest =
                dial::storage::session::latest_conversation(&catalogue).map(|s| s.path.clone());
            if latest.is_none() {
                eprintln!("No earlier conversation in this project; starting a new session.");
            }
            latest
        } else {
            let matches: Vec<_> = catalogue
                .iter()
                .filter(|s| s.id.starts_with(&wanted))
                .collect();
            if matches.len() > 1 {
                let ids: Vec<&str> = matches.iter().take(5).map(|s| s.id.as_str()).collect();
                bail!(
                    "session prefix {wanted:?} matches {} sessions ({}); use a longer id",
                    matches.len(),
                    ids.join(", ")
                );
            }
            matches.first().map(|s| s.path.clone())
        };
        match found {
            Some(path) => Some(Session::open(&path)?),
            None if wanted.is_empty() => None,
            None => bail!(
                "no session in this project starts with {wanted:?}; run `dial sessions` to list ids, or `dial -c` to continue the latest"
            ),
        }
    } else {
        None
    };

    let mut text = cli.prompt.join(" ");
    if !tty {
        let mut piped = String::new();
        std::io::stdin().read_to_string(&mut piped)?;
        if !piped.trim().is_empty() {
            text = if text.is_empty() {
                piped
            } else {
                let fence = "`".repeat(longest_backtick_run(&piped).max(2) + 1);
                format!("{text}\n\nStdin context:\n{fence}\n{piped}\n{fence}")
            };
        }
    }
    if cli.print && text.trim().is_empty() {
        bail!("--print needs a prompt (argument or stdin)");
    }
    let interactive = interactive_capable && !cli.print && cli.output_format == OutputFormat::Text;
    let options = Options {
        session,
        prompt: text,
        interactive,
        overlay,
        notices,
        print_thoughts: cli.print_thoughts,
        launch,
        add_dirs,
        mode_flags: mode,
    };
    if interactive {
        if !cli.allow.is_empty() {
            bail!("--allow is only available in print mode");
        }
        app::run_interactive(layout, config, options)
    } else {
        let code = app::run_print(layout, config, options, cli.output_format, cli.allow)?;
        if code != 0 {
            return Err(PrintExit(code).into());
        }
        Ok(())
    }
}

fn longest_backtick_run(text: &str) -> usize {
    let (mut longest, mut current) = (0, 0);
    for byte in text.bytes() {
        if byte == b'`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}
