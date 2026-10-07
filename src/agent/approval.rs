//! Shared approval policy for the main conversation and delegated tools.
use super::{Decision, Ui, subagents};
use crate::{config::types::Approval, llm::ToolCall, tools::ext::Registry};
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
fn shell_words(command: &str) -> Option<Vec<String>> {
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
            ] || args
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
                || args
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
                        "--staged",
                    ]
        }
        "rustc" | "cargo" | "go" | "python" | "python3" | "node" => args == ["--version"],
        _ => false,
    }
}

pub fn is_read_only(ext: &Registry, name: &str, args: &Value) -> bool {
    match name {
        "experience" => args["action"] != "run",
        "edit_preview" => args["check"] != "type",
        "read" => !crate::tools::documents::needs_approval(args),
        "bash" => args["command"].as_str().is_some_and(safe_bash),
        "task" => !subagents::launches_worker(args),
        "finish" | "tool_search" | "tool_load" | "artifact_read" | "working_state" => true,
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
    // Explicit user attachments are resolved against the conversation by the serial controller.
    if name == "read"
        && args["path"].as_str().is_some_and(|p| {
            (Path::new(p).is_absolute() || p.starts_with("~/"))
                && crate::llm::images::is_image(Path::new(p))
        })
    {
        return false;
    }
    matches!(
        name,
        "read" | "glob" | "grep" | "ast_search" | "web_fetch" | "web_search"
    ) && is_read_only(ext, name, args)
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
    /// at any depth). Applies to `read`, `write`, `edit` and `ast_edit`, never to `bash`, `grep`
    /// or `glob`. Empty means every path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

/// Tools whose target files a path rule can judge.
const PATH_TOOLS: &[&str] = &["read", "write", "edit", "ast_edit", "git"];

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
    let mut found: Vec<String> = Vec::new();
    for list in args["paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        found.push(project_relative(root, list));
    }
    for key in ["path", "file", "from", "to", "dest", "destination"] {
        if let Some(text) = args[key].as_str() {
            found.push(project_relative(root, text));
            // `read` selectors: `path:50-200`, `archive.zip:member`.
            if let Some((head, _)) = text.split_once(':') {
                found.push(project_relative(root, head));
            }
        }
    }
    // Hashline edits name files in `[path#TAG]` headers inside patch text.
    let mut texts = Vec::new();
    collect_strings(args, &mut texts);
    for text in texts {
        let mut rest = text;
        while let Some(open) = rest.find('[') {
            let tail = &rest[open + 1..];
            let Some(hash) = tail.find('#') else { break };
            let name = &tail[..hash];
            if !name.is_empty() && !name.contains(['\n', ']', '[']) {
                found.push(project_relative(root, name));
            }
            rest = &tail[hash..];
        }
    }
    found.sort();
    found.dedup();
    Some(found)
}

fn collect_strings<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => out.push(text),
        Value::Array(items) => items.iter().for_each(|v| collect_strings(v, out)),
        Value::Object(map) => map.values().for_each(|v| collect_strings(v, out)),
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
                return self.effect == Effect::Deny && call.name != "read";
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
        !self.command_prefix.is_empty()
            && call.arguments["command"]
                .as_str()
                .and_then(shell_words)
                .map_or(self.effect == Effect::Deny, |words| {
                    words.starts_with(&self.command_prefix)
                })
    }
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

