//! Requested execution targets. When the latest admitted user request is an imperative to run
//! (`run`, `launch`, `start`, `execute` or `play`, after leading filler such as "please" or "can
//! you") and names an existing workspace path, completion needs an observed run of that path at
//! the current effect epoch (checkpointed edits, opaque shell commands and modifying extension
//! calls). Intent and path are both lexical, so the rule is deliberately narrow: without a
//! leading run verb or a named existing path there is no obligation.
//!
//! Observations come from controller-owned facts only: `ProcessOutcome`, `bg` job identity,
//! readiness observations and the shell's launch facts, never from printed output. A run that
//! stayed up or exited zero is process evidence for the path, not verification of the requested
//! behavior; completion evidence says so. Source identities captured before dispatch are compared
//! with confined reads at completion; incomplete or unavailable identities remain unknown.

use super::inputs;
use crate::config::types::VerificationInputs;
use crate::runtime::execution::{ObservationKind, ProcessOutcome};
use crate::runtime::tool_output::ToolOutput;
use crate::tools::Workspace;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const RUN_VERBS: &[&str] = &["run", "launch", "start", "execute", "play"];
/// A run verb followed by one of these words (or by a gerund such as "start editing") describes
/// how to begin work, not a program to run.
const NOT_A_RUN: &[&str] = &[
    "by", "with", "over", "from", "on", "a", "an", "review", "audit",
];
const LEADING_FILLER: &[&str] = &[
    "ok", "okay", "alright", "hey", "please", "pls", "now", "so", "then", "just", "go", "ahead",
    "and", "can", "could", "would", "will", "you", "let's", "lets", "let", "us",
];
const MAX_WORDS: usize = 256;
const MAX_SPAN_WORDS: usize = 4;
const MAX_TARGETS: usize = 4;
const MAX_JOBS: usize = 32;
const MAX_COMMAND_CHARS: usize = 200;
/// Programs that inspect, edit or move files rather than run them. A segment they lead names a
/// path without running it, as does a redirection operand.
const FILE_UTILITIES: &[&str] = &[
    "cd", "echo", "printf", "sed", "awk", "tee", "cp", "mv", "rm", "touch", "chmod", "chown",
    "mkdir", "ln", "install", "ls", "cat", "head", "tail", "grep", "rg", "find", "wc", "file",
    "stat", "diff", "git", "less", "more", "vi", "vim", "nano",
];
/// Words that precede the program they run.
const COMMAND_PREFIXES: &[&str] = &["env", "exec", "time", "nohup", "command"];
const TRIM: &[char] = &[
    '`', '\'', '"', '(', ')', '[', ']', '{', '}', '<', '>', ',', ';', ':', '!', '?',
];

/// An existing workspace path the request names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::agent) struct Target {
    /// Workspace-relative path, without a trailing separator.
    pub path: String,
    /// The span as the user wrote it, after surrounding punctuation is removed.
    pub requested: String,
    absolute: PathBuf,
    directory: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunState {
    /// A `bg` job started; its state has not been observed since.
    Started,
    Running,
    Ready,
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    TimedOut,
    Cancelled,
}

impl RunState {
    fn from_process(process: &ProcessOutcome, live: bool, ready: bool) -> Option<Self> {
        if !process.started {
            return None;
        }
        Some(if process.timed_out {
            Self::TimedOut
        } else if process.cancelled {
            Self::Cancelled
        } else if process.exit_code.is_none() && process.signal.is_none() {
            // Only a managed job can be observed alive; a foreground result always ended.
            if !live {
                return None;
            }
            if ready { Self::Ready } else { Self::Running }
        } else {
            Self::Exited {
                code: process.exit_code,
                signal: process.signal,
            }
        })
    }

    fn succeeded(self) -> bool {
        matches!(
            self,
            Self::Running
                | Self::Ready
                | Self::Exited {
                    code: Some(0),
                    signal: None
                }
        )
    }

    fn label(self) -> &'static str {
        match self {
            Self::Started => "started_unobserved",
            Self::Running => "running",
            Self::Ready => "ready",
            Self::Exited { code: Some(0), .. } => "exited_zero",
            Self::Exited { .. } => "exited_nonzero",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug)]
