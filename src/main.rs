#[path = "cli/benchmark/mod.rs"]
mod benchmark_cli;
#[path = "cli/improve.rs"]
mod improve_cli;
#[path = "cli/update.rs"]
mod update;

use anyhow::{Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use dial::{
    app::{self, Options},
    config::types::Config,
    repl::{OutputFormat, PrintExit, PrintUi},
    setup,
    storage::{paths::Layout, session::Session},
};
use std::{
    io::{IsTerminal, Read},
    path::PathBuf,
};

#[derive(Subcommand)]
enum LayaCommand {
    /// Install a dedicated Python runtime; copy a checkpoint without linking its source.
    Install {
        #[arg(long)]
        checkpoint: Option<PathBuf>,
    },
    /// Start or wait for Dial's managed service.
    Start,
    /// Read live service identity, checkpoint and device.
    Status,
}

#[derive(Parser)]
#[command(
    version = dial::build_info::VERSION,
    about = "A local-first coding agent with optional gated learning",
    after_help = "Print exit codes: 0 completed; 1 execution/provider/configuration error; 2 incomplete, blocked, needs input, no answer or interrupted; 3 needed tool approval denied.\nUse --allow write,bash to approve named tools in print mode; --yes approves all tools."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Show build revision, build-time dirty state, target and actual executable path, then exit.
    #[arg(long, global = true)]
    build_info: bool,
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
    #[arg(short, long, alias = "session", num_args = 0..=1, default_missing_value = "", conflicts_with = "no_session")]
    resume: Option<String>,
    /// Do not write the session to disk.
    #[arg(long)]
    no_session: bool,
    /// Work in this directory instead of the current one.
    #[arg(short = 'C', long = "dir", alias = "cwd", global = true)]
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
    /// Tool approval policy (default: write).
    #[arg(long, global = true, value_parser = clap::builder::PossibleValuesParser::new(dial::config::types::Approval::NAMES.iter().copied()))]
    approval: Option<String>,
    /// Approve every tool call (same as --approval auto; also accepted as --yolo / --auto-approve).
    #[arg(short = 'y', long, global = true, aliases = ["auto-approve", "yolo"])]
    yes: bool,
    /// Compatibility alias for --approval.
    #[arg(long = "approval-mode", global = true, value_name = "MODE", value_parser = clap::builder::PossibleValuesParser::new(dial::cli::cli_modes::APPROVAL_ALIASES.iter().map(|&(alias, _)| alias)))]
    approval_mode: Option<String>,
    /// Protocol mode: text | json | rpc | rpc-ui | acp (overrides --output-format and the default path).
    #[arg(long, global = true, value_name = "MODE")]
    mode: Option<String>,
    /// RPC modes only: run without a UI surface (requires --mode rpc or rpc-ui).
    #[arg(long, global = true)]
    no_ui: bool,
    /// Print mode (text): also write the model's reasoning to stdout before the answer.
    #[arg(long, global = true)]
    print_thoughts: bool,
    /// off | dev | strict
    #[arg(long, global = true)]
    sandbox: Option<String>,
    /// apply_patch | hashline | patch | replace | sloppy
    #[arg(long, global = true, value_parser = ["apply_patch", "hashline", "patch", "replace", "sloppy"])]
    edit_mode: Option<String>,
    /// Automatic context policy: context-full | handoff | shake | snapcompact | off.
    #[arg(long, global = true, value_parser = ["context-full", "handoff", "shake", "snapcompact", "off"])]
    compaction_strategy: Option<String>,
    /// Fixed automatic compaction trigger; use auto to clear the saved token trigger.
    #[arg(long, global = true)]
    compaction_threshold_tokens: Option<String>,
    /// Positive context/output reserve; use auto to clear the saved reserve.
    #[arg(long, global = true)]
    compaction_reserve_tokens: Option<String>,
    #[command(flatten)]
    model_flags: dial::cli::model_flags::ModelFlags,
    #[command(flatten)]
    tool_flags: dial::cli::tool_flags::ToolFlagArgs,
    #[command(flatten)]
    mode_args: dial::cli::mode_flags::ModeArgs,
    #[command(flatten)]
    session_flags: dial::cli::session_flags::SessionFlagArgs,
}

