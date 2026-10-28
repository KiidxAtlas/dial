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

    // `--restore` picks the same session as `--continue`; the restore itself runs in the app.
    let continue_requested = cli.continue_session || cli.restore;
    let resume_prefix = cli.resume.clone();
    let flag_session = cli.session_flags.resolve(
        &layout.project_root,
        dial::cli::session_flags::Context {
            continue_or_resume: continue_requested || resume_prefix.is_some(),
            no_session: cli.no_session,
            protocol_mode: false,
        },
    )?;
    let session = if let Some(imported) = flag_session {
        Some(imported)
    } else if cli.no_session {
        Some(Session::ephemeral())
    } else if continue_requested || resume_prefix.is_some() {
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
            Some(path) => match Session::open_or_fork(&layout, &path)? {
                dial::storage::session::Resumed::Owned(session) => Some(session),
                dial::storage::session::Resumed::Forked { source_id, session } => {
                    eprintln!(
                        "dial: session {source_id} is open in another process; resumed as a fork of it"
                    );
                    Some(session)
                }
            },
            None if wanted.is_empty() => None,
            None => bail!(
                "no session in this project starts with {wanted:?}; run `dial sessions` to list ids, or `dial -c` to continue the latest"
            ),
        }
    } else {
        None
    };

    let mut text = cli.prompt.join(" ");
    let (images, stdin_image) = stdin_image_input(&cli.images, tty)?;
    if !tty && stdin_image.is_none() {
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
    if cli.restore && !interactive {
        bail!("--restore needs an interactive session; use it without --print");
    }
    if !cli.images.is_empty() && !cli.print {
        bail!(
            "--image is only available in print mode; interactive sessions attach images with @path"
        );
    }
    let options = Options {
        session,
        prompt: text,
        interactive,
        restore: cli.restore,
        overlay,
        notices,
        print_thoughts: cli.print_thoughts,
        launch,
        add_dirs,
        mode_flags: mode,
        images,
        stdin_image,
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

/// `--image -` takes the image bytes from stdin, so stdin cannot also carry piped prompt text.
fn stdin_image_input(
    images: &[std::path::PathBuf],
    tty: bool,
) -> Result<(Vec<std::path::PathBuf>, Option<Vec<u8>>)> {
    let dashes = images.iter().filter(|path| path.as_os_str() == "-").count();
    if dashes == 0 {
        return Ok((images.to_vec(), None));
    }
    if dashes > 1 {
        bail!("--image - may be given once; stdin carries a single image");
    }
    if tty {
        bail!("--image - reads the image bytes from stdin; pipe the image in");
    }
    let limit = dial::llm::images::MAX_IMAGE_SOURCE_BYTES;
    let mut bytes = Vec::new();
    std::io::stdin().take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("stdin image exceeds 64 MiB");
    }
    let files = images
        .iter()
        .filter(|path| path.as_os_str() != "-")
        .cloned()
        .collect();
    Ok((files, Some(bytes)))
}
