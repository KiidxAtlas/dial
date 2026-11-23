//! Conservative native resource waves. Unknown effects are barriers, never model-declared leases.
use crate::agent::*;

pub(in crate::agent) use crate::tools::surface::resources::Footprint;

/// Bookkeeping is settled on the controller in call order after native work drains. Deliverable
/// scope changes can ask the human, so they remain boundaries before later source effects.
pub(in crate::agent) fn bookkeeping(call: &ToolCall) -> bool {
    if call.name != "todo" {
        return false;
    }
    crate::agent::workflow::todo::state_call(&call.arguments).is_none_or(|state| {
        state["action"] == "view"
            || (state["action"] == "patch" && state.get("deliverables").is_none())
    })
}

pub(in crate::agent) fn footprint_for(
    workspace: &Workspace,
    mode: crate::config::types::EditMode,
    registry: &crate::tools::integrations::ext::Registry,
    call: &ToolCall,
) -> Option<Footprint> {
    if bookkeeping(call) {
        return Some(Footprint::default());
    }
    // A plain inspection of literal workspace files reads only those files, so it can join a wave
    // beside writes to other paths. Every other command that writes nothing (still approved like
    // any shell command) reads every root, so it joins reads but never runs beside a write.
    if call.name == "bash" {
        if crate::agent::safety::approval::parallel_read(registry, "bash", &call.arguments) {
            return literal_shell_reads(workspace, &call.arguments)
                .or_else(|| crate::tools::surface::resources::workspace_read(workspace));
        }
        return read_only_shell_batch(&call.arguments)
            .then(|| crate::tools::surface::resources::workspace_read(workspace))
            .flatten();
    }
    crate::tools::surface::resources::resolve(
        workspace,
        mode,
        registry,
        &call.name,
        &call.arguments,
    )
}

/// A one-shot shell call (no `session`, `pty` or `background`) whose every command the effect
/// tracker proves writes nothing. Scheduling only: approval still decides whether it runs.
fn read_only_shell_batch(arguments: &Value) -> bool {
    let off = |key| {
        matches!(
            arguments.get(key),
            None | Some(Value::Null | Value::Bool(false))
        )
    };
    off("background")
        && off("pty")
        && arguments["session"].is_null()
        && arguments["command"]
            .as_str()
            .is_some_and(|command| !crate::agent::safety::approval::opaque_shell_effect(command))
}

/// Options a literal read accepts for one program. Names are exact: no abbreviations, bundled short
/// options, or unlisted `--name=value` forms. An option outside this list may take a file operand,
/// so it keeps the broad read. A flag takes no value; a valued option takes the next word, or
/// `--name=value` for a long name. Where GNU and BSD differ, a name appears only where the value
/// shape agrees, so a mismatch can only add operands, never hide one.
struct ReadOptions {
    flags: &'static [&'static str],
    valued: &'static [&'static str],
}