#[derive(Subcommand)]
enum SteeringCommand {
    Status {
        #[arg(long)]
        json: bool,
    },
    Extract {
        #[arg(long)]
        pairs: PathBuf,
        #[arg(long)]
        layer: usize,
        #[arg(long)]
        adapter: Option<PathBuf>,
    },
    Evaluate {
        #[arg(long)]
        cases: PathBuf,
        #[arg(long)]
        adapter: Option<PathBuf>,
        /// Evaluate a pending import (see `steering import`) instead of the active vector.
        #[arg(long)]
        pending: Option<String>,
    },
    /// Export the local steering vector as a verified, portable pack.
    Export {
        #[arg(long)]
        out: PathBuf,
    },
    /// Import a steering pack as a pending vector. The active vector is never touched.
    Import {
        pack: PathBuf,
        /// Overwrite an existing pending import with the same name.
        #[arg(long)]
        replace: bool,
    },
    /// Make a locally evaluated pending vector the active one.
    Promote {
        name: String,
        /// Replace the currently active steering vector.
        #[arg(long)]
        replace: bool,
    },
}

#[derive(Subcommand)]
enum ReliabilityCommand {
    /// Inspect saved capability results without inference.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Measure tool contracts through the selected backend using disposable fixtures.
    Check {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum Command {
    /// Generate shell completions from the live command and argument definitions.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Show, read, or change Dial configuration.
    Config(dial::cli::config_models::ConfigArgs),
    /// Record or list human feedback tied to immutable artifact content.
    Annotate(dial::cli::annotate::AnnotateArgs),
    /// Generate a commit message for the working tree and commit it.
    #[command(trailing_var_arg = true, allow_hyphen_values = true)]
    Commit {
        /// Same flags as `/commit`: --push, --dry-run, --legacy, --no-changelog, -c <context>, -m <model>.
        #[arg(num_args = 0.., allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// List and inspect models.
    Models(dial::cli::config_models::ModelsArgs),
    /// Image commands: status, doctor, probe, purge.
    #[command(visible_alias = "img")]
    Images(dial::cli::images::ImagesArgs),
    /// Sweep stale blobs, archives, and WAL state (dry run unless --apply).
    Gc(dial::cli::gc::GcArgs),
    /// Install the Chrome extension, run the browser relay, or check its connection.
    BrowserRelay(dial::cli::browser_relay::BrowserRelayArgs),
    /// Write an anonymized copy of a session (redacted content, metadata kept).
    Anonymize(dial::cli::anonymize::AnonymizeArgs),
    /// List and manage background processes.
    Ps(dial::cli::ps::PsArgs),
    /// Manage plugins and marketplaces.
    #[command(alias = "plugins")]
    Plugin(dial::cli::plugin::PluginArgs),
    /// Web search.
    #[command(alias = "q", alias = "web-search")]
    Search(dial::cli::search::SearchArgs),
    /// Inspect tool reliability or run a bounded backend capability check.
    Reliability {
        #[command(subcommand)]
        action: ReliabilityCommand,
    },
    /// Inspect adaptive controls, or preview a task policy without loading a model.
    Policy {
        prompt: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Extract, evaluate or inspect experimental local MLX activation steering.
    Steering {
        #[command(subcommand)]
        action: SteeringCommand,
    },
    /// Install, start or inspect Dial's independent Laya decision service.
    Laya {
        #[command(subcommand)]
        action: LayaCommand,
    },
    /// Inspect project experience, reusable recipes and observed utility without loading a model.
    Experience {
        #[arg(long)]
        json: bool,
    },
    /// Inspect saved project understanding without loading a model or refreshing the index.
    Understanding {
        /// Use architecture for the architecture view, or filter findings, paths and outlines.
        query: Option<String>,
        /// Export every matching entry as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Install skill packages from Git or registries, or import SKILL.md folders from other tools.
    Skills {
        #[command(subcommand)]
        action: SkillCommand,
    },
    /// Create the Python/MLX environment, save the model chosen with -m as your default, verify it loads.
    Setup,
    /// List optional capabilities (MCP helpers, memory backends, voice, LSP) and their status.
    /// `--fix <id>` previews the install; add `--yes` to run it.
    Doctor {
        /// Capability to set up, e.g. `bun`.
        #[arg(long)]
        fix: Option<String>,
        /// Run the install after previewing it.
        #[arg(long, requires = "fix")]
        yes: bool,
    },
    /// Verify and install the latest signed release, or a specified release version.
    Update {
        /// Version number or v-prefixed release tag (defaults to latest).
        #[arg(long)]
        version: Option<String>,
    },
    /// Persistent JSON-lines agent control protocol on stdin/stdout.
    Rpc,
    /// ACP v1 editor integration over stdio JSON-RPC 2.0 (one launched project).
    Acp,
    /// Recurring or file-triggered prompt runs for this project.
    Schedule {
        #[command(subcommand)]
        action: ScheduleCommand,
    },
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
        /// Suite TOML. Optional with --ablate-assistance (defaults to examples/benchmarks/efficiency.toml).
        manifest: Option<PathBuf>,
        #[arg(long)]
        baseline: Option<PathBuf>,
        #[arg(long)]
        report: Option<PathBuf>,
        /// TOML file with [[profiles]] names selecting configured model profiles and ablations.
        #[arg(long)]
        profiles: Option<PathBuf>,
        /// Durable case checkpoint file for a single configured run.
        #[arg(long)]
        checkpoint: Option<PathBuf>,
        /// Resume only if task, source, configuration and executable fingerprints still match.
        #[arg(long, requires = "checkpoint")]
        resume: bool,
        /// Measure raw (assistance off), dial (shipped defaults) and one ablation per assistance feature.
        #[arg(long)]
        ablate_assistance: bool,
        /// Permit remote inference endpoints for --ablate-assistance runs.
        #[arg(long)]
        allow_remote: bool,
    },
    /// Inspect aggregate model/tool usage, or serve a loopback dashboard.
    Stats {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        serve: bool,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long, default_value = "all")]
        range: String,
    },
    /// Run a local WebSocket collaboration relay.
    Relay {
        #[arg(long, default_value = "127.0.0.1:7331")]
        bind: std::net::SocketAddr,
    },
    /// Share the latest session (or a selected session) through an encrypted upload endpoint.
    Share {
        id: Option<String>,
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long, conflicts_with = "endpoint")]
        gist: bool,
        /// Diagnostic: share the whole journal, including abandoned branches and internal records,
        /// instead of the active branch.
        #[arg(long)]
        whole_journal: bool,
    },
}
#[derive(Subcommand)]
enum ScheduleCommand {
    /// Add a schedule. Runs are read-only unless --yes (everything) or --allow (named tools) is given.
    Add {
        prompt: String,
        /// Repeat at this interval: 30s, 15m, 2h, 1d (minimum 30s).
        #[arg(long)]
        every: Option<String>,
        /// Run when files matching this glob change (repeatable), e.g. 'src/**/*.rs'.
        #[arg(long)]
        watch: Vec<String>,
        /// Tools approved for unattended runs (comma-separated), e.g. read,edit.
        #[arg(long, value_delimiter = ',')]
        allow: Vec<String>,
        /// Name for the schedule (default s1, s2, …).
        #[arg(long)]
        id: Option<String>,
    },
    /// List schedules and when they last ran.
    List,
    /// Delete a schedule.
    Remove { id: String },
    /// Print a schedule's run log.
    Log { id: String },
    /// Run the scheduler in the foreground until interrupted (--once runs due entries and exits).
    Run {
        #[arg(long)]
        once: bool,
    },
}

