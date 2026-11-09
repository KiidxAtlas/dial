use crate::cmd;
use clap::builder::Styles;
use clap::builder::styling::{AnsiColor, Effects};
use clap::{Parser, Subcommand};
use dial::cli::help_heading as section;
use dial::repl::OutputFormat;
use std::path::PathBuf;

/// Help colours follow the TUI's default accent (green headings, cyan literals). Clap drops them
/// for pipes and `NO_COLOR`.
const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default())
    .valid(AnsiColor::Green.on_default())
    .invalid(AnsiColor::Yellow.on_default().effects(Effects::BOLD))
    .error(AnsiColor::Red.on_default().effects(Effects::BOLD));

/// Task-first examples shown under `dial --help`, so the common entry points are visible before
/// the full flag reference.
fn root_after_help() -> String {
    let h = HELP_STYLES.get_header();
    let l = HELP_STYLES.get_literal();
    let examples = [
        ("dial", "Open the interactive terminal UI"),
        (
            "dial \"fix the failing test\"",
            "Start a session with this request",
        ),
        ("dial -c", "Continue the latest session in this project"),
        (
            "dial -p \"summarize README.md\"",
            "Answer once, print the result and exit",
        ),
        (
            "dial sessions",
            "List recent sessions (resume one with dial -r <id>)",
        ),
        (
            "dial setup -m <model>",
            "Prepare a local model and save it as the default",
        ),
        (
            "dial doctor",
            "Check optional capabilities and how to install them",
        ),
    ];
    let width = examples.iter().map(|(cmd, _)| cmd.len()).max().unwrap_or(0);
    let mut out = format!("{h}Examples:{h:#}\n");
    for (cmd, what) in examples {
        out.push_str(&format!("  {l}{cmd:<width$}{l:#}  {what}\n"));
    }
    out.push_str(&format!(
        "\n{h}Print mode exit codes:{h:#} 0 completed · 1 error (execution, provider or configuration)\n  \
         · 2 incomplete, blocked, needs input, no answer or interrupted · 3 tool approval denied.\n  \
         Approve named tools with {l}--allow write,bash{l:#}; {l}--yes{l:#} approves every tool.\n\
         --output-schema FILE: an answer that still fails the schema after its corrections exits 2.\n\
         \nInside the TUI, press {l}/{l:#} for commands or {l}/help{l:#} for the action palette."
    ));
    out
}

/// The subcommand a lone prompt word was probably meant to be. Sending `dial sesions` to the
/// model would start a whole agent turn for a typo, so a single bare word within one edit (two
/// for longer words) of a subcommand or alias is refused. `dial -- <word>` still sends it.
pub(crate) fn mistyped_subcommand<S: AsRef<std::ffi::OsStr>>(
    cli: &Cli,
    raw_args: impl IntoIterator<Item = S>,
) -> Option<String> {
    use clap::CommandFactory;
    let [word] = cli.prompt.as_slice() else {
        return None;
    };
    let len = word.chars().count();
    if cli.command.is_some()
        || cli.print
        || len < 4
        || !word.chars().all(|c| c.is_ascii_lowercase() || c == '-')
        || raw_args.into_iter().any(|arg| arg.as_ref() == "--")
    {
        return None;
    }
    let limit = if len <= 5 { 1 } else { 2 };
    let command = Cli::command();
    command
        .get_subcommands()
        .flat_map(|sub| {
            std::iter::once(sub.get_name())
                .chain(sub.get_all_aliases())
                .map(move |candidate| (edit_distance(word, candidate), sub.get_name()))
        })
        .filter(|&(distance, _)| distance <= limit)
        .min_by_key(|&(distance, _)| distance)
        .map(|(_, name)| name.to_owned())
}

/// Clap's own "unrecognized subcommand" error, with the suggestion and the way to send the word
/// as a request instead.
pub(crate) fn mistyped_subcommand_error(word: &str, suggestion: &str) -> clap::Error {
    use clap::CommandFactory;
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    let mut command = Cli::command();
    let usage = command.render_usage();
    let mut error = clap::Error::new(ErrorKind::InvalidSubcommand).with_cmd(&command);
    error.insert(
        ContextKind::InvalidSubcommand,
        ContextValue::String(word.to_owned()),
    );
    error.insert(
        ContextKind::SuggestedSubcommand,
        ContextValue::String(suggestion.to_owned()),
    );
    error.insert(
        ContextKind::Suggested,
        ContextValue::StyledStrs(vec![
            format!("to send '{word}' to the model as a request, use 'dial -- {word}'").into(),
        ]),
    );
    error.insert(ContextKind::Usage, ContextValue::StyledStr(usage));
    error
}