fn read_options(program: &str) -> Option<ReadOptions> {
    Some(match program {
        "cat" => ReadOptions {
            flags: &[
                "-A",
                "-b",
                "-e",
                "-E",
                "-n",
                "-s",
                "-t",
                "-T",
                "-u",
                "-v",
                "--number",
                "--number-nonblank",
                "--show-all",
                "--show-ends",
                "--show-nonprinting",
                "--show-tabs",
                "--squeeze-blank",
            ],
            valued: &[],
        },
        "wc" => ReadOptions {
            flags: &[
                "-c",
                "-l",
                "-L",
                "-m",
                "-w",
                "--bytes",
                "--chars",
                "--lines",
                "--max-line-length",
                "--words",
            ],
            valued: &[],
        },
        "head" => ReadOptions {
            flags: &["-q", "-v", "--quiet", "--silent", "--verbose"],
            valued: &["-c", "-n", "--bytes", "--lines"],
        },
        "tail" => ReadOptions {
            flags: &[
                "-f",
                "-F",
                "-q",
                "-r",
                "-v",
                "--quiet",
                "--silent",
                "--verbose",
            ],
            valued: &["-c", "-n", "--bytes", "--lines"],
        },
        // GNU `-f` is --file-system (boolean) and BSD `-f` takes a format; both read as a flag,
        // so a BSD format becomes an extra operand that falls back to the broad read.
        "stat" => ReadOptions {
            flags: &[
                "-f",
                "-L",
                "-t",
                "--dereference",
                "--file-system",
                "--terse",
            ],
            valued: &["-c", "--format", "--printf"],
        },
        // GNU `-L` dereferences symlinks; `-H` follows symlinked operands. Both stay unlisted.
        "ls" => ReadOptions {
            flags: &[
                "-1",
                "-a",
                "-A",
                "-c",
                "-C",
                "-d",
                "-F",
                "-g",
                "-h",
                "-i",
                "-l",
                "-m",
                "-n",
                "-o",
                "-p",
                "-r",
                "-R",
                "-s",
                "-S",
                "-t",
                "-u",
                "-x",
                "--all",
                "--almost-all",
                "--classify",
                "--directory",
                "--human-readable",
                "--recursive",
                "--reverse",
            ],
            valued: &[],
        },
        // `-f FILE` reads a pattern file and `-R` follows symlinks below directories (GNU), so both
        // stay unlisted. `-r` follows no symlink it meets while recursing, in GNU and BSD alike.
        "grep" => ReadOptions {
            flags: &[
                "-c",
                "-E",
                "-F",
                "-G",
                "-h",
                "-H",
                "-i",
                "-I",
                "-l",
                "-L",
                "-n",
                "-o",
                "-q",
                "-r",
                "-s",
                "-v",
                "-w",
                "-x",
                "--count",
                "--extended-regexp",
                "--files-with-matches",
                "--files-without-match",
                "--fixed-strings",
                "--ignore-case",
                "--invert-match",
                "--line-number",
                "--line-regexp",
                "--no-filename",
                "--no-messages",
                "--only-matching",
                "--quiet",
                "--recursive",
                "--silent",
                "--with-filename",
                "--word-regexp",
            ],
            // BSD takes `-C`/`--context` with an optional attached value, so `-C 3` would make `3` the
            // pattern there; both stay unlisted.
            valued: &[
                "-A",
                "-B",
                "-e",
                "-m",
                "--after-context",
                "--before-context",
                "--exclude",
                "--exclude-dir",
                "--include",
                "--max-count",
                "--regexp",
            ],
        },
        _ => return None,
    })
}

/// The directory relative operands resolve against: the workspace root, or the `cwd` argument when
/// it is an existing directory whose path has no symlink and no outside component, as the native
/// resource rules admit it. Other `cwd` values keep the broad read.
fn read_base_directory(workspace: &Workspace, arguments: &Value) -> Option<std::path::PathBuf> {
    let Some(cwd) = arguments.get("cwd").filter(|cwd| !cwd.is_null()) else {
        return Some(workspace.root().to_path_buf());
    };
    let cwd = std::path::Path::new(cwd.as_str()?);
    let lexical = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        workspace.root().join(cwd)
    };
    if !lexical.is_dir() {
        return None;
    }
    // Validation only: the directory itself is not read, and the lexical path is judged so that a
    // symlink component (which the shell would follow) is refused, not silently resolved.
    crate::tools::surface::resources::literal_reads(workspace, std::slice::from_ref(&lexical))?;
    Some(lexical)
}

/// Options whose value is the search pattern, and whether the first operand is the pattern when
/// none of them appears. A pattern is the program's argument, never a file it reads.
fn pattern_options(program: &str) -> (&'static [&'static str], bool) {
    match program {
        "grep" => (&["-e", "--regexp"], true),
        _ => (&[], false),
    }
}

/// A word that may name a literal workspace file: no stdin marker, handle or shell-special byte.
/// Option values and patterns reach the program verbatim and are not file names, so they skip this.
fn literal_operand(word: &str) -> bool {
    !(word.is_empty()
        || word == "-"
        || word.starts_with('@')
        || word.contains(['$', '*', '?', '[', ']', '{', '}', '~', '`', '\\']))
}

/// `find` predicates that read nothing beyond the starting points (`-newer FILE`, `-samefile`,
/// `-L`, `-follow` and `-xtype` would). Every other predicate keeps the broad read.
const FIND_FLAGS: &[&str] = &[
    "!", "(", ")", "-a", "-and", "-depth", "-empty", "-false", "-mount", "-not", "-o", "-or",
    "-print", "-print0", "-prune", "-true", "-xdev",
];
const FIND_VALUED: &[&str] = &[
    "-amin",
    "-atime",
    "-cmin",
    "-ctime",
    "-gid",
    "-group",
    "-iname",
    "-inum",
    "-ipath",
    "-iregex",
    "-iwholename",
    "-links",
    "-maxdepth",
    "-mindepth",
    "-mmin",
    "-mtime",
    "-name",
    "-path",
    "-perm",
    "-regex",
    "-size",
    "-type",
    "-uid",
    "-user",
    "-wholename",
];

