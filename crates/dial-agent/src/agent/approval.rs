//! Shared approval policy for the main conversation and delegated tools.
use super::{Decision, Ui, subagents};
use crate::{config::types::Approval, llm::ToolCall, tools::integrations::ext::Registry};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

const READ_ONLY: &[&str] = &["read", "glob", "grep", "skill", "session_search"];

/// A deliberately small shell lexer. Unsupported shell syntax always asks, never guesses.
pub(super) fn shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut active = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        if matches!(
            c,
            '$' | '`' | '\n' | '\r' | '(' | ')' | '{' | '}' | '<' | '>' | '&' | ';' | '|'
        ) {
            return None;
        }
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => {
                quote = Some(c);
                active = true;
            }
            (Some('\''), c) => {
                word.push(c);
                active = true;
            }
            (_, '\\') => {
                word.push(chars.next()?);
                active = true;
            }
            (None, c) if c.is_whitespace() => {
                if active {
                    words.push(std::mem::take(&mut word));
                    active = false;
                }
            }
            (_, c) => {
                word.push(c);
                active = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if active {
        words.push(word);
    }
    (!words.is_empty()).then_some(words)
}

/// Programs that run other programs or code. A reusable grant on them would authorize any payload.
const CODE_RUNNERS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "python",
    "python3",
    "node",
    "perl",
    "ruby",
    "php",
    "lua",
    "osascript",
    "deno",
    "bun",
    "awk",
    "sed",
];
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];
/// Wrappers run their operands as a program. Every operand position is checked, not just the first.
const WRAPPERS: &[&str] = &[
    "env",
    "command",
    "exec",
    "builtin",
    "nohup",
    "nice",
    "time",
    "stdbuf",
    "xargs",
    "sudo",
    "doas",
    "timeout",
    "caffeinate",
];

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Every argv a bash command can run, outermost first: wrappers, `sh -c` bodies and git global options are
/// resolved, so `/bin/rm`, `env rm`, `FOO=1 rm` and `git -C dir push` all expose their real program.
/// `complete` is false when some part cannot be parsed; deny rules then match and allow rules never do.
fn shell_argvs(command: &str) -> (Vec<Vec<String>>, bool) {
    let mut argvs = Vec::new();
    let complete =
        shell_words(command).is_some_and(|words| collect_argvs(&words, &mut argvs, 0, true));
    (argvs, complete)
}

/// Normalizes a user-typed bash prefix the way the matcher sees commands: `/bin/rm` becomes `rm` and
/// `git -C dir push` becomes `git push`. Only a single complete argv can serve as a prefix; wrappers,
/// shell bodies and compound commands would expand to several argvs and are rejected.
pub fn bash_prefix(words: &[String]) -> Result<Vec<String>, String> {
    if words.is_empty() {
        return Err("bash permissions require a command prefix".into());
    }
    let (argvs, complete) = shell_argvs(&words.join(" "));
    match argvs.as_slice() {
        [argv] if complete => Ok(argv.clone()),
        _ if !complete => Err("command prefix cannot be parsed; simplify the quoting or syntax".into()),
        _ => Err("command prefix must name a single program; wrappers, shell bodies and compound commands cannot be stored".into()),
    }
}

