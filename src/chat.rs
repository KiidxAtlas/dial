use crate::args::Cli;
use anyhow::{Result, bail};
use dial::app::{self, Options};
use dial::cli::session_flags::{self, Selection};
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
    let config = config.for_coding();
    let notices: Vec<String> = Vec::new();

    // `--restore` picks the same session as `--continue`; the restore itself runs in the app.
    let continue_requested = cli.continue_session || cli.restore;
    let resume_prefix = cli.resume.as_deref();
    let selection = cli
        .session_flags
        .selection(dial::cli::session_flags::Context {
            continue_or_resume: continue_requested || resume_prefix.is_some(),
            no_session: cli.no_session,
            protocol_mode: false,
        })?;
    if let Some(Selection::Import { source, path: None }) = selection {
        bail!(
            "{}",
            session_flags::candidates_message(source, &layout.project_root)
        );
    }

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
    let output_schema = match &cli.output_schema {
        Some(path) => Some(dial::cli::print_output::load_schema(path)?),
        None => None,
    };
    if (output_schema.is_some() || cli.output_last_message.is_some()) && !cli.print {
        bail!("--output-schema and -o/--output-last-message need print mode (-p)");
    }
    if interactive && !cli.allow.is_empty() {
        bail!("--allow is only available in print mode");
    }

    let resume_path = if continue_requested || resume_prefix.is_some() {
        let wanted = resume_prefix.unwrap_or_default();
        let catalogue = Session::list_by_activity(&layout, usize::MAX)?;
        let found = if wanted.is_empty() {
            // `-c` resumes the most recently active conversation; empty sessions never count.
            let latest =
                dial::storage::session::latest_conversation(&catalogue).map(|s| s.path.clone());
            if latest.is_none() {
                // A one-shot run that asked to continue must not silently lose its context.
                if cli.print {
                    bail!(
                        "no earlier conversation in this project to continue; drop -c to start a new one"
                    );
                }
                eprintln!("No earlier conversation in this project; starting a new session.");
            }
            latest
        } else {
            let matches: Vec<_> = catalogue
                .iter()
                .filter(|s| s.id.starts_with(wanted))
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
        if found.is_none() && !wanted.is_empty() {
            bail!(
                "no session in this project starts with {wanted:?}; run `dial sessions` to list ids, or `dial -c` to continue the latest"
            );
        }
        found
    } else {
        None
    };

    // Resolve read-only diagnostics first, but never open, import or fork on a launch refusal.
    if !interactive {
        config.require_model()?;
    }
    let session = if let Some(selection) = selection {
        Some(selection.resolve(&layout.project_root)?)
    } else if let Some(path) = resume_path {
        if interactive && !cli.restore && Session::open_elsewhere(&path) {
            let id = Session::read(&path)?.id().to_owned();
            dial::cli::watch::attach_with_prompt(&layout, &id, &text)?;
            return Ok(());
        }
        match Session::open_or_fork(&layout, &path)? {
            dial::storage::session::Resumed::Owned(session) => Some(session),
            dial::storage::session::Resumed::Forked { source_id, session } => {
                eprintln!(
                    "dial: session {source_id} is open in another process; resumed as a fork of it"
                );
                Some(session)
            }
        }
    } else {
        cli.no_session.then(Session::ephemeral)
    };
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
        output_schema,
        output_last_message: cli.output_last_message.clone(),
        review: None,
    };
    if interactive {
        app::run_interactive(layout, config, options)
    } else {
        let code = app::run_print(layout, config, options, cli.output_format, cli.allow)?;
        if code != 0 {
            return Err(PrintExit(code).into());
        }
        Ok(())
    }
}

/// Plain stdin deliberately bypasses composer history, agent hooks and the session lifecycle.
pub(crate) fn private(layout: &Layout, config: &Config) -> Result<()> {
    use dial::agent::modes::btw::SideRequest;
    config.require_model()?;
    let tty = std::io::stdin().is_terminal();
    eprintln!(
        "Private chat: no Dial transcript, memory, tools or usage stats. Your selected provider receives the conversation and may retain it."
    );
    if tty {
        eprintln!("One message per line; /exit to end.");
    }
    let _lease = config
        .requires_compute_lock()
        .then(|| dial::learn::auto::compute_lock(layout))
        .transpose()?;
    let _owner = _lease
        .as_ref()
        .map(|_| {
            dial::learn::compute::owner_record(
                &layout.user_dir,
                "session",
                &layout.project_root,
                &config.state_dir,
                &config.model,
            )
        })
        .transpose()?;
    let mut prior = Vec::new();
    loop {
        let mut input = String::new();
        if tty {
            if std::io::stdin().read_line(&mut input)? == 0 || input.trim() == "/exit" {
                break;
            }
            if input.trim().is_empty() {
                continue;
            }
        } else {
            std::io::stdin().read_to_string(&mut input)?;
        }
        let request = SideRequest::private(config, &prior, &input)?;
        let answer = request.run(Default::default(), &mut |_| {})?;
        if std::io::stdout().is_terminal() {
            println!("{}", dial::ui::text::sanitize(&answer));
        } else {
            println!("{answer}");
        }
        if !tty {
            break;
        }
        prior.push((input, answer));
    }
    Ok(())
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