/// Literal starting points of a `find` whose expression only filters and prints. Leading options
/// (`-H`, `-L`, `-P`) and any predicate outside the lists above return `None`.
fn find_operands(words: &[String]) -> Option<Vec<&str>> {
    let start = words
        .iter()
        .position(|word| word.starts_with('-') || matches!(word.as_str(), "!" | "(" | ")"))
        .unwrap_or(words.len());
    let (operands, expression) = words.split_at(start);
    let mut rest = expression.iter();
    while let Some(word) = rest.next() {
        if FIND_VALUED.contains(&word.as_str()) {
            rest.next()?;
        } else if !FIND_FLAGS.contains(&word.as_str()) {
            return None;
        }
    }
    Some(operands.iter().map(String::as_str).collect())
}

/// Inspections whose operands are exactly the files they read: one plain command from
/// [`read_options`] (or a filtering `find`), with literal operands. `None` keeps the broad read.
fn literal_shell_reads(workspace: &Workspace, arguments: &Value) -> Option<Footprint> {
    let command = arguments["command"].as_str()?;
    // `safe_bash` allows one plain command only. Newlines, `;`, `&&`, `\r` and `&` are lexer
    // operators, so `shell_words` returns no words for them and no second command reaches here.
    if !crate::agent::safety::approval::safe_bash(command) {
        return None;
    }
    let words = crate::agent::safety::approval::shell_words(command)?;
    let program = words.first()?.as_str();
    let base = read_base_directory(workspace, arguments)?;
    let mut operands = Vec::new();
    if program == "find" {
        for word in find_operands(&words[1..])? {
            if !literal_operand(word) {
                return None;
            }
            operands.push(base.join(word));
        }
    } else {
        let options = read_options(program)?;
        let (patterns, pattern_first) = pattern_options(program);
        let mut pattern_pending = pattern_first;
        let mut value_pending = false;
        let mut operands_only = false;
        for word in &words[1..] {
            if std::mem::take(&mut value_pending) {
                continue;
            }
            if operands_only || !word.starts_with('-') || word == "-" {
                if std::mem::take(&mut pattern_pending) {
                    continue;
                }
                if !literal_operand(word) {
                    return None;
                }
                operands.push(base.join(word));
            } else if word == "--" {
                operands_only = true;
            } else if options.flags.contains(&word.as_str()) {
                continue;
            } else if options.valued.contains(&word.as_str()) {
                value_pending = true;
                pattern_pending &= !patterns.contains(&word.as_str());
            } else {
                // Only `--name=value` for a listed valued long option is accepted here.
                let (name, _) = word.split_once('=')?;
                if !(name.starts_with("--") && options.valued.contains(&name)) {
                    return None;
                }
                pattern_pending &= !patterns.contains(&name);
            }
        }
        // A valued option without its value, or a missing pattern, reads nothing provable.
        if value_pending || pattern_pending {
            return None;
        }
    }
    // No operand at all means the working directory or stdin: keep the broad read.
    if operands.is_empty() {
        return None;
    }
    crate::tools::surface::resources::literal_reads(workspace, &operands)
}

/// A conservative prefix shared by foreground, child and recorded-call analysis.
/// Callers retain their policy owners; a rejected call always closes the prefix.
pub(in crate::agent) struct Plan {
    pub calls: Vec<ToolCall>,
    /// One resolved footprint per admitted call, in call order.
    pub footprints: Vec<Footprint>,
    pub barrier: &'static str,
    /// Tool name of the call that closed the prefix; `None` for `response_end` and
    /// `concurrency_limit`, which no particular call caused.
    pub barrier_tool: Option<String>,
}

impl Plan {
    pub(in crate::agent) fn writes(&self) -> bool {
        self.footprints.iter().any(Footprint::writes)
    }

    /// An admitted native call has no workspace footprint (an effect-free external read).
    pub(in crate::agent) fn external(&self) -> bool {
        self.calls
            .iter()
            .zip(&self.footprints)
            .any(|(call, footprint)| footprint.is_empty() && !bookkeeping(call))
    }