fn collect_argvs(
    words: &[String],
    out: &mut Vec<Vec<String>>,
    depth: usize,
    scan_wrappers: bool,
) -> bool {
    if depth > 8 {
        return false;
    }
    let mut i = 0;
    while i < words.len() && is_assignment(&words[i]) {
        i += 1;
    }
    let Some(first) = words.get(i) else {
        return true;
    };
    let rest = &words[i + 1..];
    if first.contains(char::is_whitespace) {
        // `env -S 'rm -f x'` splits one operand into several words.
        let Some(mut inner) = shell_words(first) else {
            return false;
        };
        inner.extend(rest.iter().cloned());
        return collect_argvs(&inner, out, depth + 1, scan_wrappers);
    }
    let name = std::path::Path::new(first)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(first);
    if name == "git" {
        // Global options (`-C dir`, `-c k=v`, `--no-pager`) sit between `git` and the subcommand.
        let mut j = 0;
        while j < rest.len() && rest[j].starts_with('-') {
            j += if matches!(
                rest[j].as_str(),
                "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace"
            ) {
                2
            } else {
                1
            };
        }
        out.push(
            std::iter::once("git".to_string())
                .chain(rest.get(j..).unwrap_or(&[]).iter().cloned())
                .collect(),
        );
        return true;
    }
    out.push(
        std::iter::once(name.to_string())
            .chain(rest.iter().cloned())
            .collect(),
    );
    if SHELLS.contains(&name) {
        // Any dash-option containing `c` (`-c`, `-lc`, `-o pipefail -c`) takes the next word as the body.
        if let Some(j) = rest
            .iter()
            .position(|w| w.starts_with('-') && !w.starts_with("--") && w.contains('c'))
        {
            let Some(body) = rest.get(j + 1).and_then(|b| shell_words(b)) else {
                return false;
            };
            return collect_argvs(&body, out, depth + 1, true);
        }
        return true;
    }
    if scan_wrappers && WRAPPERS.contains(&name) {
        // Wrapper options, assignments and durations are not parsed: every operand position is checked as a
        // possible program. Nested wrappers are covered because each of their operands is also a position here.
        let mut complete = true;
        for k in 0..rest.len() {
            complete &= collect_argvs(&rest[k..], out, depth + 1, false);
        }
        return complete;
    }
    true
}

/// These are inspection commands, not build/test/package-script exemptions. Those execute code.
pub fn safe_bash(command: &str) -> bool {
    let Some(words) = shell_words(command) else {
        return false;
    };
    let args = &words[1..];
    match words[0].as_str() {
        "ls" | "pwd" | "cat" | "head" | "tail" | "wc" | "stat" | "which" | "echo" | "printf" => {
            true
        }
        "uname" => args
            .iter()
            .all(|a| matches!(a.as_str(), "-a" | "-m" | "-n" | "-p" | "-r" | "-s" | "-v")),
        "date" => args.iter().all(|a| {
            matches!(a.as_str(), "-u" | "-R" | "-I" | "--utc" | "--rfc-email") || a.starts_with('+')
        }),
        "file" => args.iter().all(|a| {
            !a.starts_with('-')
                || matches!(
                    a.as_str(),
                    "--" | "-b"
                        | "-i"
                        | "-I"
                        | "-L"
                        | "-h"
                        | "--brief"
                        | "--mime"
                        | "--mime-type"
                        | "--mime-encoding"
                        | "--dereference"
                        | "--no-dereference"
                )
        }),
        "tree" => args.iter().all(|a| {
            !a.starts_with('-')
                || matches!(a.as_str(), "--" | "-a" | "-d" | "-f" | "-L" | "--dirsfirst")
        }),
        "grep" => true,
        "find" => !args.iter().any(|a| {
            matches!(
                a.as_str(),
                "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete"
            ) || a.starts_with("-fprint")
                || a == "-fls"
        }),
        "sed" => {
            args.len() >= 2
                && args[0] == "-n"
                && args[1].ends_with('p')
                && args[1][..args[1].len() - 1]
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == ',' || c == '$')
                && !args[2..].iter().any(|a| a.starts_with('-'))
        }
        "git" => {
            args == [
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.pager=cat",
                "--no-pager",
                "status",
                "--short",
                "--untracked-files=all",
            ] || args.get(..9).is_some_and(|prefix| {
                prefix
                    == [
                        "-c",
                        "core.fsmonitor=false",
                        "-c",
                        "core.pager=cat",
                        "--no-pager",
                        "diff",
                        "--no-color",
                        "--no-ext-diff",
                        "--no-textconv",
                    ]
            }) && {
                let flags = &args[9..];
                flags.is_empty()
                    || flags == ["--staged"]
                    || flags == ["--stat"]
                    || flags == ["--staged", "--stat"]
                    || flags == ["--stat", "--staged"]
            }
        }
        "rustc" | "cargo" | "go" | "python" | "python3" | "node" => args == ["--version"],
        _ => false,
    }
}

