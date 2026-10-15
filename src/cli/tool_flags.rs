//! Tool, system-prompt, skill, rule, LSP, PTY and title flags.
//!
//! Parsing and validation are pure. [`apply`] folds the flags that Dial's config
//! overlay can express into a `toml::Table` layer and reports every flag without an
//! honored hook as [`Blocked`] instead of silently dropping it.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Args;

use crate::tools::{BUILTIN, EXTRA_TOOLS, INTEGRATION_TOOLS, default_tools};

/// Flags accepted by `dial` for tools, system prompt, skills, rules, LSP, PTY and title.
#[derive(Debug, Clone, Default, Args)]
pub struct ToolFlagArgs {
    /// Restrict tools to a comma-separated list (names are normalized and deduplicated).
    #[arg(long, value_name = "CSV")]
    pub tools: Option<String>,
    /// Disable all tools. Ignored when `--tools` is also given.
    #[arg(long)]
    pub no_tools: bool,
    /// Additional directory the agent may access. Repeatable.
    #[arg(long = "add-dir", value_name = "DIR")]
    pub add_dir: Vec<PathBuf>,
    /// Replace the system prompt. A value containing a newline is literal text; otherwise it is a file path.
    #[arg(
        long,
        value_name = "TEXT|FILE",
        conflicts_with = "system_prompt_template"
    )]
    pub system_prompt: Option<String>,
    /// Render the system prompt from a template file.
    #[arg(long, value_name = "FILE", conflicts_with = "system_prompt")]
    pub system_prompt_template: Option<String>,
    /// Text appended to the system prompt.
    #[arg(long, value_name = "TEXT")]
    pub append_system_prompt: Option<String>,
    /// Disable skills for this session.
    #[arg(long)]
    pub no_skills: bool,
    /// Enable only the named skills (comma-separated).
    #[arg(long, value_name = "CSV")]
    pub skills: Option<String>,
    /// Disable rules for this session.
    #[arg(long)]
    pub no_rules: bool,
    /// Disable LSP servers and write-through diagnostics.
    #[arg(long)]
    pub no_lsp: bool,
    /// Disable PTY-backed execution.
    #[arg(long)]
    pub no_pty: bool,
    /// Disable session title generation.
    #[arg(long)]
    pub no_title: bool,
}

/// A flag that could not be honored, with the missing hook named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub flag: &'static str,
    pub reason: String,
}

/// Result of [`apply`]: resolved values plus the flags that need cross-slice work.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EffectiveSettings {
    /// Resolved `--tools` allowlist (after `--no-tools` handling). `None` means unchanged.
    pub tools: Option<Vec<String>>,
    /// Resolved `--add-dir` paths, for the workspace once it accepts extra roots.
    pub add_dirs: Vec<PathBuf>,
    /// Resolved `--system-prompt` text (file contents or literal).
    pub system_prompt: Option<String>,
    /// `--system-prompt-template` file path, unread until the agent loads it.
    pub system_prompt_template: Option<String>,
    /// `--append-system-prompt` text.
    pub append_system_prompt: Option<String>,
    /// Resolved skill allowlist. `Some(vec![])` means no skills; `None` means unchanged.
    pub skills: Option<Vec<String>>,
    /// Flags applied through the overlay but with a remaining gap (see `reason`).
    pub partial: Vec<Blocked>,
    /// Flags with no honored hook; nothing was applied for them.
    pub blocked: Vec<Blocked>,
    /// Flags accepted as no-ops, with the reason.
    pub notes: Vec<Blocked>,
}

/// Canonical tool names known to Dial, plus caller-supplied extension and operator names.
pub fn known_tool_names(extra: &[String]) -> Vec<String> {
    let mut out: Vec<String> = BUILTIN.iter().map(|s| (*s).to_owned()).collect();
    out.extend(default_tools());
    out.extend(INTEGRATION_TOOLS.iter().map(|s| (*s).to_owned()));
    out.extend(EXTRA_TOOLS.iter().map(|s| (*s).to_owned()));
    out.extend(extra.iter().cloned());
    let mut seen = std::collections::HashSet::new();
    out.retain(|name| seen.insert(name.clone()));
    out
}