struct Run {
    tool: &'static str,
    command: String,
    job: Option<String>,
    /// Whether the launch allocated a terminal; `None` when the producer did not report it.
    terminal: Option<bool>,
    state: RunState,
    /// Effect epoch of the launch this run observes; a later effect makes the run stale.
    epoch: u64,
    /// Requested-source identity captured before this launch's dispatch, never on job observation.
    sources: LaunchSnapshot,
}

#[derive(Clone, Debug, Default)]
struct Record {
    latest: Option<Run>,
    launches: u32,
    terminal_launches: u32,
}

#[derive(Clone, Debug)]
struct Job {
    name: String,
    targets: Vec<String>,
    terminal: Option<bool>,
    command: String,
    /// Effect epoch when the job was launched: later observations describe this launch's code.
    epoch: u64,
    sources: LaunchSnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    NotRun,
    NotObserved,
    Stale,
    SourceChanged,
    SourceUnknown,
    Failed,
    Succeeded,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::NotRun => "not_run",
            Self::NotObserved => "started_unobserved",
            Self::Stale => "stale",
            Self::SourceChanged => "source_changed",
            Self::SourceUnknown => "source_identity_unknown",
            Self::Failed => "failed",
            Self::Succeeded => "process_succeeded",
        }
    }
}

/// Confined source identities prepared immediately before dispatching a potential launch.
/// This is a source snapshot, not proof of what a process loaded or of its behavior.
#[derive(Clone, Debug, Default)]
pub(in crate::agent) struct LaunchSnapshot {
    sources: BTreeMap<String, SourceIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SourceIdentity {
    File([u8; 32]),
    Directory(inputs::Snapshot),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceState {
    Current,
    Changed,
    Unknown,
}

impl SourceState {
    fn label(self) -> &'static str {
        match self {
            Self::Current => "matches_pre_dispatch",
            Self::Changed => "changed_since_pre_dispatch",
            Self::Unknown => "unknown",
        }
    }
}

/// Per-turn requested targets and the runs observed for them.
#[derive(Clone, Debug, Default)]
pub(in crate::agent) struct Targets {
    requested: Vec<Target>,
    records: BTreeMap<String, Record>,
    jobs: Vec<Job>,
    /// Effect epoch at the previous observed call.
    last_epoch: u64,
    /// Effects of calls that only drive a tracked job (`input`, `resize`, `kill`, ...). They do
    /// not change the launched code, so they never make that job's run stale.
    own_effects: u64,
}

/// True when the request begins (after filler) with an imperative run verb.
pub(in crate::agent) fn execution_requested(text: &str) -> bool {
    let mut words = text
        .split_whitespace()
        .take(MAX_WORDS)
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'')
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .skip_while(|word| LEADING_FILLER.contains(&word.as_str()));
    let Some(verb) = words.next() else {
        return false;
    };
    RUN_VERBS.contains(&verb.as_str())
        && words.next().is_none_or(|next| {
            !NOT_A_RUN.contains(&next.as_str()) && !(next.len() > 4 && next.ends_with("ing"))
        })
}

/// Existing workspace paths the text names. A span of up to four words counts when it contains a
/// separator or a file extension, so `local testing/` resolves while plain prose does not.
pub(in crate::agent) fn requested_paths(workspace: &Workspace, text: &str) -> Vec<Target> {
    let words: Vec<&str> = text.split_whitespace().take(MAX_WORDS).collect();
    let mut targets: Vec<Target> = Vec::new();
    let mut index = 0;
    while index < words.len() && targets.len() < MAX_TARGETS {
        let longest = MAX_SPAN_WORDS.min(words.len() - index);
        let found = (1..=longest).rev().find_map(|len| {
            resolve_span(workspace, &words[index..index + len].join(" "))
                .map(|target| (len, target))
        });
        match found {
            Some((len, target)) => {
                if !targets.iter().any(|known| known.path == target.path) {
                    targets.push(target);
                }
                index += len;
            }
            None => index += 1,
        }
    }
    targets
}