/// Effect tracking only: a batch of the exact safe Git inspection forms does not
/// introduce new workspace effects. Approval still uses safe_bash and rejects batches.
pub(super) fn safe_review_batch(command: &str) -> bool {
    command
        .split("&&")
        .flat_map(|part| part.split(';'))
        .flat_map(|part| part.trim().split('\n'))
        .all(|part| {
            let part = part.trim();
            shell_words(part).is_some_and(|words| words[0] == "git") && safe_bash(part)
        })
}

pub fn is_read_only(ext: &Registry, name: &str, args: &Value) -> bool {
    match name {
        "experience" => args["action"] != "run",
        "edit_preview" => args["check"] != "type",
        "read" => {
            // `paths` reads every target through the same single-file path, so each one decides.
            let mut targets = Vec::new();
            collect_declared_paths(args, &mut targets);
            !targets.iter().any(|path| {
                crate::tools::integrations::ssh::url::is_ssh_url(path)
                    || crate::tools::fs::documents::needs_approval(
                        &serde_json::json!({ "path": path }),
                    )
            })
        }
        "grep"
            if args["path"]
                .as_str()
                .is_some_and(crate::tools::integrations::ssh::url::is_ssh_url) =>
        {
            false
        }
        "bash" => args["command"].as_str().is_some_and(safe_bash),
        "task" => !subagents::launches_worker(args),
        "context_notes" => super::context_notes::readonly(name, args),
        "new_context" => false,
        "checkpoint" | "rewind" => true,
        "ask" | "finish" | "goal" | "tool_search" | "tool_load" | "artifact_read"
        | "working_state" => true,
        "recall" | "reflect" => true,
        "retain" => !args["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["scope"] == "global")),
        "learn" => args.get("skill").is_none() && args["scope"] != "global",
        n if READ_ONLY.contains(&n) => true,
        _ => ext.get(name).is_some_and(|tool| !tool.needs_approval(args)),
    }
}

pub fn needs_approval(ext: &Registry, call: &ToolCall) -> bool {
    // Todo is private planning state, not an external effect, but is not parallelizable.
    call.name != "todo" && !is_read_only(ext, &call.name, &call.arguments)
}

/// Stateful reads (background output cursors, memory, todo) remain serial barriers.
pub fn parallel_read(ext: &Registry, name: &str, args: &Value) -> bool {
    // Read-only shell commands join parallel groups unless they name a stateful or managed form:
    // a retained `session` (cwd and exports) or a background/async job is a serial barrier.
    if name == "bash" {
        let flag_off = |key| matches!(args.get(key), None | Some(Value::Null | Value::Bool(false)));
        return flag_off("background")
            && flag_off("async")
            && args["session"].is_null()
            && is_read_only(ext, name, args);
    }
    // Explicit user attachments are resolved against the conversation by the serial controller.
    if name == "read" {
        let mut targets = Vec::new();
        collect_declared_paths(args, &mut targets);
        if targets.iter().any(|p| {
            (Path::new(p).is_absolute() || p.starts_with("~/"))
                && crate::llm::images::is_image(Path::new(p))
        }) {
            return false;
        }
    }
    let shared = ext.get(name).map_or_else(
        || matches!(name, "read" | "glob" | "grep" | "ast_search"),
        |tool| {
            tool.concurrency(args) == crate::tools::integrations::ext::Concurrency::Shared
                && !tool.supports_tool_dispatch()
                && !tool.needs_approval(args)
        },
    );
    shared && is_read_only(ext, name, args)
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Allow,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PermissionRule {
    pub effect: Effect,
    pub tool: String,
    #[serde(default)]
    pub command_prefix: Vec<String>,
    /// Glob patterns over project-relative paths (`src/**`, `*.pem`; a pattern without `/` matches
    /// at any depth). Applies to file operations, AST edits, Git, notebooks and LSP targets;
    /// never to `bash`, `grep` or `glob`. Empty means every path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

/// Tools whose target files a path rule can judge.
const PATH_TOOLS: &[&str] = &[
    "read", "write", "edit", "ast_edit", "git", "notebook", "lsp", "browser",
];

/// Lexically normalise `input` against `root`: no `.`/`..` components, relative when inside the project.
fn project_relative(root: &Path, input: &str) -> String {
    let joined = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        root.join(input)
    };
    let mut clean = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            other => clean.push(other),
        }
    }
    clean
        .strip_prefix(root)
        .unwrap_or(&clean)
        .to_string_lossy()
        .into_owned()
}