/// Split a comma-separated list, trimming whitespace and dropping empty items.
pub fn split_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Normalize and deduplicate tool names, preserving first-seen order.
///
/// A name is lowercased only when the lowercased form is a known name; unknown names
/// (for example MCP tools) keep their spelling.
pub fn normalize_tool_names(raw: &str, known: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in split_csv(raw) {
        let lower = name.to_ascii_lowercase();
        let normalized = if known.contains(&lower) { lower } else { name };
        if !out.contains(&normalized) {
            out.push(normalized);
        }
    }
    out
}

/// Reject tool names that are not known. Lists every unknown name at once.
pub fn validate_tool_names(requested: &[String], known: &[String]) -> Result<()> {
    let unknown: Vec<&str> = requested
        .iter()
        .filter(|name| !known.iter().any(|k| k == *name))
        .map(String::as_str)
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    bail!("unknown tool(s) for --tools: {}", unknown.join(", "));
}

/// Integration-gated tools among `tools`. These are the only names that need
/// `config.integrations` entries to be registered.
pub fn integrations_for_tools(tools: &[String]) -> Vec<String> {
    tools
        .iter()
        .filter(|name| INTEGRATION_TOOLS.contains(&name.as_str()))
        .cloned()
        .collect()
}

/// Resolve `--system-prompt`: a value containing a newline is literal text; otherwise it
/// is read as a file, and a failed read falls back to the literal value.
pub fn resolve_prompt_input(raw: &str) -> String {
    if raw.contains('\n') {
        return raw.to_owned();
    }
    std::fs::read_to_string(raw).unwrap_or_else(|_| raw.to_owned())
}

/// `--system-prompt` and `--system-prompt-template` cannot be combined.
pub fn validate_prompt_flags(args: &ToolFlagArgs) -> Result<()> {
    if args.system_prompt.is_some() && args.system_prompt_template.is_some() {
        bail!("--system-prompt and --system-prompt-template cannot be combined");
    }
    Ok(())
}

fn set_value(overlay: &mut toml::Table, table: &str, key: &str, value: toml::Value) {
    let entry = overlay
        .entry(table.to_owned())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let toml::Value::Table(t) = entry {
        t.insert(key.to_owned(), value);
    }
}

fn string_array(items: &[String]) -> toml::Value {
    toml::Value::Array(items.iter().cloned().map(toml::Value::String).collect())
}

