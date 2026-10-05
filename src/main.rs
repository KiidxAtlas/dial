mod benchmark_cli;
mod benchmark_rpc;
mod improve_cli;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use dial::{
    app::{self, Options},
    config::types::Config,
    learn::{auto, improve},
    repl::{OutputFormat, PrintExit, PrintUi},
    setup,
    storage::{paths::Layout, session::Session, store::Store},
};
use fs2::FileExt;
use std::{io::{IsTerminal, Read}, path::PathBuf};

#[derive(Subcommand)]
enum LayaCommand {
    /// Install a dedicated Python runtime; copy a checkpoint without linking its source.
    Install { #[arg(long)] checkpoint: Option<PathBuf> },
    /// Start or wait for Dial's managed service.
    Start,
    /// Read live service identity, checkpoint and device.
    Status,
}

#[derive(Parser)]
#[command(version, about = "A local-first coding agent harness that improves itself behind an evaluation gate",
    after_help = "Print exit codes: 0 completed; 1 execution/provider/configuration error; 2 incomplete, blocked, needs input, no answer or interrupted; 3 needed tool approval denied.\nUse --allow write,bash to approve named tools in print mode; --yes approves all tools.")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Message to send (starts a one-shot run with --print, otherwise an interactive session that opens with it).
    prompt: Vec<String>,
    /// Run non-interactively: answer once, print the result, exit.
    #[arg(short, long)]
    print: bool,
    /// Stdout format for a non-interactive turn: text, one JSON result, or JSON events.
    #[arg(long, value_enum, default_value = "text")]
    output_format: OutputFormat,
    /// Pre-approve these named tools in print mode (comma-separated); does not override readonly.
    #[arg(long, value_delimiter = ',')]
    allow: Vec<String>,
    /// Continue the most recent session in this project.
    #[arg(short = 'c', long = "continue", conflicts_with_all = ["resume", "no_session"])]
    continue_session: bool,
    /// Resume a session by id prefix (latest when no id is given).
    #[arg(short, long, num_args = 0..=1, default_missing_value = "", conflicts_with = "no_session")]
    resume: Option<String>,
    /// Do not write the session to disk.
    #[arg(long)]
    no_session: bool,
    /// Work in this directory instead of the current one.
    #[arg(short = 'C', long = "dir", global = true)]
    dir: Option<PathBuf>,
    /// Model: an MLX model id or local path (or the model name on an --endpoint).
    #[arg(short, long, global = true)]
    model: Option<String>,
    /// Select an explicitly configured model capability/reasoning profile.
    #[arg(long, global = true)]
    profile: Option<String>,
    /// Use an OpenAI-compatible server (LM Studio, Ollama, llama.cpp, hosted API) at this base URL.
    #[arg(long, global = true)]
    endpoint: Option<String>,
    /// Environment variable holding the endpoint's API key.
    #[arg(long, global = true)]
    api_key_env: Option<String>,
    /// ask | auto | readonly
    #[arg(long, global = true)]
    approval: Option<String>,
    /// Approve every tool call (same as --approval auto).
    #[arg(short = 'y', long, global = true)]
    yes: bool,
    /// off | dev | strict
    #[arg(long, global = true)]
    sandbox: Option<String>,
    /// hashline | replace
    #[arg(long, global = true)]
    edit_mode: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Install, start or inspect Dial's independent Laya decision service.
    Laya {
        #[command(subcommand)]
        action: LayaCommand,
    },
    /// Inspect saved project understanding without loading a model or refreshing the index.
    Understanding {
        /// Use architecture for the architecture view, or filter findings, paths and outlines.
        query: Option<String>,
        /// Export every matching entry as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Import portable SKILL.md folders from other harnesses or checked-out repositories.
    Skills {
        #[command(subcommand)]
        action: SkillCommand,
    },
    /// Create the Python/MLX environment, save the model chosen with -m as your default, verify it loads.
    Setup,
    /// Persistent JSON-lines agent control protocol on stdin/stdout.
    Rpc,
    /// List recent sessions for this project.
    Sessions {
        /// Include sessions from every project.
        #[arg(long, global = true)]
        all: bool,
        #[command(subcommand)]
        action: Option<SessionCommand>,
    },
    /// The gated self-improvement loop (evaluation suites, harvesting, LoRA training, rollback).
    #[command(subcommand)]
    Improve(improve_cli::Cmd),
    /// Run a reproducible coding benchmark suite in isolated project copies.
    #[command(alias = "bench")]
    Benchmark {
        manifest: PathBuf,
        #[arg(long)]
        baseline: Option<PathBuf>,
        #[arg(long)]
        report: Option<PathBuf>,
        /// TOML file with [[profiles]] names selecting configured model profiles and ablations.
        #[arg(long)]
        profiles: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum SkillCommand {
    /// List installed skills, enablement and locations.
    List,
    /// Browse compatible skills in common local harness directories (does not import).
    Sources,
    /// Enable an installed skill (project override by default).
    Enable { name: String, #[arg(long)] user: bool },
    /// Disable a skill without deleting it (project override by default).
    Disable { name: String, #[arg(long)] user: bool },
    /// Copy one skill directory or a directory containing skills. Never executes bundled scripts.
    Import {
        source: PathBuf,
        #[arg(long)]
        user: bool,
        #[arg(long)]
        dry_run: bool,
        /// Update previously imported, locally unchanged skills from the same source.
        #[arg(long)]
        update: bool,
    },
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Export a conversation as Markdown (or HTML with --html).
    Export {
        id: String,
        path: Option<PathBuf>,
        #[arg(long)]
        html: bool,
    },
    /// Give a session a title.
    Rename { id: String, title: String },
}

fn overlay(cli: &Cli) -> Result<toml::Table> {
    let mut table = toml::Table::new();
    if let Some(model) = &cli.model { table.insert("model".into(), model.clone().into()); }
    if let Some(url) = &cli.endpoint {
        table.insert("provider".into(), "endpoint".into());
        let mut endpoint = toml::Table::new();
        endpoint.insert("base_url".into(), url.clone().into());
        if let Some(var) = &cli.api_key_env { endpoint.insert("api_key_env".into(), var.clone().into()); }
        table.insert("endpoint".into(), endpoint.into());
    }
    let approval = if cli.yes { Some("auto".to_owned()) } else { cli.approval.clone() };
    if let Some(a) = approval { table.insert("approval".into(), a.into()); }
    if let Some(mode) = &cli.sandbox {
        let mut sandbox = toml::Table::new();
        sandbox.insert("mode".into(), mode.clone().into());
        table.insert("sandbox".into(), sandbox.into());
    }
    if let Some(mode) = &cli.edit_mode { table.insert("edit_mode".into(), mode.clone().into()); }
    if let Some(profile) = &cli.profile { table.insert("model_profile".into(), profile.clone().into()); }
    Ok(table)
}

fn main() {
    let cli = Cli::parse();
    let format = cli.output_format;
    let started = std::time::Instant::now();
    if let Err(error) = real_main(cli) {
        if let Some(exit) = error.downcast_ref::<PrintExit>() { std::process::exit(exit.0); }
        eprintln!("error: {error:#}");
        if format != OutputFormat::Text {
            let mut ui = PrintUi::new(format, Vec::new());
            let _ = ui.finish(None, Some(&format!("{error:#}")), None, started.elapsed());
        }
        std::process::exit(1);
    }
}

fn real_main(cli: Cli) -> Result<()> {
    let cwd = match &cli.dir { Some(dir) => dir.clone(), None => std::env::current_dir()? };
    let layout = Layout::discover(&cwd)?;
    match &cli.command {
        Some(Command::Understanding { query, json }) => {
            let report=dial::storage::codebase::Codebase::open(&layout)?.understanding(query.as_deref().unwrap_or(""),&std::sync::atomic::AtomicBool::new(false))?;
            if *json {println!("{}",serde_json::to_string_pretty(&report)?);}
            else {println!("{}",dial::storage::codebase::render_understanding(&report,usize::MAX));}
            return Ok(());
        }
        Some(Command::Skills { action }) => {
            use dial::storage::skills::{self, Scope};
            match action {
                SkillCommand::List => {
                    println!("{}", skills::management::list(&layout)?);
                }
                SkillCommand::Sources => println!("{}", skills::management::sources(&layout)),
                SkillCommand::Enable { name, user } | SkillCommand::Disable { name, user } => {
                    let enabled = matches!(action, SkillCommand::Enable { .. });
                    skills::management::set_enabled(&layout, name, enabled, if *user { Scope::User } else { Scope::Project })?;
                    println!("{}", skills::management::list(&layout)?);
                }
                SkillCommand::Import { source, user, dry_run, update } => {
                    let source = if source.is_absolute() { source.clone() } else { layout.project_root.join(source) };
                    for line in skills::import::run(&layout, &source, if *user { Scope::User } else { Scope::Project }, *dry_run, *update)? {
                        println!("{line}");
                    }
                }
            }
            return Ok(());
        }
        Some(Command::Laya {action}) => {
            use dial::agent::laya;
            match action {
                LayaCommand::Install {checkpoint} => {
                    eprintln!("Installing Dial-owned Laya runtime and checkpoint…");
                    laya::install(&layout,checkpoint.as_deref())?;
                    println!("Dial Laya installed in {}. Run dial laya start, then /laya on inside Dial.",layout.user_dir.join("laya").display());
                }
                LayaCommand::Start | LayaCommand::Status => {
                    let config=Config::load(&layout,Some(overlay(&cli)?))?;
                    let cancel=std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let health=if matches!(action,LayaCommand::Start) {laya::ensure_service(&config.laya,cancel,true)?} else {laya::health(&config.laya,cancel)?};
                    println!("{}",serde_json::to_string_pretty(&health)?);
                }
            }
            return Ok(());
        }
        Some(Command::Setup) => return setup::run(&layout, cli.model.as_deref()),
        Some(Command::Sessions { all, action }) => {
            let sessions = if *all { Session::list_all(&layout, usize::MAX)? } else { Session::list(&layout, usize::MAX)? };
            if let Some(action) = action {
                let id = match action { SessionCommand::Export { id, .. } | SessionCommand::Rename { id, .. } => id };
                let mut matching = sessions.iter().filter(|s| s.id.starts_with(id));
                let info = matching.next().ok_or_else(|| anyhow::anyhow!("no session matching {id:?}"))?;
                if matching.next().is_some() { bail!("session prefix {id:?} is ambiguous; use a longer id"); }
                let mut session = Session::open(&info.path)?;
                match action {
                    SessionCommand::Rename { title, .. } => {
                        session.rename(title)?;
                        println!("Renamed {} to {}", info.id, session.title());
                    }
                    SessionCommand::Export { path, html, .. } => {
                        let path = path.clone().unwrap_or_else(|| PathBuf::from(format!("session-{}.{}", info.id, if *html { "html" } else { "md" })));
                        let path = if path.is_absolute() { path } else { layout.project_root.join(path) };
                        session.export(&path, *html)?;
                        println!("{}", path.display());
                    }
                }
            } else {
                for s in sessions.iter().take(if *all { usize::MAX } else { 30 }) {
                    println!("{}  {}  {:>4} msgs  {}{}", &s.id[..s.id.len().min(8)], s.started, s.messages, s.title,
                        if *all { format!("  {}", s.path.display()) } else { String::new() });
                }
            }
            return Ok(());
        }
        _ => {}
    }
    let config = Config::load(&layout, Some(overlay(&cli)?))?;
    if matches!(cli.command, Some(Command::Rpc)) {
        auto::prepare_state(&layout)?;
        return dial::rpc::run(layout, config);
    }
    if let Some(Command::Benchmark { manifest, baseline, report, profiles }) = cli.command.as_ref() {
        return benchmark_cli::run(&layout, &config, manifest, baseline.as_deref(), report.as_deref(), profiles.as_deref());
    }
    if let Some(Command::Improve(cmd)) = cli.command {
        return improve_cli::run(cmd, &layout, config);
    }
    chat(cli, layout, config)
}

fn chat(cli: Cli, layout: Layout, config: Config) -> Result<()> {
    let tty = std::io::stdin().is_terminal();
    if cli.print || !tty { config.require_model()?; }
    auto::prepare_state(&layout)?;
    let mut notices: Vec<String> = Vec::new();
    let store = if config.improve.enabled && !config.model.is_empty() {
        match Store::open(&config.state_dir, &config.model, &config.use_case, &config.harness) {
            Ok(store) => {
                let control = auto::lock_file(&config.state_dir.join("control.lock"))?;
                if control.try_lock_exclusive().is_ok() && auto::enabled(&config, &store)? {
                    if let Some(note) = improve::monitor(&store, &config)? { notices.push(note); }
                }
                if !auto::enabled(&config, &store)? {
                    notices.push("automatic improvement is paused; observations are still recorded (`dial improve enable` resumes)".into());
                }
                Some(store)
            }
            Err(error) => { notices.push(format!("self-improvement recording disabled: {error:#}")); None }
        }
    } else { None };

    let resume_prefix = cli.resume.clone();
    let session = if cli.no_session {
        Some(Session::ephemeral())
    } else if cli.continue_session || resume_prefix.is_some() {
        let wanted = resume_prefix.unwrap_or_default();
        let matches: Vec<_> = Session::list(&layout, usize::MAX)?.into_iter()
            .filter(|s| wanted.is_empty() || s.id.starts_with(&wanted)).collect();
        if !wanted.is_empty() && matches.len() > 1 { bail!("session prefix {wanted:?} is ambiguous; use a longer id"); }
        let found = matches.into_iter().next();
        match found {
            Some(info) => Some(Session::open(&info.path)?),
            None => bail!("no session to resume in this project"),
        }
    } else {
        None
    };

    let mut text = cli.prompt.join(" ");
    if !tty {
        let mut piped = String::new();
        std::io::stdin().read_to_string(&mut piped)?;
        if !piped.trim().is_empty() {
            text = if text.is_empty() { piped } else {
                let fence = "`".repeat(longest_backtick_run(&piped).max(2) + 1);
                format!("{text}\n\nStdin context:\n{fence}\n{piped}\n{fence}")
            };
        }
    }
    if cli.print && text.trim().is_empty() { bail!("--print needs a prompt (argument or stdin)"); }
    drop(store);
    let interactive = tty && !cli.print && cli.output_format == OutputFormat::Text;
    let options = Options {
        session, prompt: text, interactive, overlay: overlay(&cli)?, notices,
    };
    if interactive {
        if !cli.allow.is_empty() { bail!("--allow is only available in print mode"); }
        app::run_interactive(layout, config, options)
    } else {
        let code = app::run_print(layout, config, options, cli.output_format, cli.allow)?;
        if code != 0 { return Err(PrintExit(code).into()); }
        Ok(())
    }
}

fn longest_backtick_run(text: &str) -> usize {
    let (mut longest, mut current) = (0, 0);
    for byte in text.bytes() {
        if byte == b'`' { current += 1; longest = longest.max(current); }
        else { current = 0; }
    }
    longest
}