/// Paths a call targets, or `None` when the tool is not governed by path rules. An empty list means
/// the target could not be determined.
fn call_paths(call: &ToolCall, root: &Path) -> Option<Vec<String>> {
    if !PATH_TOOLS.contains(&call.name.as_str()) {
        return None;
    }
    let args = &call.arguments;
    if call.name == "edit" {
        let Ok(paths) = crate::tools::declared_edit_targets(args) else {
            return Some(Vec::new());
        };
        let mut found = Vec::new();
        for path in paths {
            if !record_target(root, &path, &mut found) {
                return Some(Vec::new());
            }
        }
        found.sort();
        found.dedup();
        return Some(found);
    }
    let mut found: Vec<String> = Vec::new();
    let mut declared = Vec::new();
    collect_declared_paths(args, &mut declared);
    for text in declared {
        let literal = if Path::new(text).is_absolute() {
            PathBuf::from(text)
        } else {
            root.join(text)
        };
        if call.name == "git" && (literal.is_dir() || text.contains(['*', '?', '[', ':'])) {
            // Git pathspecs/directories can expand beyond the declared string. A narrow
            // permission cannot prove their targets; deny conservatively, never grant.
            return Some(Vec::new());
        }
        let target = if call.name == "read" && !literal.is_file() {
            text.split([':', '?']).next().unwrap_or(text)
        } else {
            text
        };
        if !record_target(root, target, &mut found) {
            return Some(Vec::new());
        }
    }
    found.sort();
    found.dedup();
    Some(found)
}
/// Match both the declared name and its physical alias, including a new file below
/// an existing symlinked parent. Literal filenames retain selector precedence.
fn record_target(root: &Path, input: &str, out: &mut Vec<String>) -> bool {
    if input.len() > 4096 {
        return false;
    }
    out.push(project_relative(root, input));
    let joined = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        root.join(input)
    };
    if joined.as_os_str().len() > 4096 {
        return false;
    }
    for ancestor in joined.ancestors() {
        if let Ok(mut physical) = fs::canonicalize(ancestor) {
            if let Ok(suffix) = joined.strip_prefix(ancestor) {
                physical.push(suffix);
            }
            let Some(name) = physical.to_str() else {
                return false;
            };
            out.push(project_relative(root, name));
            break;
        }
    }
    true
}

fn collect_declared_paths<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::Array(items) => items
            .iter()
            .for_each(|value| collect_declared_paths(value, out)),
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(
                    key.as_str(),
                    "path" | "file" | "from" | "to" | "dest" | "destination"
                ) {
                    if let Some(path) = value.as_str() {
                        out.push(path);
                    }
                } else if matches!(key.as_str(), "paths" | "files") {
                    out.extend(
                        value
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str),
                    );
                } else if value.is_array() || value.is_object() {
                    collect_declared_paths(value, out);
                }
            }
        }
        _ => {}
    }
}

fn glob_set(patterns: &[String]) -> Result<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        let anchored = pattern.trim_start_matches("./");
        let pattern = if anchored.contains('/') {
            anchored.to_owned()
        } else {
            format!("**/{anchored}")
        };
        // macOS volumes are case-insensitive by default: `.ENV` is `.env`.
        builder.add(
            globset::GlobBuilder::new(&pattern)
                .literal_separator(true)
                .case_insensitive(cfg!(target_os = "macos"))
                .build()
                .with_context(|| format!("invalid path pattern {pattern:?}"))?,
        );
    }
    Ok(builder.build()?)
}