    /// Keep the first `admitted` calls after a controller check closed the prefix early at
    /// `reason`; the first excluded call becomes the closing call.
    pub(in crate::agent) fn close_at(&mut self, admitted: usize, reason: &'static str) {
        self.barrier_tool = self.calls.get(admitted).map(|call| call.name.clone());
        self.barrier = reason;
        self.calls.truncate(admitted);
        self.footprints.truncate(admitted);
    }

    /// Whether this prefix should run as a native wave rather than through the parallel-read
    /// group or serially: more than one native call, and at least one the read group cannot take
    /// (a declared write, a gated shell inspection, an external read, or settled bookkeeping).
    pub(in crate::agent) fn dispatchable(&self) -> bool {
        self.calls.iter().filter(|call| !bookkeeping(call)).count() > 1
            && (self.writes()
                || self.external()
                || self.calls.iter().any(bookkeeping)
                // Admitted shell reads are already one-shot; only safe_bash inspections can
                // take the parallel-read path without the native wave's authorization pass.
                || self.calls.iter().any(|call| {
                    call.name == "bash"
                        && call.arguments["command"]
                            .as_str()
                            .is_some_and(|cmd| !crate::agent::safety::approval::safe_bash(cmd))
                }))
    }
}

/// Role differences in wave admission; every other rule is shared by [`plan_wave`].
#[derive(Clone, Copy, Debug)]
pub(in crate::agent) struct WavePolicy {
    /// Bookkeeping joins the wave and settles on the controller after native work drains.
    pub bookkeeping: bool,
    /// A call carrying `verify` entries joins the wave and closes it; otherwise it is a barrier.
    pub verify_closes: bool,
    /// Barrier label for a role refusal (hooks, bookkeeping, verification, preflight).
    pub refusal: &'static str,
}

impl WavePolicy {
    /// The foreground controller settles bookkeeping and coalesced verification itself.
    pub(in crate::agent) const PARENT: Self = Self {
        bookkeeping: true,
        verify_closes: true,
        refusal: "hook_or_preflight",
    };
    /// Workers run bookkeeping and verification through their serial call path.
    pub(in crate::agent) const CHILD: Self = Self {
        bookkeeping: false,
        verify_closes: false,
        refusal: "worker_admission",
    };
}

/// Resources a wave would schedule against; hooks and the role's preflight are evaluated first.
pub(in crate::agent) struct WaveScope<'a> {
    pub workspace: &'a Workspace,
    pub mode: crate::config::types::EditMode,
    pub registry: &'a crate::tools::integrations::ext::Registry,
    pub hooks: &'a [crate::config::types::HookConfig],
    pub limit: usize,
}

/// The one wave admission for foreground, child and replay controllers. `preflight` applies the
/// role's controller checks and may return the effective (expanded) call; a refusal closes the
/// prefix. Resources are resolved from the effective call.
pub(in crate::agent) fn plan_wave(
    calls: &[ToolCall],
    scope: WaveScope<'_>,
    policy: WavePolicy,
    mut preflight: impl FnMut(&ToolCall) -> std::result::Result<ToolCall, &'static str>,
) -> Plan {
    plan_prefix(calls, scope.limit, |call| {
        let verifies = call.arguments["verify"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty());
        if (!policy.bookkeeping && bookkeeping(call))
            || (!policy.verify_closes && verifies)
            || ["pre_tool_use", "post_tool_use"].iter().any(|event| {
                crate::agent::safety::hooks::wanted_for_tool(scope.hooks, event, &call.name)
            })
        {
            return Err(policy.refusal);
        }
        let effective = preflight(call)?;
        let resources = footprint_for(scope.workspace, scope.mode, scope.registry, &effective)
            .ok_or("unknown_resources")?;
        Ok((effective, resources))
    })
}