/// Fold the flags into `overlay` (a config layer) and return the effective settings.
///
/// `base_integrations` is the current `integrations` list, needed for `--no-lsp` alone.
/// `extra_known_tools` are operator (`[tools.*]`) and extension tool names that are valid
/// for `--tools` in addition to Dial's built-in catalog.
pub fn apply(
    args: &ToolFlagArgs,
    overlay: &mut toml::Table,
    base_integrations: &[String],
    extra_known_tools: &[String],
) -> Result<EffectiveSettings> {
    validate_prompt_flags(args)?;
    let mut out = EffectiveSettings::default();
    let known = known_tool_names(extra_known_tools);

    // Tools: --tools wins over --no-tools; --no-tools alone means an empty allowlist.
    let tool_list = match (&args.tools, args.no_tools) {
        (Some(csv), _) => {
            let names = normalize_tool_names(csv, &known);
            validate_tool_names(&names, &known)?;
            Some(names)
        }
        (None, true) => Some(Vec::new()),
        (None, false) => None,
    };
    let mut integrations = base_integrations.to_vec();
    if let Some(names) = &tool_list {
        set_value(overlay, "harness", "tools", string_array(names));
        integrations = integrations_for_tools(names);
    }
    out.tools = tool_list;

    // LSP is registered only when `integrations` contains `lsp`; removing it disables LSP,
    // including write-through diagnostics (lsp::tools is never called).
    if args.no_lsp {
        integrations.retain(|name| name != "lsp");
    }
    if out.tools.is_some() || args.no_lsp {
        set_value_root(overlay, "integrations", string_array(&integrations));
    }

    if args.no_rules {
        set_value(
            overlay,
            "portable_rules",
            "enabled",
            toml::Value::Boolean(false),
        );
    }

    out.skills = if args.no_skills {
        Some(Vec::new())
    } else {
        args.skills.as_deref().map(split_csv)
    };

    out.add_dirs = args.add_dir.clone();

    if let Some(raw) = &args.system_prompt {
        out.system_prompt = Some(resolve_prompt_input(raw));
    }
    if let Some(path) = &args.system_prompt_template {
        out.system_prompt_template = Some(path.clone());
    }
    if let Some(text) = &args.append_system_prompt {
        out.append_system_prompt = Some(text.clone());
    }

    if args.no_pty {
        set_value(overlay, "shell", "no_pty", toml::Value::Boolean(true));
    }

    if args.no_title {
        out.notes.push(Blocked {
            flag: "--no-title",
            reason: "Dial generates no session titles with a model; no title-generation code \
                     path exists to disable."
                .to_owned(),
        });
    }

    Ok(out)
}