impl PermissionRule {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.tool.is_empty(), "permission tool must not be empty");
        ensure!(
            self.tool != "bash" || !self.command_prefix.is_empty(),
            "bash permissions require a command prefix"
        );
        ensure!(
            self.tool == "bash" || self.command_prefix.is_empty(),
            "command prefixes apply only to bash"
        );
        ensure!(
            self.paths.is_empty() || PATH_TOOLS.contains(&self.tool.as_str()),
            "path patterns apply only to {}",
            PATH_TOOLS.join(", ")
        );
        ensure!(
            self.paths.len() <= 32
                && self
                    .paths
                    .iter()
                    .all(|p| !p.trim().is_empty() && p.len() <= 256),
            "path patterns must be 1..256 bytes, at most 32"
        );
        glob_set(&self.paths)?;
        Ok(())
    }

    fn matches(&self, call: &ToolCall, root: &Path) -> bool {
        if self.tool != call.name {
            return false;
        }
        if !self.paths.is_empty() {
            let (Some(targets), Ok(set)) = (call_paths(call, root), glob_set(&self.paths)) else {
                return self.effect == Effect::Deny;
            };
            // An unreadable target is judged conservatively: it matches deny rules, never allow rules.
            if targets.is_empty() {
                return self.effect == Effect::Deny;
            }
            // A call touching several files is allowed only if every file is covered, denied if any is.
            return match self.effect {
                Effect::Deny => targets.iter().any(|t| set.is_match(t)),
                Effect::Allow => targets.iter().all(|t| set.is_match(t)),
            };
        }
        if call.name != "bash" {
            return true;
        }
        if self.command_prefix.is_empty() {
            return false;
        }
        let (argvs, complete) = call.arguments["command"]
            .as_str()
            .map_or((Vec::new(), false), shell_argvs);
        match self.effect {
            // Deny: any executed argv, or any part that cannot be parsed, is a match.
            Effect::Deny => !complete || argvs.iter().any(|a| a.starts_with(&self.command_prefix)),
            // Allow: only when every executed argv is covered and nothing is hidden.
            Effect::Allow => {
                complete
                    && !argvs.is_empty()
                    && argvs.iter().all(|a| a.starts_with(&self.command_prefix))
            }
        }
    }
}
/// Checks native targets after the server has computed them but before any effects.
/// The original operation was already approved; this gate cannot widen its grant.
pub(crate) fn authorize_mutation_paths(
    rules: &[PermissionRule],
    root: &Path,
    extra_roots: &[PathBuf],
    read_only: bool,
    original: &ToolCall,
    requested_name: &str,
    paths: &[PathBuf],
    unbounded_effects: bool,
) -> std::result::Result<(), String> {
    if requested_name != original.name {
        return Err("native mutation authority does not match the executing tool".into());
    }
    if read_only {
        return Err("native mutation denied by read-only execution policy".into());
    }
    let relevant = |rule: &PermissionRule| {
        rule.tool == requested_name || matches!(rule.tool.as_str(), "write" | "edit" | "ast_edit")
    };
    if unbounded_effects {
        if rules
            .iter()
            .any(|rule| relevant(rule) && (!rule.paths.is_empty() || rule.effect == Effect::Deny))
        {
            return Err(
                "unbounded native effects cannot honor the active path permission policy".into(),
            );
        }
        return Ok(());
    }
    if paths.is_empty()
        || paths.iter().any(|path| {
            !path.is_absolute()
                || !(path.starts_with(root)
                    || extra_roots.iter().any(|extra| path.starts_with(extra)))
                || path
                    .components()
                    .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        })
    {
        return Err("native mutation must declare canonical workspace targets".into());
    }
    let targets: Vec<String> = paths
        .iter()
        .map(|path| project_relative(root, &path.to_string_lossy()))
        .collect();
    for rule in rules
        .iter()
        .filter(|rule| relevant(rule) && rule.effect == Effect::Deny)
    {
        if rule.paths.is_empty() {
            return Err(format!(
                "native mutation denied by {} permission",
                rule.tool
            ));
        }
        let set = glob_set(&rule.paths)
            .map_err(|error| format!("invalid native path permission: {error:#}"))?;
        if targets.iter().any(|target| set.is_match(target)) {
            return Err(format!(
                "native mutation target denied by {} path permission",
                rule.tool
            ));
        }
    }
    let scoped_grant = rules.iter().any(|rule| {
        rule.tool == requested_name
            && rule.effect == Effect::Allow
            && !rule.paths.is_empty()
            && rule.matches(original, root)
    });
    if scoped_grant {
        let covered = rules
            .iter()
            .filter(|rule| rule.tool == requested_name && rule.effect == Effect::Allow)
            .any(|rule| {
                rule.paths.is_empty()
                    || glob_set(&rule.paths)
                        .is_ok_and(|set| targets.iter().all(|target| set.is_match(target)))
            });
        if !covered {
            return Err("computed native mutation targets exceed the approved path grant".into());
        }
    }
    Ok(())
}