fn resolve_span(workspace: &Workspace, span: &str) -> Option<Target> {
    let candidate = span
        .trim_start_matches(TRIM)
        .trim_end_matches(|c: char| TRIM.contains(&c) || c == '.');
    if candidate.is_empty() || candidate.starts_with('@') {
        return None;
    }
    let path_like = candidate.contains('/')
        || Path::new(candidate).extension().is_some_and(|extension| {
            let extension = extension.to_string_lossy();
            extension.len() <= 8 && extension.chars().all(|c| c.is_ascii_alphanumeric())
        });
    if !path_like {
        return None;
    }
    let absolute = workspace.resolve(candidate).ok()?;
    if !absolute.exists() {
        return None;
    }
    let path = workspace.rel(&absolute);
    if path == "." {
        return None;
    }
    Some(Target {
        path,
        requested: candidate.to_owned(),
        directory: absolute.is_dir(),
        absolute,
    })
}

/// `needle` occurs in `haystack` delimited as a path or argument, not inside a longer name.
fn mentions(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let before = |c: char| c.is_whitespace() || "\"'`/=(;&|".contains(c);
    let after = |c: char| c.is_whitespace() || "\"'`/;&|)".contains(c);
    haystack.match_indices(needle).any(|(start, _)| {
        let end = start + needle.len();
        haystack[..start].chars().next_back().is_none_or(before)
            && haystack[end..].chars().next().is_none_or(after)
    })
}

/// Drops unquoted redirections and their operands: writing to a path does not run it.
fn without_redirections(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    let mut quote: Option<char> = None;
    let mut chars = segment.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(open) => {
                if c == open {
                    quote = None;
                }
                out.push(c);
            }
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                out.push(c);
            }
            None if c == '<' || c == '>' => {
                while chars
                    .next_if(|next| matches!(next, '<' | '>' | '&' | '|'))
                    .is_some()
                {}
                while chars.next_if(|next| next.is_whitespace()).is_some() {}
                let mut operand_quote: Option<char> = None;
                while let Some(&next) = chars.peek() {
                    match operand_quote {
                        Some(open) if next == open => operand_quote = None,
                        Some(_) => {}
                        None if next.is_whitespace() => break,
                        None if next == '\'' || next == '"' => operand_quote = Some(next),
                        None => {}
                    }
                    chars.next();
                }
                out.push(' ');
            }
            None => out.push(c),
        }
    }
    out
}

/// The program a simple command segment runs, after assignments and prefixes such as `env`.
fn program(segment: &str) -> Option<&str> {
    segment
        .split_whitespace()
        .map(|word| word.trim_matches(|c| c == '"' || c == '\'' || c == '('))
        .find(|word| !word.is_empty() && !word.contains('=') && !COMMAND_PREFIXES.contains(word))
        .map(|word| word.rsplit('/').next().unwrap_or(word))
}

impl Target {
    fn source_identity(&self, workspace: &Workspace) -> Option<SourceIdentity> {
        if self.directory {
            // Reuse verification's complete-input manifest: sorted relative paths and file
            // content, including ignored/hidden members, all confined through Workspace. It
            // rejects symlinks, unsupported entries and incomplete traversal rather than
            // manufacturing a digest for a partial tree (4096 entries, 64 MiB, 250 ms).
            let paths = vec![self.path.clone()];
            return inputs::capture(
                workspace,
                &VerificationInputs {
                    paths,
                    environment: vec![],
                },
            )
            .map(SourceIdentity::Directory);
        }
        workspace
            .read_identity(self.absolute.to_str()?)
            .map(SourceIdentity::File)
    }