/// Set a root-level overlay key.
fn set_value_root(overlay: &mut toml::Table, key: &str, value: toml::Value) {
    overlay.insert(key.to_owned(), value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known() -> Vec<String> {
        known_tool_names(&[])
    }

    #[test]
    fn split_csv_trims_and_drops_empty() {
        assert_eq!(split_csv(" read , ,bash,"), vec!["read", "bash"]);
        assert!(split_csv(" , ").is_empty());
    }

    #[test]
    fn normalize_dedupes_and_lowercases_only_known_names() {
        let names = normalize_tool_names("READ, read, MyMcpTool, Bash", &known());
        assert_eq!(names, vec!["read", "MyMcpTool", "bash"]);
    }

    #[test]
    fn validate_reports_all_unknown_names() {
        let requested = vec!["read".to_owned(), "nope".to_owned(), "zap".to_owned()];
        let err = validate_tool_names(&requested, &known())
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope") && err.contains("zap"), "{err}");
        assert!(!err.contains("read"), "{err}");
    }

    #[test]
    fn integrations_for_tools_selects_only_gated_names() {
        let tools = vec!["read".to_owned(), "lsp".to_owned(), "web_fetch".to_owned()];
        assert_eq!(integrations_for_tools(&tools), vec!["lsp", "web_fetch"]);
    }

    #[test]
    fn resolve_prompt_input_treats_newline_as_literal() {
        assert_eq!(
            resolve_prompt_input("line one\nline two"),
            "line one\nline two"
        );
    }

    #[test]
    fn resolve_prompt_input_reads_file_and_falls_back_to_literal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt.md");
        std::fs::write(&path, "from file").unwrap();
        assert_eq!(resolve_prompt_input(path.to_str().unwrap()), "from file");
        assert_eq!(resolve_prompt_input("just words"), "just words");
    }

    #[test]
    fn prompt_flags_are_mutually_exclusive() {
        let args = ToolFlagArgs {
            system_prompt: Some("x".into()),
            system_prompt_template: Some("t".into()),
            ..Default::default()
        };
        assert!(validate_prompt_flags(&args).is_err());
        let mut overlay = toml::Table::new();
        assert!(apply(&args, &mut overlay, &[], &[]).is_err());
    }

    #[test]
    fn tools_flag_sets_harness_allowlist_and_integrations() {
        let args = ToolFlagArgs {
            tools: Some("read,lsp".into()),
            no_tools: true,
            ..Default::default()
        };
        let mut overlay = toml::Table::new();
        let out = apply(&args, &mut overlay, &["web_search".into()], &[]).unwrap();
        assert_eq!(out.tools, Some(vec!["read".to_owned(), "lsp".to_owned()]));
        let harness = overlay["harness"].as_table().unwrap();
        assert_eq!(harness["tools"].as_array().unwrap().len(), 2);
        assert_eq!(overlay["integrations"].as_array().unwrap().len(), 1);
        assert!(out.partial.is_empty(), "--tools is a hard allowlist now");
    }

    #[test]
    fn no_tools_alone_empties_allowlist() {
        let args = ToolFlagArgs {
            no_tools: true,
            ..Default::default()
        };
        let mut overlay = toml::Table::new();
        let out = apply(&args, &mut overlay, &["lsp".into()], &[]).unwrap();
        assert_eq!(out.tools, Some(Vec::new()));
        assert!(overlay["integrations"].as_array().unwrap().is_empty());
    }

    #[test]
    fn no_lsp_removes_only_lsp_integration() {
        let args = ToolFlagArgs {
            no_lsp: true,
            ..Default::default()
        };
        let mut overlay = toml::Table::new();
        let base = vec!["lsp".to_owned(), "web_search".to_owned()];
        apply(&args, &mut overlay, &base, &[]).unwrap();
        let got: Vec<&str> = overlay["integrations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(got, vec!["web_search"]);
    }

    #[test]
    fn no_rules_disables_portable_rules() {
        let args = ToolFlagArgs {
            no_rules: true,
            ..Default::default()
        };
        let mut overlay = toml::Table::new();
        apply(&args, &mut overlay, &[], &[]).unwrap();
        assert_eq!(overlay["portable_rules"]["enabled"].as_bool(), Some(false));
    }

    #[test]
    fn no_skills_wins_over_skills() {
        let args = ToolFlagArgs {
            no_skills: true,
            skills: Some("a,b".into()),
            ..Default::default()
        };
        let mut overlay = toml::Table::new();
        let out = apply(&args, &mut overlay, &[], &[]).unwrap();
        assert_eq!(out.skills, Some(Vec::new()));
        assert!(out.blocked.is_empty());
    }

    #[test]
    fn prompt_text_and_skill_list_are_wired() {
        let args = ToolFlagArgs {
            system_prompt: Some("you are terse".into()),
            skills: Some("review".into()),
            ..Default::default()
        };
        let mut overlay = toml::Table::new();
        let out = apply(&args, &mut overlay, &[], &[]).unwrap();
        assert_eq!(out.system_prompt.as_deref(), Some("you are terse"));
        assert_eq!(out.skills, Some(vec!["review".to_owned()]));
        assert!(out.blocked.is_empty());
    }

    #[test]
    fn no_pty_flag_writes_shell_overlay() {
        let args = ToolFlagArgs {
            no_pty: true,
            ..Default::default()
        };
        let mut overlay = toml::Table::new();
        apply(&args, &mut overlay, &[], &[]).unwrap();
        assert_eq!(overlay["shell"]["no_pty"].as_bool(), Some(true));
    }
    #[test]
    fn unapplied_flags_are_reported_not_dropped() {
        let args = ToolFlagArgs {
            add_dir: vec![PathBuf::from("/tmp/x")],
            no_title: true,
            append_system_prompt: Some("more".into()),
            ..Default::default()
        };
        let mut overlay = toml::Table::new();
        let out = apply(&args, &mut overlay, &[], &[]).unwrap();
        let flags: Vec<&str> = out.blocked.iter().map(|b| b.flag).collect();
        assert!(!flags.contains(&"--add-dir"));
        assert!(!flags.contains(&"--append-system-prompt"));
        assert_eq!(out.notes[0].flag, "--no-title");
        assert_eq!(out.add_dirs, vec![PathBuf::from("/tmp/x")]);
    }
}
