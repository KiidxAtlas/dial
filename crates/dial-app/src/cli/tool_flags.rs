//! Tool, system-prompt, skill, rule, LSP, PTY and title flags.
//!
//! Parsing and validation are pure. [`apply`] folds the flags that Dial's config
//! overlay can express into a `toml::Table` layer and reports every flag without an
//! honored hook as [`Blocked`] instead of silently dropping it.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Args;

use crate::config::tools::integrations::INTEGRATION_TOOLS;
use crate::config::tools::{BUILTIN, EXTRA_TOOLS, default_tools};

/// Flags accepted by `dial` for tools, system prompt, skills, rules, LSP, PTY and title.
#[derive(Debug, Clone, Default, Args)]
#[command(next_help_heading = crate::cli::help_heading::CONTEXT)]
pub struct ToolFlagArgs {
    /// Restrict tools to a comma-separated list (names are normalized and deduplicated).
    #[arg(long, value_name = "CSV")]
    pub tools: Option<String>,
    /// Disable all tools. Ignored when `--tools` is also given.
    #[arg(long)]
    pub no_tools: bool,
    /// Additional directory the agent may access. Repeatable.
    #[arg(long = "add-dir", value_name = "DIR", help_heading = crate::cli::help_heading::SAFETY)]
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
    /// Candidate provider names requiring exact registry validation by `Agent::set_launch`.
    pub deferred_tools: Vec<String>,
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

/// Canonical tool names known to Dial, plus caller-supplied registered and operator names.
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
        let name = canonical_tool_name(&name).to_owned();
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

/// Canonical name for a renamed tool. `kernel` was merged into `eval` (`language = "py"`), so
/// `--tools kernel` and a `kernel` entry select `eval`. This is the single alias point for CLI
/// tool and integration names.
pub fn canonical_tool_name(name: &str) -> &str {
    if name.eq_ignore_ascii_case("kernel") {
        "eval"
    } else {
        name
    }
}

/// Integration-gated tools among `tools`, after alias canonicalization. These are the only names
/// that need `config.integrations` entries to be registered.
pub fn integrations_for_tools(tools: &[String]) -> Vec<String> {
    tools
        .iter()
        .map(|name| canonical_tool_name(name))
        .filter(|name| INTEGRATION_TOOLS.contains(name))
        .map(str::to_owned)
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
/// `extra_known_tools` are operator (`[tools.*]`) names and exact names from an already
/// discovered extension/MCP catalog. Configuration keys for servers or extensions are
/// not tool names: callers must not infer names from prefixes or start providers just
/// to validate flags. Unknown names remain errors until an authoritative catalog is available.
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

/// Resolve flags for a launch that will call `Agent::set_launch` after normal provider
/// registration. Unknown API-valid identifiers are candidates, not accepted tools:
/// the launch must carry `deferred_tools` through to the agent before any model request.
/// This performs no provider discovery, process startup, or prefix-based authorization.
pub fn apply_for_launch(
    args: &ToolFlagArgs,
    overlay: &mut toml::Table,
    base_integrations: &[String],
    extra_known_tools: &[String],
) -> Result<EffectiveSettings> {
    let known = known_tool_names(extra_known_tools);
    let deferred: Vec<String> = args.tools.as_deref().map_or_else(Vec::new, |csv| {
        normalize_tool_names(csv, &known)
            .into_iter()
            .filter(|name| {
                !known.contains(name)
                    && name.len() <= 64
                    && name
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            })
            .collect()
    });
    let mut extra = extra_known_tools.to_vec();
    extra.extend(deferred.iter().cloned());
    let mut out = apply(args, overlay, base_integrations, &extra)?;
    out.deferred_tools = deferred;
    Ok(out)
}

/// Set a root-level overlay key.
fn set_value_root(overlay: &mut toml::Table, key: &str, value: toml::Value) {
    overlay.insert(key.to_owned(), value);
}

#[cfg(test)]
mod tests;