pub(in crate::agent) fn plan_prefix(
    calls: &[ToolCall],
    limit: usize,
    mut admit: impl FnMut(&ToolCall) -> std::result::Result<(ToolCall, Footprint), &'static str>,
) -> Plan {
    let mut plan = Plan {
        calls: Vec::new(),
        footprints: Vec::new(),
        barrier: "response_end",
        barrier_tool: None,
    };
    for call in calls {
        if plan.calls.len() >= limit {
            plan.barrier = "concurrency_limit";
            break;
        }
        let (call, resources) = match admit(call) {
            Ok(admitted) => admitted,
            Err(reason) => {
                plan.barrier = reason;
                plan.barrier_tool = Some(call.name.clone());
                break;
            }
        };
        if plan
            .footprints
            .iter()
            .any(|previous| resources.conflicts(previous))
        {
            plan.barrier = "resource_conflict";
            plan.barrier_tool = Some(call.name.clone());
            break;
        }
        plan.footprints.push(resources);
        let verifies = call.arguments["verify"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty());
        if verifies {
            plan.barrier = "verification";
            plan.barrier_tool = Some(call.name.clone());
        }
        plan.calls.push(call);
        if verifies {
            break;
        }
    }
    plan
}

#[cfg(test)]
fn footprint(
    workspace: &Workspace,
    mode: crate::config::types::EditMode,
    call: &ToolCall,
) -> Option<Footprint> {
    footprint_for(
        workspace,
        mode,
        &crate::tools::integrations::ext::Registry::new(),
        call,
    )
}

/// Failed native mutations are prerequisites only for actions with overlapping or unknown
/// resources. Diagnostic reads and proven-disjoint native work remain available in this batch.
/// A failed `bg start` is likewise a prerequisite of later actions that operate on its job.
#[derive(Default)]
pub(in crate::agent) struct BatchPrerequisites {
    failed: Vec<(String, Option<Footprint>)>,
    failed_jobs: Vec<String>,
}

impl BatchPrerequisites {
    pub(in crate::agent) fn observe(
        &mut self,
        workspace: &Workspace,
        mode: crate::config::types::EditMode,
        call: &ToolCall,
        output: &ToolOutput,
        registry: &crate::tools::integrations::ext::Registry,
    ) {
        let resources = footprint_for(workspace, mode, registry, call);
        if output.is_error
            && output.execution.status != Some(tools::ToolStatus::Skipped)
            && call.name == "bg"
            && call.arguments["action"] == "start"
            && let Some(name) = call.arguments["name"].as_str()
            // A busy name means an existing job still answers to it (`bg` table refusal text).
            && !output
                .text
                .contains(&format!("named {name:?} is already running"))
        {
            self.failed_jobs.push(name.to_owned());
        }
        if output.is_error
            && output.execution.status != Some(tools::ToolStatus::Skipped)
            && (matches!(call.name.as_str(), "write" | "edit" | "ast_edit")
                || resources.as_ref().is_some_and(Footprint::writes))
        {
            self.failed.push((call.name.clone(), resources));
        }
    }

    pub(in crate::agent) fn rejection(
        &self,
        workspace: &Workspace,
        mode: crate::config::types::EditMode,
        registry: &crate::tools::integrations::ext::Registry,
        call: &ToolCall,
    ) -> Option<ToolOutput> {
        if let Some(name) = self.failed_job_target(call) {
            return Some(skipped(format!(
                "This call was not executed: `bg start` for job `{name}` failed earlier in this batch, so the job does not exist. Inspect that failure, then start the job again in the next request; completed results are preserved."
            )));
        }
        if self.failed.is_empty() {
            return None;
        }
        // Checks consume the candidate source even though their own approval gate is read-only.
        // Job observations and cancellation do not start work against that candidate.
        if (call.name == "bg"
            && matches!(
                call.arguments["action"].as_str(),
                Some("output" | "wait" | "list" | "kill")
            ))
            || (call.name != "verify"
                && !is_check_call(call)
                && crate::agent::safety::approval::is_read_only(
                    registry,
                    &call.name,
                    &call.arguments,
                ))
        {
            return None;
        }
        let resources = footprint_for(workspace, mode, registry, call);
        let (name, _) =
            self.failed
                .iter()
                .find(|(_, failed)| match (failed.as_ref(), resources.as_ref()) {
                    (Some(failed), Some(resources)) => failed.conflicts(resources),
                    (Some(failed), None) => !disjoint_shell_subtree(workspace, call, failed),
                    _ => true,
                })?;
        Some(skipped(format!(
            "This call was not executed: an earlier `{name}` mutation failed in this batch and this action has overlapping or unknown source dependencies. Inspect the failure and current source, then replan in the next request. Diagnostic reads and proven-disjoint native actions remain available; completed results are preserved."
        )))
    }