/// Source bytes read to prepare an approved `edit`/`write`. This is read-deny policy over the
/// computed targets only: it grants no mutation, ignores read-only mutation policy, and is
/// reachable only from the owner that is already executing the guarded edit.
pub(crate) fn authorize_source_read_paths(
    rules: &[PermissionRule],
    root: &Path,
    extra_roots: &[PathBuf],
    original: &ToolCall,
    requested_name: &str,
    paths: &[PathBuf],
    unbounded_effects: bool,
) -> std::result::Result<(), String> {
    if requested_name != "read" || !matches!(original.name.as_str(), "edit" | "write") {
        return Err("native source read authority does not match the executing tool".into());
    }
    // Same containment as `authorize_mutation_paths`: the primary root or an explicitly added root.
    if unbounded_effects
        || paths.is_empty()
        || paths.iter().any(|path| {
            !path.is_absolute()
                || !(path.starts_with(root)
                    || extra_roots.iter().any(|extra| path.starts_with(extra)))
                || path
                    .components()
                    .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        })
    {
        return Err("native source read must declare canonical workspace targets".into());
    }
    for path in paths {
        let call = ToolCall {
            id: String::new(),
            name: "read".into(),
            arguments: serde_json::json!({"path": path.to_string_lossy()}),
            malformed_arguments: false,
        };
        if denied_by_rule(rules, &call, root) {
            return Err("native source read denied by read permission".into());
        }
    }
    Ok(())
}

pub(crate) fn authorize_managed_memory_paths(
    rules: &[PermissionRule],
    layout: &crate::config::paths::Layout,
    read_only: bool,
    original: &ToolCall,
    paths: &[PathBuf],
) -> std::result::Result<(), String> {
    let root = crate::storage::memory_backend::managed_root(layout);
    let directory = crate::storage::memory_backend::managed_directory(layout);
    if !paths
        .iter()
        .all(|path| path == &root || path.starts_with(&directory))
    {
        return Err(
            "managed memory mutation must stay in the owning project's private namespace".into(),
        );
    }
    authorize_mutation_paths(
        rules,
        &root,
        &[],
        read_only,
        original,
        &original.name,
        paths,
        false,
    )
}

#[derive(Default, Serialize, Deserialize)]
struct Permissions {
    #[serde(default)]
    rules: Vec<PermissionRule>,
}

pub fn load(path: &Path) -> Result<Vec<PermissionRule>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("reading project permissions"),
    };
    let permissions: Permissions = toml::from_str(&text).context("parsing project permissions")?;
    for rule in &permissions.rules {
        rule.validate()?;
    }
    Ok(permissions.rules)
}