pub fn authorize(
    approval: Approval,
    rules: &mut Vec<PermissionRule>,
    root: &Path,
    call: &ToolCall,
    summary: &str,
    ui: &mut dyn Ui,
) -> Result<(), Denial> {
    if denied_by_rule(rules, call, root) {
        return Err(Denial::Rule);
    }
    match approval {
        Approval::Readonly => Err(Denial::ReadOnly),
        Approval::Auto => Ok(()),
        Approval::Ask
            if rules
                .iter()
                .any(|r| r.effect == Effect::Allow && r.matches(call, root)) =>
        {
            Ok(())
        }
        Approval::Ask => {
            let summary = if call.name == "bash" {
                format!("{summary}\nShell command requires approval (not classified as read-only).")
            } else {
                summary.to_owned()
            };
            match ui.approve(&call.name, &summary) {
                Decision::Allow => Ok(()),
                Decision::AllowAlways => {
                    let command_prefix = if call.name == "bash" {
                        // Compound/unsupported shell expressions cannot establish a reusable grant.
                        let Some(words) = call.arguments["command"].as_str().and_then(shell_words)
                        else {
                            return Ok(());
                        };
                        let count = if words.get(1).is_some_and(|s| !s.starts_with('-')) {
                            2
                        } else {
                            1
                        };
                        words.into_iter().take(count).collect()
                    } else {
                        Vec::new()
                    };
                    rules.push(PermissionRule {
                        effect: Effect::Allow,
                        tool: call.name.clone(),
                        command_prefix,
                        paths: Vec::new(),
                    });
                    Ok(())
                }
                Decision::Deny => Err(Denial::User),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn shell_inspection_is_conservative() {
        for text in [
            "pwd",
            "ls 'a b'",
            "cat \"a b\"",
            "find . -name '*.rs'",
            "sed -n '1,10p' file",
            "git -c core.fsmonitor=false -c core.pager=cat --no-pager status --short --untracked-files=all",
            "git -c core.fsmonitor=false -c core.pager=cat --no-pager diff --no-color --no-ext-diff --no-textconv",
            "git -c core.fsmonitor=false -c core.pager=cat --no-pager diff --no-color --no-ext-diff --no-textconv --staged",
        ] {
            assert!(safe_bash(text), "{text}");
        }
        for text in [
            "git status",
            "git diff",
            "git log",
            "git show",
            "rg pattern",
            "ls; rm -rf ~",
            "cat x > y",
            "git diff --output=f",
            "find . -exec rm {} +",
            "echo $(rm x)",
            "ls | sh",
            "PATH=x ls",
            "sudo ls",
            "cargo test",
            "cargo build",
            "npm test",
            "python -m pytest",
            "make -n",
            "rg --pre=sh x",
            "sed -n '1e touch x' f",
        ] {
            assert!(!safe_bash(text), "{text}");
        }
    }

    fn bash(command: &str) -> ToolCall {
        ToolCall {
            id: "x".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command":command}),
            malformed_arguments: false,
        }
    }

    #[test]
    fn always_allow_is_scoped_and_deny_wins_even_in_auto_mode() {
        struct Always(usize);
        impl Ui for Always {
            fn approve(&mut self, _: &str, _: &str) -> Decision {
                self.0 += 1;
                Decision::AllowAlways
            }
        }
        let mut rules = Vec::new();
        let mut ui = Always(0);
        assert!(
            authorize(
                Approval::Ask,
                &mut rules,
                Path::new("/p"),
                &bash("cargo test --lib"),
                "",
                &mut ui
            )
            .is_ok()
        );
        assert!(
            authorize(
                Approval::Ask,
                &mut rules,
                Path::new("/p"),
                &bash("cargo test --test agent"),
                "",
                &mut ui
            )
            .is_ok()
        );
        assert_eq!(ui.0, 1);
        assert!(
            authorize(
                Approval::Ask,
                &mut rules,
                Path::new("/p"),
                &bash("cargo build"),
                "",
                &mut ui
            )
            .is_ok()
        );
        assert_eq!(ui.0, 2);
        assert!(
            authorize(
                Approval::Ask,
                &mut rules,
                Path::new("/p"),
                &bash("cargo test; rm x"),
                "",
                &mut ui
            )
            .is_ok()
        );
        assert_eq!(ui.0, 3, "compound command cannot inherit a prefix grant");
        assert_eq!(
            rules.len(),
            2,
            "compound command cannot create a blanket grant"
        );
        rules.push(PermissionRule {
            effect: Effect::Deny,
            tool: "bash".into(),
            command_prefix: vec!["cargo".into(), "test".into()],
            paths: vec![],
        });
        assert!(matches!(
            authorize(
                Approval::Auto,
                &mut rules,
                Path::new("/p"),
                &bash("cargo test"),
                "",
                &mut ui
            ),
            Err(Denial::Rule)
        ));
        assert!(matches!(
            authorize(
                Approval::Readonly,
                &mut rules,
                Path::new("/p"),
                &bash("cargo build"),
                "",
                &mut ui
            ),
            Err(Denial::ReadOnly)
        ));
    }

    #[test]
    fn project_rules_round_trip_and_require_scoped_shell_grants() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.toml");
        let rules = vec![PermissionRule {
            effect: Effect::Deny,
            tool: "bash".into(),
            command_prefix: vec!["git".into(), "push".into()],
            paths: vec![],
        }];
        save(&path, &rules).unwrap();
        assert_eq!(load(&path).unwrap(), rules);
        assert!(
            PermissionRule {
                effect: Effect::Allow,
                tool: "bash".into(),
                command_prefix: vec![],
                paths: vec![]
            }
            .validate()
            .is_err()
        );
    }

    fn file_call(tool: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "x".into(),
            name: tool.into(),
            arguments: args,
            malformed_arguments: false,
        }
    }

    fn rule(effect: Effect, tool: &str, paths: &[&str]) -> PermissionRule {
        PermissionRule {
            effect,
            tool: tool.into(),
            command_prefix: vec![],
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    #[test]
    fn path_rules_match_normalised_project_paths() {
        let root = Path::new("/p");
        let deny = [rule(
            Effect::Deny,
            "write",
            &[".env", "*.pem", "secrets/**"],
        )];
        let blocked = |args: Value| denied_by_rule(&deny, &file_call("write", args), root);
        for path in [
            ".env",
            "./.env",
            "/p/.env",
            "a/../.env",
            "sub/dir/.env",
            "key.pem",
            "secrets/a/b.txt",
            "src/../secrets/x",
        ] {
            assert!(blocked(json!({"path":path,"content":""})), "{path}");
        }
        for path in ["src/main.rs", "env", "secretsx/a"] {
            assert!(!blocked(json!({"path":path,"content":""})), "{path}");
        }
        if cfg!(target_os = "macos") {
            assert!(blocked(json!({"path":".ENV"})));
        }
        assert!(
            !denied_by_rule(&deny, &file_call("edit", json!({"path":".env"})), root),
            "rule names another tool"
        );
    }

    #[test]
    fn path_deny_fails_closed_and_allow_needs_every_file_covered() {
        let root = Path::new("/p");
        let deny = [rule(Effect::Deny, "edit", &["secrets/**"])];
        let hashline = file_call(
            "edit",
            json!({"patch":"[src/a.rs#AB]\n1:x\n[secrets/k#CD]\nPUT 1 +y"}),
        );
        assert!(
            denied_by_rule(&deny, &hashline, root),
            "hashline patch naming a protected file"
        );
        assert!(
            denied_by_rule(
                &deny,
                &file_call("edit", json!({"patch":"nothing parseable"})),
                root
            ),
            "unknown target is denied"
        );
        let allow = rule(Effect::Allow, "edit", &["src/**"]);
        assert!(allow.matches(&file_call("edit", json!({"path":"src/a/b.rs"})), root));
        assert!(!allow.matches(&file_call("edit", json!({"path":"Cargo.toml"})), root));
        assert!(
            !allow.matches(&hashline, root),
            "allow requires all files covered"
        );
        assert!(
            !allow.matches(&file_call("edit", json!({"patch":"x"})), root),
            "unknown target is never allowed"
        );
        assert!(rule(Effect::Allow, "bash", &["src/**"]).validate().is_err());
        assert!(rule(Effect::Allow, "edit", &["["]).validate().is_err());
    }

    #[test]
    fn path_rules_round_trip_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.toml");
        let rules = vec![rule(Effect::Deny, "write", &["*.pem"])];
        save(&path, &rules).unwrap();
        assert_eq!(load(&path).unwrap(), rules);
    }
}