    /// The failed job a later `bg` action would operate on. `kill` stays available: it may clear
    /// a busy name that made the start fail.
    fn failed_job_target<'a>(&'a self, call: &ToolCall) -> Option<&'a str> {
        if call.name != "bg"
            || !matches!(
                call.arguments["action"].as_str(),
                Some("wait" | "output" | "input" | "close_stdin" | "resize")
            )
        {
            return None;
        }
        let name = call.arguments["name"].as_str()?;
        self.failed_jobs
            .iter()
            .find(|failed| *failed == name)
            .map(String::as_str)
    }
}

fn skipped(text: String) -> ToolOutput {
    ToolOutput::err(text)
        .with_status(tools::ToolStatus::Skipped)
        .with_effects(tools::surface::outcome::Effects::None)
        .with_result_origin(tools::surface::outcome::ResultOrigin::Skipped)
}

/// Whether a non-check shell command (`bash` or `bg start`) is explicitly scoped to a plain
/// subdirectory strictly disjoint from `failed`'s declared write footprint, with no parent
/// traversal, repo-wide git invocation or mention of the failed paths.
fn disjoint_shell_subtree(workspace: &Workspace, call: &ToolCall, failed: &Footprint) -> bool {
    if is_check_call(call) {
        return false;
    }
    let is_shell_launch = call.name == "bash"
        || (call.name == "bg" && call.arguments["action"].as_str() == Some("start"));
    if !is_shell_launch || failed.write_paths().is_empty() {
        return false;
    }
    let Some(raw_command) = call.arguments["command"].as_str() else {
        return false;
    };
    let (cwd_arg, script) = if let Some(cwd) = call.arguments["cwd"].as_str() {
        (cwd, raw_command)
    } else if let Some(rest) = raw_command.trim_start().strip_prefix("cd ")
        && let Some((target, script)) = rest.split_once(" && ")
    {
        let target = target.trim();
        if target.is_empty() || target.contains(['\'', '"', '`', '$', ';', '|']) {
            return false;
        }
        (target, script)
    } else {
        return false;
    };
    let Ok(cwd) = workspace.resolve(cwd_arg) else {
        return false;
    };
    if cwd == workspace.root() || !cwd.starts_with(workspace.root()) {
        return false;
    }
    let Ok(meta) = std::fs::symlink_metadata(&cwd) else {
        return false;
    };
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return false;
    }
    if failed.overlaps_path(&cwd) {
        return false;
    }
    if script.contains("..") || contains_word(script, "git") {
        return false;
    }
    if let Some(root) = workspace.root().to_str()
        && !root.is_empty()
        && script.contains(root)
    {
        return false;
    }
    for path in failed.write_paths() {
        let Ok(rel) = path.strip_prefix(workspace.root()) else {
            return false;
        };
        if let Some(first) = rel.components().next()
            && let Some(name) = first.as_os_str().to_str()
            && contains_path_token(script, name)
        {
            return false;
        }
        if let Some(file_name) = path.file_name().and_then(|n| n.to_str())
            && contains_path_token(script, file_name)
        {
            return false;
        }
    }
    true
}

fn contains_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .any(|token| token == word)
}

fn contains_path_token(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '/' | '\\'
                    | '\''
                    | '"'
                    | '`'
                    | ';'
                    | ','
                    | ':'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '['
                    | ']'
                    | '='
                    | '&'
                    | '|'
                    | '<'
                    | '>'
            )
    })
    .any(|token| {
        #[cfg(any(windows, target_os = "macos"))]
        {
            token.eq_ignore_ascii_case(needle)
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            token == needle
        }
    })
}