pub fn save(path: &Path, rules: &[PermissionRule]) -> Result<()> {
    for rule in rules {
        rule.validate()?;
    }
    let parent = path.parent().context("permissions path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(
        toml::to_string(&Permissions {
            rules: rules.to_vec(),
        })?
        .as_bytes(),
    )?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub enum Denial {
    ReadOnly,
    User,
    Rule,
}
impl Denial {
    pub fn message(&self) -> &'static str {
        match self {
            Self::ReadOnly => {
                "denied: read-only mode; this tool changes things. Tell the user what you would do instead."
            }
            Self::User => {
                "denied by the user. Do not retry the same action; ask what they would prefer."
            }
            Self::Rule => {
                "denied by a permission rule; deny rules override allow rules and approval mode."
            }
        }
    }
}

pub fn denied_by_rule(rules: &[PermissionRule], call: &ToolCall, root: &Path) -> bool {
    rules
        .iter()
        .any(|r| r.effect == Effect::Deny && r.matches(call, root))
}

/// A preview is an internal read, never a permission grant. Skip it when editing
/// would need a prompt or any source/destination alias has a current read denial.
pub(crate) fn edit_preview_allowed(
    approval: Approval,
    rules: &[PermissionRule],
    root: &Path,
    call: &ToolCall,
    paths: &[PathBuf],
) -> bool {
    if paths.is_empty()
        || denied_by_rule(rules, call, root)
        || approval == Approval::Readonly
        || (approval == Approval::Ask
            && !rules
                .iter()
                .any(|rule| rule.effect == Effect::Allow && rule.matches(call, root)))
    {
        return false;
    }
    let mut read = ToolCall {
        id: String::new(),
        name: "read".into(),
        arguments: serde_json::json!({"path":""}),
        malformed_arguments: false,
    };
    for path in paths {
        let Some(path) = path.to_str() else {
            return false;
        };
        read.arguments["path"] = Value::String(path.into());
        if denied_by_rule(rules, &read, root) {
            return false;
        }
    }
    true
}

/// Shell commands the `write` tier still asks about: anything that destroys data outside the
/// edit it was asked for, rewrites history, escalates privileges or runs downloaded code. The
/// sandbox already confines writes to the project; this is the second line.
pub fn destructive_bash(command: &str) -> bool {
    let (argvs, complete) = shell_argvs(command);
    if !complete || argvs.is_empty() {
        return true;
    }
    let lower = command.to_ascii_lowercase();
    if lower.contains("| sh")
        || lower.contains("|sh")
        || lower.contains("| bash")
        || lower.contains("|bash")
    {
        return true;
    }
    argvs.iter().any(|argv| {
        let program = argv[0].rsplit('/').next().unwrap_or(&argv[0]);
        let args = &argv[1..];
        let has_flag = |letters: &[char]| {
            args.iter().any(|a| {
                a.starts_with('-')
                    && !a.starts_with("--")
                    && a[1..].chars().any(|c| letters.contains(&c))
            })
        };
        match program {
            "sudo" | "su" | "doas" | "dd" | "shutdown" | "reboot" | "halt" | "mkfs" | "fdisk"
            | "diskutil" | "launchctl" | "systemctl" | "crontab" | "truncate" | "shred" => true,
            "rm" => {
                has_flag(&['r', 'R', 'f'])
                    || args.iter().any(|a| a == "--recursive" || a == "--force")
            }
            "chmod" | "chown" | "chgrp" => has_flag(&['R']),
            "kill" | "killall" | "pkill" => true,
            "git" => {
                args.iter().any(|a| {
                    matches!(
                        a.as_str(),
                        "push"
                            | "reset"
                            | "clean"
                            | "rebase"
                            | "filter-branch"
                            | "gc"
                            | "prune"
                            | "reflog"
                    ) || a == "--hard"
                        || a == "--force"
                        || a == "-f"
                        || a == "-D"
                        || a == "--delete"
                }) || args
                    .windows(2)
                    .any(|w| w[0] == "checkout" && (w[1] == "--" || w[1] == "."))
            }
            "find" => args
                .iter()
                .any(|a| a == "-delete" || a == "-exec" || a == "-execdir"),
            "curl" | "wget" => args
                .iter()
                .any(|a| a == "-o" || a == "-O" || a.starts_with("--output")),
            _ => false,
        }
    }) || argvs
        .iter()
        .any(|argv| CODE_RUNNERS.contains(&argv[0].as_str()) && argv.iter().any(|a| a == "-c"))
}