#[derive(Subcommand)]
enum SkillCommand {
    /// List installed skills, enablement and locations.
    List,
    /// Browse compatible skills in common local harness directories (does not import).
    Sources,
    /// Enable an installed skill (project override by default).
    Enable {
        name: String,
        #[arg(long)]
        user: bool,
    },
    /// Disable a skill without deleting it (project override by default).
    Disable {
        name: String,
        #[arg(long)]
        user: bool,
    },
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
    /// Install a skill package from a Git URL (`url[@ref][#path]`) or a configured registry (`<registry>/<skill>[@ref]`).
    Install {
        spec: String,
        #[arg(long)]
        user: bool,
    },
    /// Update one installed package, or all of them, to the latest commit of its locked source.
    Update { name: Option<String> },
    /// Remove an installed skill package and its stored files.
    Remove { name: String },
    /// List installed skill packages and where they came from.
    Installed,
    /// Search the configured skill registries (no query lists everything).
    Search { query: Vec<String> },
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

/// Maps a CLI subcommand's exit code onto the process: 0 is success, anything else exits with it.
fn exit_with(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        flush_or_exit(code)
    }
}

fn overlay(cli: &Cli) -> Result<toml::Table> {
    let mut table = toml::Table::new();
    if let Some(model) = &cli.model {
        table.insert("model".into(), model.clone().into());
    }
    if let Some(url) = &cli.endpoint {
        table.insert("provider".into(), "endpoint".into());
        let mut endpoint = toml::Table::new();
        endpoint.insert("base_url".into(), url.clone().into());
        if let Some(var) = &cli.api_key_env {
            endpoint.insert("api_key_env".into(), var.clone().into());
        }
        table.insert("endpoint".into(), endpoint.into());
    }
    let approval = if cli.yes {
        Some("auto".to_owned())
    } else if let Some(mode) = &cli.approval_mode {
        if cli.approval.is_some() {
            anyhow::bail!("use either --approval or --approval-mode, not both");
        }
        Some(
            dial::cli::cli_modes::approval_alias(mode)
                .map_err(anyhow::Error::msg)?
                .to_owned(),
        )
    } else {
        cli.approval.clone()
    };
    if let Some(a) = approval {
        table.insert("approval".into(), a.into());
    }
    if let Some(mode) = &cli.sandbox {
        let mut sandbox = toml::Table::new();
        sandbox.insert("mode".into(), mode.clone().into());
        table.insert("sandbox".into(), sandbox.into());
    }
    if let Some(mode) = &cli.edit_mode {
        let mut edit = toml::Table::new();
        edit.insert("mode".into(), mode.clone().into());
        table.insert("edit".into(), edit.into());
    }
    if let Some(profile) = &cli.profile {
        table.insert("model_profile".into(), profile.clone().into());
    }
    if cli.compaction_strategy.is_some()
        || cli.compaction_threshold_tokens.is_some()
        || cli.compaction_reserve_tokens.is_some()
    {
        let mut compaction = toml::Table::new();
        if let Some(strategy) = &cli.compaction_strategy {
            compaction.insert("strategy".into(), strategy.clone().into());
        }
        for (key, input, max) in [
            (
                "threshold_tokens",
                &cli.compaction_threshold_tokens,
                9_007_199_254_740_991u64,
            ),
            (
                "reserve_tokens",
                &cli.compaction_reserve_tokens,
                u64::from(u32::MAX),
            ),
        ] {
            if let Some(input) = input {
                let value = if input == "auto" {
                    toml::Value::String("auto".into())
                } else {
                    let tokens = input.parse::<u64>().map_err(|_| {
                        anyhow::anyhow!("compaction.{key} requires a positive integer or auto")
                    })?;
                    anyhow::ensure!(
                        tokens > 0 && tokens <= max,
                        "compaction.{key} must be in 1..={max} or auto"
                    );
                    toml::Value::Integer(tokens as i64)
                };
                compaction.insert(key.into(), value);
            }
        }
        table.insert("compaction".into(), compaction.into());
    }
    Ok(table)
}