impl Agent<'_> {
    /// A maximal independent prefix, bounded by `TaskConfig::dependency_wave_limit` (at most 32).
    /// A conflicting resource or unknown effect closes the wave; nothing jumps a serial barrier.
    pub(in crate::agent) fn dependency_wave(&mut self, calls: &[ToolCall]) -> Plan {
        // Waves read the same effective (profile-overridden) config as streaming and read groups.
        let config = self.effective_config();
        let scope = WaveScope {
            workspace: &self.workspace,
            mode: self.edit_mode(),
            registry: &self.ext,
            hooks: self.tool_hooks(),
            limit: config.task.dependency_wave_limit(),
        };
        let plan = plan_wave(calls, scope, WavePolicy::PARENT, |call| {
            if self.speculative_pending(call) {
                return Err(WavePolicy::PARENT.refusal);
            }
            let mut effective = call.clone();
            crate::agent::window::file_handles::expand(&self.workspace, &mut effective)
                .map_err(|_| "file_reference")?;
            // Only serial dispatch delivers the refusal (and consumes the one-shot gate).
            if self.verification_option_disabled(&effective)
                || self.repro_first_pending(&effective, crate::config::types::ReproFirst::Gate)
            {
                return Err(WavePolicy::PARENT.refusal);
            }
            Ok(effective)
        });
        self.efficiency_metrics
            .scheduling
            .barrier(plan.barrier, plan.barrier_tool.as_deref());
        plan
    }

    /// Schema-admitted, dependency-independent native calls use the ordinary worker pump.
    pub(in crate::agent) fn dispatch_dependency_wave(
        &mut self,
        backend: &mut Backend,
        calls: &[ToolCall],
        ui: &mut dyn Ui,
        read_reuse: Option<&crate::agent::workflow::investigation::ReadReuse>,
    ) -> Vec<(ToolOutput, f64)> {
        let mut results = vec![None; calls.len()];
        let blocked = |reason: &str| {
            ToolOutput::err(reason)
                .with_status(tools::ToolStatus::Blocked)
                .with_effects(tools::surface::outcome::Effects::None)
        };
        let started = Instant::now();
        let mut ready = Vec::new();
        let mut positions = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            if bookkeeping(call) {
                continue;
            }
            if self.interrupt.load(Ordering::SeqCst) {
                results[index] = Some((crate::tools::exec::execution::before_start(), 0.0));
            } else if let Err(denial) = self.authorize_call(call, ui) {
                results[index] = Some((blocked(denial.message()), 0.0));
            } else if let Some(preview) = self.authorized_edit_preview(call) {
                // Nothing in this wave has started. Preserve the serial preview refusal boundary
                // rather than publishing any sibling effect after a failed authorized preview.
                for result in &mut results {
                    if result.is_none() {
                        *result = Some((
                            blocked(
                                "Native wave not started: an authorized edit preview failed; regenerate the edit.",
                            ),
                            0.0,
                        ));
                    }
                }
                results[index] = Some((preview, 0.0));
                self.edit_preview_intercepted = true;
                return results
                    .into_iter()
                    .map(|result| result.expect("refused wave closed"))
                    .collect();
            } else if results[index].is_none() {
                positions.push(index);
                ready.push(call.clone());
            }
        }
        if !ready.is_empty() {
            let outputs = self
                .run_background_tools(&ready, ui, Some(backend), read_reuse)
                .expect("admitted native tools");
            for (position, output) in positions.into_iter().zip(outputs) {
                results[position] = Some(output);
            }
        }
        // All wave resources are disjoint. Settle each path's observed revision before the first
        // result can be published; the ordinary round still accounts for every call separately.
        if let Some(checkpoints) = &self.checkpoints {
            checkpoints.settle_changes();
        }
        // Settle source epochs before any coalesced verification sees the final candidate tree.
        for (call, output) in calls.iter().zip(&mut results) {
            if let Some((output, _)) = output {
                self.settle_mutation_output(call, output);
            }
        }
        for (index, call) in calls.iter().enumerate() {
            if bookkeeping(call) {
                results[index] = Some((self.dispatch(backend, call, ui), 0.0));
            } else if let Some((output, _)) = &mut results[index] {
                self.post_mutation_verification(call, output, ui);
            }
        }
        let results: Vec<_> = results
            .into_iter()
            .map(|result| result.expect("one settled outcome per admitted call"))
            .collect();
        self.efficiency_metrics.scheduling.group(
            "native",
            calls.iter().filter(|call| !bookkeeping(call)).count(),
            started.elapsed(),
            calls
                .iter()
                .zip(&results)
                .filter(|(call, _)| !bookkeeping(call))
                .map(|(_, (_, seconds))| *seconds),
        );
        for (call, (output, _)) in calls.iter().zip(&results) {
            if !bookkeeping(call) {
                crate::agent::modes::lifecycle::Lifecycle::dispatched(self, call, output);
            }
        }
        results
    }
}

#[cfg(test)]
pub(super) mod tests;