/// What the `write` tier runs without a prompt: the workspace editing tools, delegation, and
/// shell commands that are not [`destructive_bash`]. Extension tools that declared they need
/// approval (network, browser, MCP) are outside the workspace and still ask.
fn workspace_write(call: &ToolCall) -> bool {
    match call.name.as_str() {
        "write" | "edit" | "ast_edit" | "notebook_edit" | "task" | "todo" | "finish" => true,
        "bash" => call.arguments["command"]
            .as_str()
            .is_some_and(|command| !destructive_bash(command)),
        _ => false,
    }
}

/// `Ok(true)` when the user chose "Always allow in this project" and the caller should save the rules.
pub fn authorize(
    approval: Approval,
    rules: &mut Vec<PermissionRule>,
    root: &Path,
    call: &ToolCall,
    summary: &str,
    ui: &mut dyn Ui,
) -> Result<bool, Denial> {
    authorize_inner(approval, rules, root, call, summary, ui, None)
}

pub(crate) fn authorize_with_cancel(
    approval: Approval,
    rules: &mut Vec<PermissionRule>,
    root: &Path,
    call: &ToolCall,
    summary: &str,
    ui: &mut dyn Ui,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<bool, Denial> {
    authorize_inner(approval, rules, root, call, summary, ui, Some(cancel))
}

fn authorize_inner(
    approval: Approval,
    rules: &mut Vec<PermissionRule>,
    root: &Path,
    call: &ToolCall,
    summary: &str,
    ui: &mut dyn Ui,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<bool, Denial> {
    if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)) {
        return Err(Denial::User);
    }
    if denied_by_rule(rules, call, root) {
        return Err(Denial::Rule);
    }
    match approval {
        Approval::Readonly => Err(Denial::ReadOnly),
        Approval::Auto => Ok(false),
        Approval::Ask | Approval::Write
            if rules
                .iter()
                .any(|r| r.effect == Effect::Allow && r.matches(call, root)) =>
        {
            Ok(false)
        }
        Approval::Write if workspace_write(call) => Ok(false),
        Approval::Ask | Approval::Write => {
            let summary = if call.name == "bash" && approval == Approval::Write {
                format!("{summary}\nDestructive or privileged shell command; approval required.")
            } else if call.name == "bash" {
                format!("{summary}\nShell command requires approval (not classified as read-only).")
            } else {
                summary.to_owned()
            };
            let decision = ui.approve_call(&call.id, &call.name, &summary, cancel);
            if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)) {
                return Err(Denial::User);
            }
            match decision {
                Decision::Allow => Ok(false),
                Decision::AllowAlways | Decision::AllowProject => {
                    let persist = decision == Decision::AllowProject;
                    let command_prefix = if call.name == "bash" {
                        // Only one plain program grants a reusable prefix. Wrappers, shell bodies, code
                        // runners and flag-first invocations would authorize far more than the approved command.
                        let (argvs, complete) = call.arguments["command"]
                            .as_str()
                            .map_or((Vec::new(), false), shell_argvs);
                        let [argv] = argvs.as_slice() else {
                            return Ok(false);
                        };
                        if !complete
                            || CODE_RUNNERS.contains(&argv[0].as_str())
                            || argv.get(1).is_some_and(|s| s.starts_with('-'))
                        {
                            return Ok(false);
                        }
                        argv.iter().take(2).cloned().collect()
                    } else {
                        Vec::new()
                    };
                    rules.push(PermissionRule {
                        effect: Effect::Allow,
                        tool: call.name.clone(),
                        command_prefix,
                        paths: Vec::new(),
                    });
                    Ok(persist)
                }
                Decision::Deny => Err(Denial::User),
            }
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod ast_registration_tests;
#[cfg(test)]
mod parallel_bash_tests;
