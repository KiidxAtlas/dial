//! Shared approval policy for the main conversation and delegated tools.
use super::{Decision, Ui, subagents};
use crate::{config::types::Approval, llm::ToolCall, tools::ext::Registry};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, io::Write, path::Path};

const READ_ONLY: &[&str] = &["read", "glob", "grep", "skill", "session_search"];

/// A deliberately small shell lexer. Unsupported shell syntax always asks, never guesses.
fn shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut active = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        if matches!(c, '$' | '`' | '\n' | '\r' | '(' | ')' | '{' | '}' | '<' | '>' | '&' | ';' | '|' ) { return None; }
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => { quote = Some(c); active = true; }
            (Some('\''), c) => { word.push(c); active = true; }
            (_, '\\') => { word.push(chars.next()?); active = true; }
            (None, c) if c.is_whitespace() => {
                if active { words.push(std::mem::take(&mut word)); active = false; }
            }
            (_, c) => { word.push(c); active = true; }
        }
    }
    if quote.is_some() { return None; }
    if active { words.push(word); }
    (!words.is_empty()).then_some(words)
}

/// These are inspection commands, not build/test/package-script exemptions. Those execute code.
pub fn safe_bash(command: &str) -> bool {
    let Some(words) = shell_words(command) else { return false };
    let args = &words[1..];
    match words[0].as_str() {
        "ls" | "pwd" | "cat" | "head" | "tail" | "wc" | "stat" | "which" |
        "echo" | "printf" => true,
        "uname" => args.iter().all(|a| matches!(a.as_str(), "-a" | "-m" | "-n" | "-p" | "-r" | "-s" | "-v")),
        "date" => args.iter().all(|a| matches!(a.as_str(), "-u" | "-R" | "-I" | "--utc" | "--rfc-email") || a.starts_with('+')),
        "file" => args.iter().all(|a| !a.starts_with('-') || matches!(a.as_str(), "--" | "-b" | "-i" | "-I" | "-L" | "-h" | "--brief" | "--mime" | "--mime-type" | "--mime-encoding" | "--dereference" | "--no-dereference")),
        "tree" => args.iter().all(|a| !a.starts_with('-') || matches!(a.as_str(), "--" | "-a" | "-d" | "-f" | "-L" | "--dirsfirst")),
        "grep" => true,
        "find" => !args.iter().any(|a| matches!(a.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete") || a.starts_with("-fprint") || a == "-fls"),
        "sed" => args.len() >= 2 && args[0] == "-n" && args[1].ends_with('p')
            && args[1][..args[1].len()-1].chars().all(|c| c.is_ascii_digit() || c == ',' || c == '$')
            && !args[2..].iter().any(|a| a.starts_with('-')),
        "rustc" | "cargo" | "go" | "python" | "python3" | "node" => args == ["--version"],
        _ => false,
    }
}

pub fn is_read_only(ext: &Registry, name: &str, args: &Value) -> bool {
    match name {
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
    matches!(name, "read" | "glob" | "grep" | "ast_search" | "web_fetch" | "web_search")
        && is_read_only(ext, name, args)
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Effect { Allow, Deny }

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PermissionRule {
    pub effect: Effect,
    pub tool: String,
    #[serde(default)]
    pub command_prefix: Vec<String>,
}

impl PermissionRule {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.tool.is_empty(), "permission tool must not be empty");
        ensure!(self.tool != "bash" || !self.command_prefix.is_empty(), "bash permissions require a command prefix");
        ensure!(self.tool == "bash" || self.command_prefix.is_empty(), "command prefixes apply only to bash");
        Ok(())
    }

    fn matches(&self, call: &ToolCall) -> bool {
        if self.tool != call.name { return false; }
        if call.name != "bash" { return true; }
        !self.command_prefix.is_empty() && call.arguments["command"].as_str().and_then(shell_words)
            .map_or(self.effect == Effect::Deny, |words| words.starts_with(&self.command_prefix))
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Permissions { #[serde(default)] rules: Vec<PermissionRule> }

pub fn load(path: &Path) -> Result<Vec<PermissionRule>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("reading project permissions"),
    };
    let permissions: Permissions = toml::from_str(&text).context("parsing project permissions")?;
    for rule in &permissions.rules { rule.validate()?; }
    Ok(permissions.rules)
}

pub fn save(path: &Path, rules: &[PermissionRule]) -> Result<()> {
    for rule in rules { rule.validate()?; }
    let parent = path.parent().context("permissions path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(toml::to_string(&Permissions { rules: rules.to_vec() })?.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub enum Denial { ReadOnly, User, Rule }
impl Denial {
    pub fn message(&self) -> &'static str {
        match self {
            Self::ReadOnly => "denied: read-only mode; this tool changes things. Tell the user what you would do instead.",
            Self::User => "denied by the user. Do not retry the same action; ask what they would prefer.",
            Self::Rule => "denied by a permission rule; deny rules override allow rules and approval mode.",
        }
    }
}

pub fn denied_by_rule(rules: &[PermissionRule], call: &ToolCall) -> bool {
    rules.iter().any(|r| r.effect == Effect::Deny && r.matches(call))
}

pub fn authorize(approval: Approval, rules: &mut Vec<PermissionRule>, call: &ToolCall, summary: &str, ui: &mut dyn Ui) -> Result<(), Denial> {
    if denied_by_rule(rules, call) { return Err(Denial::Rule); }
    match approval {
        Approval::Readonly => Err(Denial::ReadOnly),
        Approval::Auto => Ok(()),
        Approval::Ask if rules.iter().any(|r| r.effect == Effect::Allow && r.matches(call)) => Ok(()),
        Approval::Ask => {
            let summary = if call.name == "bash" { format!("{summary}\nShell command requires approval (not classified as read-only).") } else { summary.to_owned() };
            match ui.approve(&call.name, &summary) {
                Decision::Allow => Ok(()),
                Decision::AllowAlways => {
                    let command_prefix = if call.name == "bash" {
                        // Compound/unsupported shell expressions cannot establish a reusable grant.
                        let Some(words) = call.arguments["command"].as_str().and_then(shell_words) else { return Ok(()) };
                        let count = if words.get(1).is_some_and(|s| !s.starts_with('-')) { 2 } else { 1 };
                        words.into_iter().take(count).collect()
                    } else { Vec::new() };
                    rules.push(PermissionRule { effect: Effect::Allow, tool: call.name.clone(), command_prefix });
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
    #[test]
    fn shell_inspection_is_conservative() {
        for text in ["pwd", "ls 'a b'", "cat \"a b\"", "find . -name '*.rs'", "sed -n '1,10p' file"] { assert!(safe_bash(text), "{text}"); }
        for text in ["git status", "git diff", "git log", "git show", "rg pattern", "ls; rm -rf ~", "cat x > y", "git diff --output=f", "find . -exec rm {} +", "echo $(rm x)", "ls | sh", "PATH=x ls", "sudo ls", "cargo test", "cargo build", "npm test", "python -m pytest", "make -n", "rg --pre=sh x", "sed -n '1e touch x' f"] { assert!(!safe_bash(text), "{text}"); }
    }

    fn bash(command: &str) -> ToolCall {
        ToolCall { id:"x".into(), name:"bash".into(), arguments:serde_json::json!({"command":command}), malformed_arguments:false }
    }

    #[test]
    fn always_allow_is_scoped_and_deny_wins_even_in_auto_mode() {
        struct Always(usize);
        impl Ui for Always {
            fn approve(&mut self, _: &str, _: &str) -> Decision { self.0 += 1; Decision::AllowAlways }
        }
        let mut rules = Vec::new();
        let mut ui = Always(0);
        assert!(authorize(Approval::Ask, &mut rules, &bash("cargo test --lib"), "", &mut ui).is_ok());
        assert!(authorize(Approval::Ask, &mut rules, &bash("cargo test --test agent"), "", &mut ui).is_ok());
        assert_eq!(ui.0, 1);
        assert!(authorize(Approval::Ask, &mut rules, &bash("cargo build"), "", &mut ui).is_ok());
        assert_eq!(ui.0, 2);
        assert!(authorize(Approval::Ask, &mut rules, &bash("cargo test; rm x"), "", &mut ui).is_ok());
        assert_eq!(ui.0, 3, "compound command cannot inherit a prefix grant");
        assert_eq!(rules.len(), 2, "compound command cannot create a blanket grant");
        rules.push(PermissionRule { effect:Effect::Deny, tool:"bash".into(), command_prefix:vec!["cargo".into(), "test".into()] });
        assert!(matches!(authorize(Approval::Auto, &mut rules, &bash("cargo test"), "", &mut ui), Err(Denial::Rule)));
        assert!(matches!(authorize(Approval::Readonly, &mut rules, &bash("cargo build"), "", &mut ui), Err(Denial::ReadOnly)));
    }

    #[test]
    fn project_rules_round_trip_and_require_scoped_shell_grants() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.toml");
        let rules = vec![PermissionRule { effect:Effect::Deny, tool:"bash".into(), command_prefix:vec!["git".into(), "push".into()] }];
        save(&path, &rules).unwrap();
        assert_eq!(load(&path).unwrap(), rules);
        assert!(PermissionRule { effect:Effect::Allow, tool:"bash".into(), command_prefix:vec![] }.validate().is_err());
    }
}