    /// Whether a command run from `cwd` runs something at this path. Each `;`/`&&`/newline
    /// segment is read separately: `cd DIR` moves the working directory for later segments,
    /// redirection operands and segments led by a file utility or a plain inspection do not count.
    /// A counted segment names the path directly, by absolute path, from inside the directory,
    /// or (for a file) by its name from its directory.
    fn referenced(&self, workspace: &Workspace, command: &str, cwd: Option<&str>) -> bool {
        let Ok(mut cwd) = cwd.map_or(Ok(workspace.root().to_path_buf()), |dir| {
            workspace.resolve(dir)
        }) else {
            return false;
        };
        let segments = crate::agent::safety::approval::top_level_segments(command)
            .unwrap_or_else(|| vec![command]);
        for segment in segments {
            let text = without_redirections(segment).replace("\\ ", " ");
            match program(&text) {
                None => continue,
                Some("cd") => {
                    let Some(dir) = text.split_whitespace().nth(1) else {
                        continue;
                    };
                    let dir = text[text.find(dir).unwrap_or(0)..]
                        .trim()
                        .trim_matches(|c| c == '"' || c == '\'');
                    let joined = workspace.rel(&cwd.join(dir));
                    match workspace.resolve(&joined) {
                        Ok(next) => cwd = next,
                        Err(_) => return false,
                    }
                }
                Some(name)
                    if FILE_UTILITIES.contains(&name)
                        || crate::agent::safety::approval::safe_bash(segment) => {}
                Some(_) => {
                    if self.named_in(workspace, &text, &cwd) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn named_in(&self, workspace: &Workspace, text: &str, cwd: &Path) -> bool {
        if mentions(text, &self.path) || mentions(text, &self.absolute.to_string_lossy()) {
            return true;
        }
        if self.directory {
            return cwd.starts_with(&self.absolute);
        }
        let (Some(parent), Some(name)) = (self.absolute.parent(), self.absolute.file_name()) else {
            return false;
        };
        let parent_named = workspace.rel(parent);
        (cwd == parent || (parent_named != "." && mentions(text, &parent_named)))
            && mentions(text, &name.to_string_lossy())
    }
}

fn readiness_satisfied(output: &ToolOutput) -> bool {
    output
        .execution
        .observation
        .as_ref()
        .is_some_and(|observation| {
            observation.kind == ObservationKind::Readiness && observation.satisfied
        })
}

fn clip(command: &str) -> String {
    command.chars().take(MAX_COMMAND_CHARS).collect()
}

impl Targets {
    /// Obligations for one admitted user request; empty unless it asks to run a named path.
    pub(in crate::agent) fn resolve(workspace: &Workspace, request: &str) -> Self {
        if !execution_requested(request) {
            return Self::default();
        }
        Self {
            requested: requested_paths(workspace, request),
            ..Self::default()
        }
    }

    /// A steering message that itself asks to run named paths replaces the obligations, keeping
    /// runs already observed for paths it still names. Any other steering keeps them: only an
    /// explicit new run request redefines what must run.
    pub(in crate::agent) fn retarget(&mut self, workspace: &Workspace, request: &str) {
        let next = Self::resolve(workspace, request);
        if next.requested.is_empty() {
            return;
        }
        let named = |path: &String| next.requested.iter().any(|target| &target.path == path);
        self.records.retain(|path, _| named(path));
        for job in &mut self.jobs {
            job.targets.retain(|path| named(path));
        }
        self.jobs.retain(|job| !job.targets.is_empty());
        self.requested = next.requested;
    }

    /// Whether the latest request named any path to run.
    pub(in crate::agent) fn has_obligations(&self) -> bool {
        !self.requested.is_empty()
    }

    fn matching(&self, workspace: &Workspace, command: &str, cwd: Option<&str>) -> Vec<String> {
        self.requested
            .iter()
            .filter(|target| target.referenced(workspace, command, cwd))
            .map(|target| target.path.clone())
            .collect()
    }

    /// Capture requested-source content before dispatch, not after a process has run. Only the
    /// existing narrow command/path inference selects candidates. Unavailable or incomplete
    /// identities are intentionally absent; they must not acquire a fabricated identity.
    pub(in crate::agent) fn prepare(
        &self,
        workspace: &Workspace,
        tool: &str,
        args: &Value,
    ) -> LaunchSnapshot {
        if !(tool == "bash" || tool == "bg" && args["action"] == "start") {
            return LaunchSnapshot::default();
        }
        let Some(command) = args["command"].as_str() else {
            return LaunchSnapshot::default();
        };
        LaunchSnapshot {
            sources: self
                .requested
                .iter()
                .filter(|target| target.referenced(workspace, command, args["cwd"].as_str()))
                .filter_map(|target| {
                    target
                        .source_identity(workspace)
                        .map(|digest| (target.path.clone(), digest))
                })
                .collect(),
        }
    }

    /// Record a finished `bash` or `bg` call whose command names a requested path, or a later
    /// `bg wait`/`output` of a job that does. Refused or unstarted calls record nothing.
    pub(in crate::agent) fn observe(
        &mut self,
        workspace: &Workspace,
        tool: &str,
        args: &Value,
        output: &ToolOutput,
        epoch: u64,
        sources: LaunchSnapshot,
    ) {
        // Effects of driving a tracked job are attributed to that job, not to new code.
        let advanced = epoch.saturating_sub(self.last_epoch);
        self.last_epoch = epoch;
        let action = args["action"].as_str();
        if tool == "bg"
            && matches!(action, Some("input" | "close_stdin" | "resize" | "kill"))
            && args["name"]
                .as_str()
                .is_some_and(|name| self.jobs.iter().any(|job| job.name == name))
        {
            self.own_effects += advanced;
        }
        if self.requested.is_empty() {
            return;
        }
        let epoch = epoch.saturating_sub(self.own_effects);
        match (tool, action) {
            ("bash", _) => self.observe_shell(workspace, args, output, epoch, sources),
            ("bg", Some("start")) => {
                self.observe_start(workspace, "bg", args, output, epoch, sources)
            }
            ("bg", Some("wait" | "output")) => self.observe_job(args, output),
            _ => {}
        }
    }

    fn observe_shell(
        &mut self,
        workspace: &Workspace,
        args: &Value,
        output: &ToolOutput,
        epoch: u64,
        sources: LaunchSnapshot,
    ) {
        let Some(command) = args["command"].as_str() else {
            return;
        };
        // A plain inspection (`ls`, `cat`, `grep`) names a path without running it.
        if !crate::agent::safety::approval::opaque_shell_effect(command) {
            return;
        }
        if args["pty"] == true {
            self.observe_start(workspace, "bash", args, output, epoch, sources);
            return;
        }
        let paths = self.matching(workspace, command, args["cwd"].as_str());
        if paths.is_empty() {
            return;
        }
        let data = output.data.as_ref();
        let job = data.and_then(|data| data["background_job"].as_str());
        let state = match job {
            // Adoption is a launch receipt, not an observation of the program's state.
            Some(_) => Some(RunState::Started),
            None => output
                .process_outcome()
                .and_then(|process| RunState::from_process(&process, false, false)),
        };
        let Some(state) = state else {
            return;
        };
        // Compatibility payloads are for clients, not authority for terminal claims.
        let terminal = output
            .execution
            .shell_launch
            .as_ref()
            .map(|launch| launch.terminal_allocated);
        if let Some(job) = job {
            self.track_job(job, &paths, terminal, command, epoch, sources.clone());
        }
        let run = Run {
            tool: "bash",
            command: clip(command),
            job: job.map(str::to_owned),
            terminal,
            state,
            epoch,
            sources,
        };
        self.record(&paths, run, true);
    }

    fn observe_start(
        &mut self,
        workspace: &Workspace,
        tool: &'static str,
        args: &Value,
        output: &ToolOutput,
        epoch: u64,
        sources: LaunchSnapshot,
    ) {
        let (Some(command), Some(job)) = (
            args["command"].as_str(),
            output.data.as_ref().map(|data| &data["job"]),
        ) else {
            return;
        };
        let Some(name) = job["name"].as_str() else {
            return;
        };
        let paths = self.matching(workspace, command, args["cwd"].as_str());
        if paths.is_empty() {
            return;
        }
        let terminal = output
            .execution
            .shell_launch
            .as_ref()
            .map(|launch| launch.terminal_allocated);
        let state = output
            .process_outcome()
            .and_then(|process| RunState::from_process(&process, true, readiness_satisfied(output)))
            .unwrap_or(RunState::Started);
        self.track_job(name, &paths, terminal, command, epoch, sources.clone());
        let run = Run {
            tool,
            command: clip(command),
            job: Some(name.to_owned()),
            terminal,
            state,
            epoch,
            sources,
        };
        self.record(&paths, run, true);
    }

    fn observe_job(&mut self, args: &Value, output: &ToolOutput) {
        let Some(name) = args["name"].as_str() else {
            return;
        };
        let Some(job) = self.jobs.iter().find(|job| job.name == name).cloned() else {
            return;
        };
        let Some(state) = output.process_outcome().and_then(|process| {
            RunState::from_process(&process, true, readiness_satisfied(output))
        }) else {
            return;
        };
        let run = Run {
            tool: "bg",
            command: clip(&job.command),
            job: Some(job.name.clone()),
            terminal: job.terminal,
            state,
            epoch: job.epoch,
            sources: job.sources.clone(),
        };
        self.record(&job.targets, run, false);
    }

    fn track_job(
        &mut self,
        name: &str,
        paths: &[String],
        terminal: Option<bool>,
        command: &str,
        epoch: u64,
        sources: LaunchSnapshot,
    ) {
        // A reused name refers to the newest job.
        self.jobs.retain(|job| job.name != name);
        if self.jobs.len() >= MAX_JOBS {
            self.jobs.remove(0);
        }
        self.jobs.push(Job {
            name: name.to_owned(),
            targets: paths.to_vec(),
            terminal,
            command: command.to_owned(),
            epoch,
            sources,
        });
    }

    fn record(&mut self, paths: &[String], run: Run, launch: bool) {
        for path in paths {
            let record = self.records.entry(path.clone()).or_default();
            if launch {
                record.launches += 1;
                if run.terminal == Some(true) {
                    record.terminal_launches += 1;
                }
            }
            record.latest = Some(run.clone());
        }
    }

    fn source_state(&self, workspace: &Workspace, target: &Target) -> SourceState {
        let launch = self
            .records
            .get(&target.path)
            .and_then(|record| record.latest.as_ref())
            .and_then(|run| run.sources.sources.get(&target.path));
        let Some(launch) = launch else {
            return SourceState::Unknown;
        };
        match target.source_identity(workspace) {
            Some(current) if *launch == current => SourceState::Current,
            Some(_) => SourceState::Changed,
            None => SourceState::Unknown,
        }
    }

    fn status(&self, target: &Target, epoch: u64, source: SourceState) -> Status {
        let Some(run) = self
            .records
            .get(&target.path)
            .and_then(|record| record.latest.as_ref())
        else {
            return Status::NotRun;
        };
        if run.epoch != epoch {
            Status::Stale
        } else if run.state == RunState::Started {
            Status::NotObserved
        } else if run.state.succeeded() {
            match source {
                SourceState::Current => Status::Succeeded,
                SourceState::Changed => Status::SourceChanged,
                SourceState::Unknown => Status::SourceUnknown,
            }
        } else {
            Status::Failed
        }
    }

    fn describe(&self, target: &Target, status: Status, terminal_launch_available: bool) -> String {
        let record = self.records.get(&target.path);
        let run = record.and_then(|record| record.latest.as_ref());
        let detail = match (status, run) {
            (Status::NotObserved, Some(run)) => format!(
                "`bg` job `{}` was started but its state was never observed",
                run.job.as_deref().unwrap_or("?")
            ),
            (Status::Stale, _) => {
                "its last run launched before a later edit or effectful command".into()
            }
            (Status::SourceChanged, _) => {
                "its source changed since the pre-dispatch launch snapshot; observing the old job cannot verify the current source".into()
            }
            (Status::SourceUnknown, _) => {
                "a successful process was observed, but its source identity is unknown (unavailable or incomplete source snapshot, or no pre-dispatch snapshot)".into()
            }
            (Status::Failed, Some(run)) => {
                let ended = match run.state {
                    RunState::Exited {
                        code: Some(code), ..
                    } => format!("exited with code {code}"),
                    RunState::Exited {
                        signal: Some(signal),
                        ..
                    } => format!("was terminated by signal {signal}"),
                    RunState::TimedOut => "timed out".into(),
                    RunState::Cancelled => "was cancelled".into(),
                    _ => "ended without a success status".into(),
                };
                let launch = match run.terminal {
                    Some(true) => "; launched in a terminal",
                    Some(false) => "; launched without a terminal",
                    None => "",
                };
                let mut detail = format!(
                    "its latest run (`{}`: `{}`) {ended}{launch}",
                    run.tool, run.command
                );
                if self.untried_terminal(target, terminal_launch_available) {
                    detail.push_str(". A terminal-backed launch (`bg` start with `pty: true`) is available and was not attempted for this path");
                }
                detail
            }
            _ => {
                "no run naming this path was observed (commands that only inspect it do not count)"
                    .into()
            }
        };
        format!("- `{}`: {detail}.", target.requested)
    }

    /// The latest run of `target` launched without a terminal while an untried terminal launch is
    /// available, so its failure may say more about the launch than about the program.
    fn untried_terminal(&self, target: &Target, terminal_launch_available: bool) -> bool {
        let record = self.records.get(&target.path);
        terminal_launch_available
            && record.is_some_and(|record| {
                record.terminal_launches == 0
                    && record
                        .latest
                        .as_ref()
                        .is_some_and(|run| run.terminal == Some(false))
            })
    }

    /// Requested paths whose latest run failed at the current epoch with no untried terminal
    /// launch: the failure itself is the observation, which is what a request to run a path and
    /// explain its failure asks for. Described as `blocker` would describe them.
    pub(in crate::agent) fn conclusive_failures(
        &self,
        workspace: &Workspace,
        epoch: u64,
        terminal_launch_available: bool,
    ) -> Vec<String> {
        let epoch = epoch.saturating_sub(self.own_effects);
        self.requested
            .iter()
            .filter(|target| {
                self.status(target, epoch, self.source_state(workspace, target)) == Status::Failed
                    && !self.untried_terminal(target, terminal_launch_available)
            })
            .map(|target| self.describe(target, Status::Failed, terminal_launch_available))
            .collect()
    }

    /// Why completion is refused, or `None` when every requested path has a current successful run.
    /// With `accept_failures`, a current conclusive failed run (see [`Self::conclusive_failures`])
    /// answers the request instead of blocking it.
    pub(in crate::agent) fn blocker(
        &self,
        workspace: &Workspace,
        epoch: u64,
        terminal_launch_available: bool,
        accept_failures: bool,
    ) -> Option<String> {
        let epoch = epoch.saturating_sub(self.own_effects);
        let open: Vec<String> = self
            .requested
            .iter()
            .filter_map(|target| {
                let source = self.source_state(workspace, target);
                match self.status(target, epoch, source) {
                    Status::Succeeded => None,
                    Status::Failed
                        if accept_failures
                            && !self.untried_terminal(target, terminal_launch_available) =>
                    {
                        None
                    }
                    status => Some(self.describe(target, status, terminal_launch_available)),
                }
            })
            .collect();
        if open.is_empty() {
            return None;
        }
        Some(format!(
            "The latest request asks to run a named path, and no current successful run of it was observed this turn:\n{}\nRun the requested path, or report what prevents it as a blocker.",
            open.join("\n")
        ))
    }

    /// Completion-evidence record, or `Null` when the request named nothing to run.
    pub(in crate::agent) fn evidence(&self, workspace: &Workspace, epoch: u64) -> Value {
        if self.requested.is_empty() {
            return Value::Null;
        }
        let epoch = epoch.saturating_sub(self.own_effects);
        let targets: Vec<Value> = self
            .requested
            .iter()
            .map(|target| {
                let source = self.source_state(workspace, target);
                let record = self.records.get(&target.path).cloned().unwrap_or_default();
                let latest = record.latest.as_ref().map(|run| {
                    let (exit_code, signal) = match run.state {
                        RunState::Exited { code, signal } => (code, signal),
                        _ => (None, None),
                    };
                    json!({
                        "tool": run.tool, "command": run.command, "job": run.job,
                        "terminal": run.terminal, "state": run.state.label(),
                        "exit_code": exit_code, "signal": signal,
                        "current": run.epoch == epoch && source == SourceState::Current,
                        "source_identity": source.label(),
                        "source_snapshot_boundary": run.sources.sources.contains_key(&target.path).then_some("pre_dispatch"),
                        "behavior_verified": false,
                    })
                });
                json!({
                    "path": target.path,
                    "requested": target.requested,
                    "kind": if target.directory { "directory" } else { "file" },
                    "status": self.status(target, epoch, source).label(),
                    "launches": record.launches,
                    "terminal_launches": record.terminal_launches,
                    "latest": latest,
                })
            })
            .collect();
        json!({
            "intent": "lexical_run_request",
            "targets": targets,
            "behavior": "not_assessed",
            "behavior_verified": false,
            "note": "A run that started, stayed running or exited zero is process evidence for the named path, not verification of the requested behavior.",
        })
    }
}

#[cfg(test)]
mod tests;
