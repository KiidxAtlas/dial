use crate::cmd;
use clap::{Parser, Subcommand};
use dial::repl::OutputFormat;
use std::path::PathBuf;

#[derive(Subcommand)]
pub(crate) enum LayaCommand {
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
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
    /// Show build revision, build-time dirty state, target and actual executable path, then exit.
    #[arg(long, global = true)]
    pub(crate) build_info: bool,
    /// Message to send (starts a one-shot run with --print, otherwise an interactive session that opens with it).
    pub(crate) prompt: Vec<String>,
    /// Run non-interactively: answer once, print the result, exit.
    #[arg(short, long)]
    pub(crate) print: bool,
    /// Stdout format for a non-interactive turn: text, one JSON result, or JSON events.
    #[arg(long, value_enum, default_value = "text")]
    pub(crate) output_format: OutputFormat,
    /// Pre-approve these named tools in print mode (comma-separated); does not override readonly.
    #[arg(long, value_delimiter = ',')]
    pub(crate) allow: Vec<String>,
    /// Continue the most recent session in this project.
    #[arg(short = 'c', long = "continue", conflicts_with_all = ["resume", "no_session"])]
    pub(crate) continue_session: bool,
    /// Resume a session by id prefix (latest when no id is given).
    #[arg(short, long, alias = "session", num_args = 0..=1, default_missing_value = "", conflicts_with = "no_session")]
    pub(crate) resume: Option<String>,
    /// Do not write the session to disk.
    #[arg(long)]
    pub(crate) no_session: bool,
    /// Work in this directory instead of the current one.
    #[arg(short = 'C', long = "dir", alias = "cwd", global = true)]
    pub(crate) dir: Option<PathBuf>,
    /// Model: an MLX model id or local path (or the model name on an --endpoint).
    #[arg(short, long, global = true)]
    pub(crate) model: Option<String>,
    /// Select an explicitly configured model capability/reasoning profile.
    #[arg(long, global = true)]
    pub(crate) profile: Option<String>,
    /// Use an OpenAI-compatible server (LM Studio, Ollama, llama.cpp, hosted API) at this base URL.
    #[arg(long, global = true)]
    pub(crate) endpoint: Option<String>,
    /// Environment variable holding the endpoint's API key.
    #[arg(long, global = true)]
    pub(crate) api_key_env: Option<String>,
    /// Tool approval policy (default: write).
    #[arg(long, global = true, value_parser = clap::builder::PossibleValuesParser::new(dial::config::types::Approval::NAMES.iter().copied()))]
    pub(crate) approval: Option<String>,
    /// Approve every tool call (same as --approval auto; also accepted as --yolo / --auto-approve).
    #[arg(short = 'y', long, global = true, aliases = ["auto-approve", "yolo"])]
    pub(crate) yes: bool,
    /// Compatibility alias for --approval.
    #[arg(long = "approval-mode", global = true, value_name = "MODE", value_parser = clap::builder::PossibleValuesParser::new(dial::cli::cli_modes::APPROVAL_ALIASES.iter().map(|&(alias, _)| alias)))]
    pub(crate) approval_mode: Option<String>,
    /// Protocol mode: text | json | rpc | rpc-ui | acp (overrides --output-format and the default path).
    #[arg(long, global = true, value_name = "MODE")]
    pub(crate) mode: Option<String>,
    /// RPC modes only: run without a UI surface (requires --mode rpc or rpc-ui).
    #[arg(long, global = true)]
    pub(crate) no_ui: bool,
    /// Print mode (text): also write the model's reasoning to stdout before the answer.
    #[arg(long, global = true)]
    pub(crate) print_thoughts: bool,
    /// off | dev | strict
    #[arg(long, global = true)]
    pub(crate) sandbox: Option<String>,
    /// apply_patch | hashline | patch | replace | sloppy
    #[arg(long, global = true, value_parser = ["apply_patch", "hashline", "patch", "replace", "sloppy"])]
    pub(crate) edit_mode: Option<String>,
    /// Automatic context policy: context-full | handoff | shake | snapcompact | off.
    #[arg(long, global = true, value_parser = ["context-full", "handoff", "shake", "snapcompact", "off"])]
    pub(crate) compaction_strategy: Option<String>,
    /// Fixed automatic compaction trigger; use auto to clear the saved token trigger.
    #[arg(long, global = true)]
    pub(crate) compaction_threshold_tokens: Option<String>,
    /// Positive context/output reserve; use auto to clear the saved reserve.
    #[arg(long, global = true)]
    pub(crate) compaction_reserve_tokens: Option<String>,
    #[command(flatten)]
    pub(crate) model_flags: dial::cli::model_flags::ModelFlags,
    #[command(flatten)]
    pub(crate) tool_flags: dial::cli::tool_flags::ToolFlagArgs,
    #[command(flatten)]
    pub(crate) mode_args: dial::cli::mode_flags::ModeArgs,
    #[command(flatten)]
    pub(crate) session_flags: dial::cli::session_flags::SessionFlagArgs,
}

#[derive(Subcommand)]
pub(crate) enum SteeringCommand {
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
pub(crate) enum ReliabilityCommand {
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
pub(crate) enum Command {
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
    /// Follow live edits, diagnostics and screenshots in another pane.
    Watch(dial::cli::watch::WatchArgs),
    /// Sweep stale blobs, archives, and WAL state (dry run unless --apply).
    Gc(dial::cli::gc::GcArgs),
    /// Run the deterministic, model-free edit-grammar benchmark (exits 1 on any wrong result).
    EditBench(dial::cli::edit_bench::EditBenchArgs),
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
    Improve(cmd::improve::Cmd),
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
        /// Share the original subscription authentication store and refresh locks; never copy tokens.
        #[arg(long)]
        share_auth: bool,
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
pub(crate) enum ScheduleCommand {
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
pub(crate) enum SkillCommand {
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
pub(crate) enum SessionCommand {
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

#[cfg(test)]
mod tests;