fn main() {
    dial::install_tls_provider();
    // Re-exec target for isolated document parsing; never reaches the CLI parser.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == dial::tools::documents::DOCUMENT_WORKER_ARG)
    {
        dial::tools::documents::document_worker_main();
    }
    // Re-exec target for the built-in shell (`shell.backend = "embedded"`); never reaches the CLI parser.
    #[cfg(feature = "embedded-shell")]
    {
        let mut args = std::env::args_os().skip(1);
        if args
            .next()
            .is_some_and(|a| a == dial::embedded_shell::HOST_ARG)
        {
            let script = args
                .next()
                .map(std::path::PathBuf::from)
                .unwrap_or_default();
            let state = args.next().map(std::path::PathBuf::from);
            std::process::exit(dial::embedded_shell::run_host_with_state(
                &script,
                state.as_deref(),
            ));
        }
    }
    let cli = Cli::parse();
    if cli.build_info {
        println!(
            "{}",
            serde_json::to_string_pretty(&dial::build_info::report()).unwrap()
        );
        flush_or_exit(0);
    }
    let format = dial::cli::cli_modes::resolve(cli.mode.as_deref(), cli.no_ui)
        .ok()
        .flatten()
        .map_or(cli.output_format, |mode| mode.output);
    let started = std::time::Instant::now();
    if let Err(error) = real_main(cli) {
        if let Some(exit) = error.downcast_ref::<PrintExit>() {
            flush_or_exit(exit.0);
        }
        eprintln!("error: {error:#}");
        if format != OutputFormat::Text {
            let mut ui = PrintUi::new(format, Vec::new());
            let _ = ui.finish(None, Some(&format!("{error:#}")), None, started.elapsed());
        }
        flush_or_exit(1);
    }
    flush_or_exit(0);
}