/// Optimal-string-alignment distance: Levenshtein plus adjacent transpositions (`modles`).
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut rows = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in rows[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (rows[i - 1][j] + 1)
                .min(rows[i][j - 1] + 1)
                .min(rows[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(rows[i - 2][j - 2] + 1);
            }
            rows[i][j] = best;
        }
    }
    rows[a.len()][b.len()]
}

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

#[derive(Subcommand)]
pub(crate) enum AuthCommand {
    /// Manage the stored accounts of an OAuth provider (the id shown in /providers).
    Accounts {
        provider: String,
        #[command(subcommand)]
        action: Option<AccountCommand>,
    },
}

#[derive(Subcommand)]
pub(crate) enum AccountCommand {
    /// List accounts; the active one is marked `*`. Also the default when no action is given.
    List {
        /// Print the accounts as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Make a stored account the active one.
    Select {
        /// Account key shown by `list`.
        key: String,
    },
    /// Remove one stored account; the grant file is deleted with the last one.
    Remove {
        /// Account key shown by `list`.
        key: String,
    },
}

#[derive(Parser)]
#[command(
    version = dial::build_info::VERSION,
    about = "A local-first coding agent with optional gated learning",
    styles = HELP_STYLES,
    after_help = root_after_help(),
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
    /// Message to send (starts a one-shot run with --print, otherwise an interactive session that opens with it).
    pub(crate) prompt: Vec<String>,
    /// Run non-interactively: answer once, print the result, exit.
    #[arg(short, long, help_heading = section::RUN)]
    pub(crate) print: bool,
    /// Stdout format for a non-interactive turn: text, one JSON result, or JSON events.
    #[arg(long, value_enum, default_value = "text", help_heading = section::RUN)]
    pub(crate) output_format: OutputFormat,
    /// Pre-approve these named tools in print mode (comma-separated); does not override readonly.
    #[arg(long, value_delimiter = ',', help_heading = section::RUN)]
    pub(crate) allow: Vec<String>,
    /// Print mode: the final answer must be JSON that validates against this JSON Schema file. A
    /// rejected answer gets bounded correction turns; the validated JSON becomes the answer.
    #[arg(long, value_name = "FILE", help_heading = section::RUN)]
    pub(crate) output_schema: Option<std::path::PathBuf>,
    /// Print mode: also write the final answer to FILE, atomically. No answer leaves FILE untouched.
    #[arg(short = 'o', long = "output-last-message", value_name = "FILE", help_heading = section::RUN)]
    pub(crate) output_last_message: Option<std::path::PathBuf>,
    /// Attach a PNG, JPEG, GIF or WebP image to the print turn (repeatable; vision models only).
    /// Pass `-` to read the image bytes from stdin (format detected by magic bytes; stdin is then
    /// not read as prompt text).
    #[arg(long = "image", value_name = "PATH", help_heading = section::RUN)]
    pub(crate) images: Vec<std::path::PathBuf>,
    /// Continue the most recent session in this project.
    #[arg(short = 'c', long = "continue", conflicts_with_all = ["resume", "no_session"], help_heading = section::SESSIONS)]
    pub(crate) continue_session: bool,
    /// Resume a session by id prefix (latest when no id is given). `dial sessions` lists ids.
    #[arg(short, long, alias = "session", num_args = 0..=1, default_missing_value = "", conflicts_with = "no_session", help_heading = section::SESSIONS)]
    pub(crate) resume: Option<String>,
    /// Restore interrupted workers of the session being continued, leaving the editor unsubmitted.
    /// Without a resume prefix this continues the most recent session in this project.
    #[arg(long, conflicts_with_all = ["print", "mode", "no_session"], help_heading = section::SESSIONS)]
    pub(crate) restore: bool,
    /// Do not write the session to disk.
    #[arg(long, help_heading = section::SESSIONS)]
    pub(crate) no_session: bool,
    /// Work in this directory instead of the current one.
    #[arg(short = 'C', long = "dir", alias = "cwd", global = true)]
    pub(crate) dir: Option<PathBuf>,
    /// Model: an MLX model id or local path (or the model name on an --endpoint).
    #[arg(short, long, global = true, help_heading = section::MODEL)]
    pub(crate) model: Option<String>,
    /// Select an explicitly configured model capability/reasoning profile.
    #[arg(long, global = true, help_heading = section::MODEL)]
    pub(crate) profile: Option<String>,
    /// Use an OpenAI-compatible server (LM Studio, Ollama, llama.cpp, hosted API) at this base URL.
    #[arg(long, global = true, help_heading = section::MODEL)]
    pub(crate) endpoint: Option<String>,
    /// Environment variable holding the endpoint's API key.
    #[arg(long, global = true, help_heading = section::MODEL)]
    pub(crate) api_key_env: Option<String>,
    /// Tool approval: ask (before every edit and command), write (default: destructive or
    /// outside-the-project actions ask), auto (never asks) or readonly.
    #[arg(long, global = true, help_heading = section::SAFETY, value_parser = clap::builder::PossibleValuesParser::new(dial::config::types::Approval::NAMES.iter().copied()))]
    pub(crate) approval: Option<String>,
    /// Approve every tool call (same as --approval auto; also accepted as --yolo / --auto-approve).
    #[arg(short = 'y', long, global = true, aliases = ["auto-approve", "yolo"], help_heading = section::SAFETY)]
    pub(crate) yes: bool,
    /// Compatibility alias for --approval.
    #[arg(long = "approval-mode", global = true, value_name = "MODE", help_heading = section::SAFETY, value_parser = clap::builder::PossibleValuesParser::new(dial::cli::cli_modes::APPROVAL_ALIASES.iter().map(|&(alias, _)| alias)))]
    pub(crate) approval_mode: Option<String>,
    /// Print mode (text): also write the model's reasoning to stdout before the answer.
    #[arg(long, global = true, help_heading = section::RUN)]
    pub(crate) print_thoughts: bool,
    /// Shell isolation: dev (default: writes only the project, hides secrets), strict (workspace
    /// only, no network) or off.
    #[arg(long, global = true, help_heading = section::SAFETY, value_parser = ["off", "dev", "strict"])]
    pub(crate) sandbox: Option<String>,
    /// How the model writes edits.
    #[arg(long, global = true, help_heading = section::CONTEXT, value_parser = ["apply_patch", "hashline", "patch", "replace", "sloppy"])]
    pub(crate) edit_mode: Option<String>,
    /// Automatic context policy when the window fills.
    #[arg(long, global = true, help_heading = section::CONTEXT, value_parser = ["context-full", "handoff", "shake", "snapcompact", "off"])]
    pub(crate) compaction_strategy: Option<String>,
    /// Fixed automatic compaction trigger; use auto to clear the saved token trigger.
    #[arg(long, global = true, help_heading = section::CONTEXT)]
    pub(crate) compaction_threshold_tokens: Option<String>,
    /// Positive context/output reserve; use auto to clear the saved reserve.
    #[arg(long, global = true, help_heading = section::CONTEXT)]
    pub(crate) compaction_reserve_tokens: Option<String>,
    #[command(flatten)]
    pub(crate) model_flags: dial::cli::model_flags::ModelFlags,
    #[command(flatten)]
    pub(crate) tool_flags: dial::cli::tool_flags::ToolFlagArgs,
    #[command(flatten)]
    pub(crate) mode_args: dial::cli::mode_flags::ModeArgs,
    #[command(flatten)]
    pub(crate) session_flags: dial::cli::session_flags::SessionFlagArgs,
    /// Show build revision, build-time dirty state, target and actual executable path, then exit.
    #[arg(long, global = true, help_heading = section::PROTOCOL)]
    pub(crate) build_info: bool,
    /// Protocol mode: text | json | rpc | rpc-ui | acp (overrides --output-format and the default path).
    #[arg(long, global = true, value_name = "MODE", help_heading = section::PROTOCOL)]
    pub(crate) mode: Option<String>,
    /// RPC modes only: run without a UI surface (requires --mode rpc or rpc-ui).
    #[arg(long, global = true, help_heading = section::PROTOCOL)]
    pub(crate) no_ui: bool,
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

/// Weight inspection and evaluation-gated weight edits of the configured local MLX model.
#[derive(Subcommand)]
pub(crate) enum WeightsCommand {
    /// Show the stored result of every inspection and any running job.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Checkpoint layout and parameter counts from safetensors headers (no model load).
    Map {
        #[arg(long)]
        json: bool,
    },
    /// Probe the model with built-in text and record what each layer, attention head and neuron does.
    Analyze {
        #[arg(long)]
        json: bool,
    },
    /// Give the model a goal in plain words; it runs the whole weight-editing loop itself.
    Improve {
        /// What to change, e.g. "stop endless repetition in long lists".
        #[arg(required = true, num_args = 1..)]
        goal: Vec<String>,
    },
    /// Per-tensor and per-layer weight statistics (no forward pass).
    Stats {
        /// Measure only tensors whose name starts with this prefix.
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Per-layer activation contrast for matched response pairs.
    Activations {
        /// JSON array of {positive, negative} message lists sharing a prompt (1–64 pairs).
        #[arg(long)]
        pairs: PathBuf,
        #[arg(long)]
        adapter: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Answer-loss change with each layer skipped, or each head of one layer removed.
    Trace {
        /// JSON array of {messages} ending in the assistant answer to score (1–16 cases).
        #[arg(long)]
        cases: PathBuf,
        /// Remove each attention head of this zero-based layer instead of skipping layers.
        #[arg(long)]
        heads: Option<u64>,
        /// Comma-separated zero-based layers to skip (default: all).
        #[arg(long, value_delimiter = ',', conflicts_with = "heads")]
        layers: Vec<u64>,
        #[arg(long)]
        adapter: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Create an experiment with a forward-pass budget, or show an existing one.
    Experiment {
        name: String,
        /// Create the experiment with this many model runs (1–256).
        #[arg(long)]
        budget: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Greedy answers per case: tokens, token-limit hits, repetition and response checks.
    Generate {
        /// Experiment charged one model run.
        #[arg(long)]
        experiment: String,
        /// JSON array of {messages, required?, forbidden?} ending in a user turn (1–32 cases).
        #[arg(long)]
        cases: PathBuf,
        /// Token limit per answer (default 512).
        #[arg(long)]
        max_tokens: Option<u64>,
        /// Show the last N characters of each answer (0–400).
        #[arg(long)]
        tail: Option<u64>,
        #[arg(long)]
        adapter: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Measure and save a per-layer direction between unwanted and wanted answers.
    Direction {
        /// Name of the saved direction.
        name: String,
        /// Experiment charged one model run.
        #[arg(long)]
        experiment: String,
        /// JSON array of {messages} ending in a wanted assistant answer.
        #[arg(long)]
        positive: PathBuf,
        /// JSON array of {messages} ending in an unwanted assistant answer.
        #[arg(long)]
        negative: PathBuf,
        /// Measure MLP outputs (`mlp`, default) or the residual stream (`residual`).
        #[arg(long)]
        site: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Score an edit in memory at one or more strengths, without writing a model copy.
    Try {
        /// Experiment charged one model run.
        #[arg(long)]
        experiment: String,
        /// JSON array of {messages} ending in the assistant answer the edit should change.
        #[arg(long)]
        cases: PathBuf,
        /// JSON array of ordinary {messages} answers the edit must not disturb (up to 8).
        #[arg(long)]
        neutral: Option<PathBuf>,
        /// Comma-separated edit strengths in (0, 2] (up to 5; default 1).
        #[arg(long, value_delimiter = ',')]
        doses: Vec<f64>,
        /// Direction to project out.
        #[arg(long, required_unless_present = "changes", conflicts_with = "changes")]
        direction: Option<String>,
        /// Comma-separated zero-based layers to edit (1–16).
        #[arg(long, value_delimiter = ',', required_unless_present = "changes")]
        layers: Vec<u64>,
        /// Scale tensors, heads or neurons directly: JSON array of changes.
        #[arg(long)]
        changes: Option<PathBuf>,
        #[arg(long)]
        columns: Option<u64>,
        #[arg(long)]
        alpha: Option<f64>,
        /// Accept a quantized edit whose requantization error exceeds half the edit.
        #[arg(long)]
        allow_noisy: bool,
        #[arg(long)]
        json: bool,
    },
    /// Rank attention heads and MLP neurons by how strongly they support the given answers.
    Attribute {
        /// Experiment charged one model run.
        #[arg(long)]
        experiment: String,
        /// JSON array of {messages} ending in the (unwanted) assistant answer to explain.
        #[arg(long)]
        cases: PathBuf,
        /// JSON array of {messages} ending in the wanted answer; ranks components by the difference.
        #[arg(long)]
        wanted: Option<PathBuf>,
        /// JSON array of {messages} ending in ordinary answers the components should not matter for.
        #[arg(long)]
        neutral: Option<PathBuf>,
        /// Score every answer token (`answer`, default) or only tokens that repeat an earlier 3-token sequence (`repeats`).
        #[arg(long)]
        focus: Option<String>,
        /// Components listed per kind (1–100, default 20).
        #[arg(long)]
        top: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Write a new model copy with a direction projected out of chosen layers' MLP outputs, or with exact tensors scaled.
    Edit {
        /// Name of the edited copy.
        name: String,
        /// Experiment the edit belongs to (the direction's, when one is used).
        #[arg(long)]
        experiment: String,
        /// Direction to project out.
        #[arg(long, required_unless_present_any = ["changes", "bundle"], conflicts_with_all = ["changes", "bundle"])]
        direction: Option<String>,
        /// Comma-separated zero-based layers to edit (1–16).
        #[arg(long, value_delimiter = ',', required_unless_present_any = ["changes", "bundle"])]
        layers: Vec<u64>,
        /// Fold a steering bundle (from `dial steering`) into the weights instead: its path.
        #[arg(long, conflicts_with = "changes", requires_all = ["data", "strength"])]
        bundle: Option<PathBuf>,
        /// Steering: the records the bundle was measured on.
        #[arg(long)]
        data: Option<PathBuf>,
        /// Steering: strength to fold in.
        #[arg(long)]
        strength: Option<f64>,
        /// Scale exact tensors directly: JSON array of {tensor, scale, rows?, columns?}.
        #[arg(long)]
        changes: Option<PathBuf>,
        /// down_proj output columns edited per layer (default 64).
        #[arg(long)]
        columns: Option<u64>,
        /// Fraction of the direction removed, in (0, 2] (default 1).
        #[arg(long)]
        alpha: Option<f64>,
        #[arg(long)]
        json: bool,
    },
    /// Compare an edited copy with its base on held-out cases the experiment never saw.
    Evaluate {
        /// The edited copy.
        edit: String,
        /// JSON array of {messages, required?, forbidden?} ending in a user turn (8–32 cases).
        #[arg(long)]
        cases: PathBuf,
        /// Token limit per answer (default 512).
        #[arg(long)]
        max_tokens: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Make an edited copy that passed its evaluation this project's model.
    Promote { edit: String },
    /// Restore the base model of the promoted edit.
    Revert,
    /// Serve the loopback dashboard and open it in the browser.
    Open {
        /// Port on 127.0.0.1 (default: an ephemeral port).
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// List kept weights runs, newest first.
    History {
        /// Only runs of this operation (for example `analyze` or `evaluate`).
        #[arg(long)]
        op: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show the full result of a kept run.
    Show {
        /// Run id from `dial weights history`.
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Compare two kept `analyze` runs layer by layer (for example base and edited copy).
    Compare {
        before: String,
        after: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove all but the newest kept runs.
    Prune {
        /// Runs to keep.
        #[arg(long, default_value_t = 50)]
        keep: usize,
    },
    /// Sum 2–4 edited copies of one experiment into a new edited copy (evaluate it before promoting).
    Combine {
        /// Name of the combined copy.
        name: String,
        /// Comma-separated edits to sum.
        #[arg(long, value_delimiter = ',', required = true)]
        edits: Vec<String>,
        /// Accept a combination whose requantization error exceeds half the summed edits.
        #[arg(long)]
        allow_noisy: bool,
        #[arg(long)]
        json: bool,
    },
    /// Build generation cases from a past session's user turns (for generate or evaluate).
    Cases {
        /// Session id or unique prefix (`dial sessions`).
        #[arg(long)]
        session: String,
        /// Newest user turns kept (1–32).
        #[arg(long, default_value_t = 16)]
        limit: usize,
        /// Earlier messages kept before each turn.
        #[arg(long, default_value_t = 4)]
        context: usize,
        /// Write the cases here instead of printing them.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Suggest the next step of the weight-editing loop from what has been run so far.
    Next,
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
    // Declaration order is `dial --help` order: everyday work first, then integrations,
    // inspection and maintenance, and the learning/experimental tooling last.
    /// List recent sessions in this project; export, rename, pack or unpack one.
    #[command(alias = "session")]
    Sessions {
        /// Include sessions from every project.
        #[arg(long, global = true)]
        all: bool,
        #[command(subcommand)]
        action: Option<SessionCommand>,
    },
    /// Create the Python/MLX environment, save the model chosen with -m as your default, verify it loads.
    Setup,
    /// Explicit paid native API batches (never runs automatically).
    ProviderBatch(dial::cli::provider_batches::Args),
    /// Review changes read-only, like /review in the TUI: --uncommitted (default), --base, --commit or a prompt.
    Review(dial::cli::review::Args),
    /// List and inspect models; `recommend` checks which local models fit in memory.
    Models(dial::cli::config_models::ModelsArgs),
    /// Show, read, or change Dial configuration.
    Config(dial::cli::config_models::ConfigArgs),
    /// Check optional capabilities (language servers, voice, memory, MCP helpers) and fix them.
    ///
    /// `--fix <id>` previews the install; add `--yes` to run it.
    Doctor {
        /// Capability to set up, e.g. `bun`.
        #[arg(long)]
        fix: Option<String>,
        /// Run the install after previewing it.
        #[arg(long, requires = "fix")]
        yes: bool,
    },
    /// Manage the stored sign-ins of an OAuth provider: list accounts, select one, or remove one.
    Auth {
        #[command(subcommand)]
        action: AuthCommand,
    },
    /// Generate a commit message for the working tree and commit it.
    #[command(trailing_var_arg = true, allow_hyphen_values = true)]
    Commit {
        /// Same flags as `/commit`: --push, --dry-run, --legacy, --no-changelog, -c <context>, -m <model>.
        #[arg(num_args = 0.., allow_hyphen_values = true)]
        args: Vec<String>,
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
    /// Search the web from the command line.
    #[command(alias = "q", alias = "web-search")]
    Search(dial::cli::search::SearchArgs),
    /// Follow live edits, diagnostics and screenshots in another pane.
    Watch(dial::cli::watch::WatchArgs),
    /// Run a foreground command and expose bounded output to approved monitors by name.
    Run(dial::cli::run::RunArgs),
    /// Recurring or file-triggered prompt runs for this project.
    Schedule {
        #[command(subcommand)]
        action: ScheduleCommand,
    },
    /// Install skill packages from Git or registries, or import SKILL.md folders from other tools.
    Skills {
        #[command(subcommand)]
        action: SkillCommand,
    },
    /// Verify and install the latest signed release, or a specified release version.
    Update {
        /// Version number or v-prefixed release tag (defaults to latest).
        #[arg(long)]
        version: Option<String>,
    },
    /// Generate shell completions from the live command and argument definitions.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Persistent JSON-lines agent control protocol on stdin/stdout.
    Rpc,
    /// ACP v1 editor integration over stdio JSON-RPC 2.0 (one launched project).
    Acp,
    /// Install the Chrome extension, run the browser relay, or check its connection.
    BrowserRelay(dial::cli::browser_relay::BrowserRelayArgs),
    /// Run a local WebSocket collaboration relay.
    Relay {
        #[arg(long, default_value = "127.0.0.1:7331")]
        bind: std::net::SocketAddr,
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
    /// List and manage background processes.
    Ps(dial::cli::ps::PsArgs),
    /// Image commands: status, doctor, probe, purge.
    #[command(visible_alias = "img")]
    Images(dial::cli::images::ImagesArgs),
    /// Inspect saved project understanding without loading a model or refreshing the index.
    Understanding {
        /// Use architecture for the architecture view, or filter findings, paths and outlines.
        query: Option<String>,
        /// Export every matching entry as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Inspect project experience, reusable recipes and observed utility without loading a model.
    Experience {
        #[arg(long)]
        json: bool,
    },
    /// Record or list human feedback tied to immutable artifact content.
    Annotate(dial::cli::annotate::AnnotateArgs),
    /// Developer and research tools: benchmarks, learning, MLX weights, diagnostics.
    ///
    /// Benchmarks, the gated self-improvement loop, MLX weights and steering, Laya, policy and
    /// reliability inspection, stream rules, session anonymizing and garbage collection.
    Dev {
        #[command(subcommand)]
        command: DevCommand,
    },
    /// The `dev` commands also parse at the top level (`dial weights …`) so existing scripts keep
    /// working; [`command`] hides them from the root help and completions.
    #[command(flatten)]
    Developer(DevCommand),
}

impl Command {
    /// `dial dev <cmd>` and the hidden top-level `dial <cmd>` are the same command; dispatch sees
    /// only [`Command::Developer`].
    pub(crate) fn normalized(self) -> Self {
        match self {
            Command::Dev { command } => Command::Developer(command),
            other => other,
        }
    }
}

/// The root command for parsing, help and completions: the top-level copies of the `dev`
/// subcommands are hidden, so `dial --help` lists everyday commands and the `dev` group.
pub(crate) fn command() -> clap::Command {
    use clap::{CommandFactory, Subcommand};
    let names: Vec<String> = DevCommand::augment_subcommands(clap::Command::new("dev"))
        .get_subcommands()
        .map(|sub| sub.get_name().to_owned())
        .collect();
    names.iter().fold(Cli::command(), |root, name| {
        root.mut_subcommand(name, |sub| sub.hide(true))
    })
}

/// Parse the process arguments through [`command`], exiting with clap's message on error.
/// [`crate::dispatch::real_main`] folds `dev` into [`Command::Developer`].
pub(crate) fn parse() -> Cli {
    use clap::FromArgMatches;
    let matches = command().get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.format(&mut command()).exit())
}

#[derive(Subcommand)]
pub(crate) enum DevCommand {
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
    /// Run the deterministic, model-free edit-grammar benchmark (exits 1 on any wrong result).
    EditBench(dial::cli::edit_bench::EditBenchArgs),
    /// The gated self-improvement loop (evaluation suites, harvesting, LoRA training, rollback).
    #[command(subcommand)]
    Improve(cmd::improve::Cmd),
    /// Extract, evaluate or inspect experimental local MLX activation steering.
    Steering {
        #[command(subcommand)]
        action: SteeringCommand,
    },
    /// Inspect the local MLX model's weights and activations (read-only), with a dashboard.
    Weights {
        #[command(subcommand)]
        action: WeightsCommand,
    },
    /// Install, start or inspect Dial's independent Laya decision service.
    Laya {
        #[command(subcommand)]
        action: LayaCommand,
    },
    /// Inspect adaptive controls, or preview a task policy without loading a model.
    Policy {
        prompt: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Inspect tool reliability or run a bounded backend capability check.
    Reliability {
        #[command(subcommand)]
        action: ReliabilityCommand,
    },
    /// List the stream rules this workspace loads, or test which ones a sample output fires.
    Rules(dial::cli::rules::RulesArgs),
    /// Write an anonymized copy of a session (redacted content, metadata kept).
    Anonymize(dial::cli::anonymize::AnonymizeArgs),
    /// Sweep stale blobs, archives, and WAL state (dry run unless --apply).
    Gc(dial::cli::gc::GcArgs),
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
    /// Pack a session and its side history, stream journal, subagent transcripts and referenced
    /// artifacts into one verified, portable zip archive.
    Pack {
        id: String,
        /// Archive to write (default: session-<id>.dialpack in the project root). Never replaces an
        /// existing file.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Unpack a session archive into this project. Refuses anything that would overwrite an
    /// existing file, and records the transfer so the original path forks if reopened.
    Unpack { file: PathBuf },
}

#[cfg(test)]
mod tests;