/// Drains buffered stdout (bounded) so final output is never lost, then exits with `code`.
fn flush_or_exit(code: i32) -> ! {
    dial::telemetry::shutdown();
    if let Err(error) = dial::flush_stdout_on_exit() {
        eprintln!("error: stdout flush failed: {error}");
        std::process::exit(if code == 0 { 1 } else { code });
    }
    std::process::exit(code)
}

fn real_main(mut cli: Cli) -> Result<()> {
    if let Some(Command::Completions { shell }) = &cli.command {
        clap_complete::generate(*shell, &mut Cli::command(), "dial", &mut std::io::stdout());
        return Ok(());
    }
    if let Some(Command::Update { version }) = &cli.command {
        return update::run(version.as_deref());
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
            let config = Config::load(&layout, Some(overlay(&cli)?))?;
            let sessions = Session::list_newest(&layout, usize::MAX)?;
            let selected = if let Some(id) = id {
                let mut matches = sessions.iter().filter(|session| session.id.starts_with(id));
                let session = matches
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("no session matching {id:?}"))?;
                if matches.next().is_some() {
                    bail!("session prefix {id:?} is ambiguous; use a longer id");
                }
                session
            } else {
                sessions
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("there are no sessions to share"))?
            };
            // Default: the active branch. Abandoned branches and internal records stay local.
            let scope = if *whole_journal {
                dial::storage::share::Scope::WholeJournal
            } else {
                dial::storage::share::Scope::ActiveBranch
            };
            let transcript = dial::storage::share::transcript(&selected.path, scope)?;
            let encrypted = dial::storage::share::encrypt_transcript(&transcript)?;
            let link = if *gist {
                dial::storage::share::upload_gist(&encrypted)?
            } else {
                let endpoint=endpoint.clone().filter(|s| !s.is_empty())
                    .or_else(|| (!config.share.server_url.is_empty()).then(|| config.share.server_url.clone()))
                    .or_else(|| std::env::var("DIAL_SHARE_SERVER").ok())
                    .ok_or_else(|| anyhow::anyhow!("set [share].server_url or DIAL_SHARE_SERVER, pass --endpoint, or use --gist"))?;
                dial::storage::share::upload(&encrypted, &endpoint)?
            };
            println!("{link}");
            return Ok(());
        }
        Some(Command::Reliability { action }) => {
            let config = Config::load(&layout, Some(overlay(&cli)?))?;
            let (value, json) = match action {
                ReliabilityCommand::Status { json } => (
                    dial::reliability::report(&config, &serde_json::json!({})),
                    *json,
                ),
                ReliabilityCommand::Check { json } => {
                    let checked = dial::reliability::check(&config)?;
                    let mut value = dial::reliability::report(&config, &serde_json::json!({}));
                    value["measured"] = checked;
                    (value, *json)
                }
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("{}", dial::reliability::render(&value));
            }
            return Ok(());
        }
        Some(Command::Policy { prompt, json }) => {
            let config = Config::load(&layout, Some(overlay(&cli)?))?;
            let value = if let Some(prompt) = prompt {
                let task = dial::learn::task_policy::Policy::for_prompt(
                    prompt,
                    config.reasoning_guard.max_pre_action_secs,
                );
                let phase = dial::adaptive::State::begin(task.kind, prompt);
                let h = config.effective_harness(&config.harness)?;
                let selection = dial::adaptive::select(
                    &config,
                    &phase,
                    dial::adaptive::Request {
                        kind: task.kind,
                        temperature: h.temperature,
                        tokens: h.max_tokens,
                        pre_action_secs: task.initial_reasoning_secs,
                        base_profile: config.active_model_profile(),
                        recovering_output: false,
                        hermetic: false,
                    },
                );
                let mut control = serde_json::json!({"adaptive_policy":{"selection":selection,"result":{"state":"preview only; configured baseline, no model request"}}});
                control["adaptive_history"] = serde_json::json!([]);
                dial::adaptive::report(&config, &control)
            } else {
                dial::adaptive::report(&config, &serde_json::json!({}))
            };
            if *json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("{}", dial::adaptive::render(&value));
            }
            return Ok(());
        }
        Some(Command::Steering { action }) => {
            let config = Config::load(&layout, Some(overlay(&cli)?))?;
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
                    dial::steering::run(
                        &config,
                        "extract",
                        pairs,
                        Some(*layer),
                        adapter.as_deref()
                    )?
                ),
                SteeringCommand::Evaluate {
                    cases,
                    adapter,
                    pending,
                } => {
                    let path = match pending {
                        Some(name) => dial::steering::pending_path(&config, name)?,
                        None => dial::steering::vector_path(&config),
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
                    serde_json::to_string_pretty(&dial::pack::import_steering(
                        &config, pack, *replace
                    )?)?
                ),
                SteeringCommand::Promote { name, replace } => println!(
                    "{}",
                    serde_json::to_string_pretty(&dial::steering::promote(
                        &config, name, *replace
                    )?)?
                ),
            }
            return Ok(());
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
            let report = dial::storage::codebase::Codebase::open(&layout)?.understanding(
                query.as_deref().unwrap_or(""),
                &std::sync::atomic::AtomicBool::new(false),
            )?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "{}",
                    dial::storage::codebase::render_understanding(&report, usize::MAX)
                );
            }
            return Ok(());
        }
        Some(Command::Skills { action }) => {
            use dial::storage::skills::{self, Scope};
            // Package actions share the slash-command path, so the CLI and `/skills` behave identically.
            let run_package = |words: Vec<String>| -> Result<()> {
                let config = Config::load(&layout, Some(overlay(&cli)?))?;
                let cancel = std::sync::atomic::AtomicBool::new(false);
                let text = skills::management::command(
                    &layout,
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
                    println!("{}", skills::management::list(&layout)?);
                }
                SkillCommand::Sources => println!("{}", skills::management::sources(&layout)),
                SkillCommand::Enable { name, user } | SkillCommand::Disable { name, user } => {
                    let enabled = matches!(action, SkillCommand::Enable { .. });
                    skills::management::set_enabled(
                        &layout,
                        name,
                        enabled,
                        if *user { Scope::User } else { Scope::Project },
                    )?;
                    println!("{}", skills::management::list(&layout)?);
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
                        &layout,
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
                SkillCommand::Remove { name } => {
                    run_package(vec!["remove".to_owned(), name.clone()])?
                }
                SkillCommand::Installed => run_package(vec!["installed".to_owned()])?,
                SkillCommand::Search { query } => {
                    run_package(
                        std::iter::once("search".to_owned())
                            .chain(query.iter().cloned())
                            .collect(),
                    )?;
                }
            }
            return Ok(());
        }
        Some(Command::Laya { action }) => {
            use dial::agent::laya;
            match action {
                LayaCommand::Install { checkpoint } => {
                    eprintln!("Installing Dial-owned Laya runtime and checkpoint…");
                    if let Some(model) = laya::install(&layout, checkpoint.as_deref())? {
                        use dial::storage::settings_store::{self, Change, Scope};
                        settings_store::save(
                            &layout,
                            Scope::Global,
                            &[
                                Change::Set {
                                    path: "model_roles.decision.provider".into(),
                                    value: laya::PROVIDER.into(),
                                },
                                Change::Set {
                                    path: "model_roles.decision.model".into(),
                                    value: model.display().to_string().into(),
                                },
                            ],
                        )?;
                    }
                    println!(
                        "Dial Laya installed in {}. Run dial laya start, then /laya on inside Dial.",
                        layout.user_dir.join("laya").display()
                    );
                }
                LayaCommand::Start | LayaCommand::Status => {
                    let config = Config::load(&layout, Some(overlay(&cli)?))?;
                    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let health = if matches!(action, LayaCommand::Start) {
                        laya::ensure_service(&config.laya_service(), cancel, true)?
                    } else {
                        laya::health(&config.laya_service(), cancel)?
                    };
                    println!("{}", serde_json::to_string_pretty(&health)?);
                }
            }
            return Ok(());
        }
        Some(Command::Schedule { action }) => {
            use dial::schedule::{self, Entry, Store};
            let store = Store::new(&layout)?;
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
                    let config = Config::load(&layout, Some(overlay(&cli)?))?;
                    let stop = dial::repl::install_sigint();
                    schedule::run(&layout, *once, config.worker_timeout_secs, &stop)?;
                }
            }
            return Ok(());
        }
        Some(Command::Update { version }) => return update::run(version.as_deref()),
        Some(Command::Setup) => return setup::run(&layout, cli.model.as_deref()),
        Some(Command::Doctor { fix, yes }) => {
            let env = dial::setup::deps::Env::current(&layout.user_dir);
            match fix {
                None => {
                    println!("{}", dial::setup::deps::run::list_text(&env));
                    println!();
                    match Config::load(&layout, Some(overlay(&cli)?)) {
                        Ok(config) => println!("{}", dial::setup::memory::doctor_text(&config)),
                        Err(error) => {
                            println!("Memory: settings could not be read ({error:#}).")
                        }
                    }
                }
                Some(id) if id.starts_with("embeddings") => {
                    let model = dial::setup::embeddings::model_from_id(id)
                        .ok_or_else(|| anyhow::anyhow!("unknown embedding model in {id}"))?;
                    if *yes {
                        let cancel = std::sync::atomic::AtomicBool::new(false);
                        let summary =
                            dial::setup::embeddings::setup(&layout, model, &cancel, &mut |line| {
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
                Some(id) if !*yes => println!(
                    "{}\n\nRe-run with --yes to set it up.",
                    dial::setup::deps::run::preview_text(id, &env)
                ),
                Some(id) => {
                    let cancel = std::sync::atomic::AtomicBool::new(false);
                    let done = dial::setup::deps::run::install_and_save(
                        &layout,
                        id,
                        &cancel,
                        &mut |line| println!("{line}"),
                    )?;
                    println!("Set up and saved:\n{}", done.join("\n"));
                }
            }
            return Ok(());
        }
        Some(Command::Sessions { all, action }) => {
            let mut sessions = if *all {
                Session::list_all(&layout, usize::MAX)?
            } else {
                Session::list_by_activity(&layout, usize::MAX)?
            };
            // Activity order in both views: the most recently written session first.
            sessions.sort_by(|a, b| b.updated.cmp(&a.updated));
            if let Some(action) = action {
                let id = match action {
                    SessionCommand::Export { id, .. } | SessionCommand::Rename { id, .. } => id,
                };
                let mut matching = sessions.iter().filter(|s| s.id.starts_with(id));
                let info = matching
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("no session matching {id:?}"))?;
                if matching.next().is_some() {
                    bail!("session prefix {id:?} is ambiguous; use a longer id");
                }
                let mut session = match action {
                    SessionCommand::Rename { .. } => Session::open(&info.path)?,
                    SessionCommand::Export { .. } => Session::read(&info.path)?,
                };
                match action {
                    SessionCommand::Rename { title, .. } => {
                        session.rename(title)?;
                        println!("Renamed {} to {}", info.id, session.title());
                    }
                    SessionCommand::Export { path, html, .. } => {
                        let path = path.clone().unwrap_or_else(|| {
                            PathBuf::from(format!(
                                "session-{}.{}",
                                info.id,
                                if *html { "html" } else { "md" }
                            ))
                        });
                        let path = if path.is_absolute() {
                            path
                        } else {
                            layout.project_root.join(path)
                        };
                        session.export(&path, *html)?;
                        println!("{}", path.display());
                    }
                }
            } else {
                let shown = if *all {
                    sessions.len()
                } else {
                    sessions.len().min(30)
                };
                for s in sessions.iter().take(shown) {
                    println!(
                        "{}  {}  {:>4} msgs  {}{}",
                        &s.id[..s.id.len().min(8)],
                        s.started,
                        s.messages,
                        s.title,
                        if *all {
                            format!("  {}", s.project)
                        } else {
                            String::new()
                        }
                    );
                }
                if let Some(notice) = Session::listing_notice(shown, sessions.len()) {
                    eprintln!("{notice}");
                }
            }
            return Ok(());
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
            return benchmark_cli::run_assist(
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
        return benchmark_cli::run(
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
        return improve_cli::run(cmd, &layout, config);
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

fn chat(
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
                bail!("session prefix {wanted:?} is ambiguous; use a longer id");
            }
            matches.first().map(|s| s.path.clone())
        };
        match found {
            Some(path) => Some(Session::open(&path)?),
            None if wanted.is_empty() => None,
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

#[cfg(test)]
mod cli_contract_tests;
