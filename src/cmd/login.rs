//! `dial login [provider]`: interactive sign-in for OAuth and subscription providers.
//!
//! Shares the exact owner flows and credential stores with `/providers` and RPC `login`, while
//! allowing providers that require typed input (`needs_terminal`) to prompt directly in the
//! terminal.

use anyhow::{Context, Result, bail, ensure};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use dial::config::paths::{Layout, subscription_auth_dir};
use dial::llm::{anthropic_oauth, chatgpt, oauth};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Anthropic,
    OpenAi,
    OAuth(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderRow {
    pub(crate) provider: &'static str,
    pub(crate) name: &'static str,
    pub(crate) signed_in: bool,
    pub(crate) identity: Option<String>,
    pub(crate) needs_terminal: bool,
}

/// Resolve a user-supplied provider name or common alias to its sign-in owner.
pub(crate) fn resolve_target(raw: &str) -> Result<Target> {
    let trimmed = raw.trim();
    match trimmed {
        "anthropic" | "claude" => Ok(Target::Anthropic),
        "openai" | "chatgpt" | "codex" | "openai-codex" => Ok(Target::OpenAi),
        other => {
            if let Some(flow) = oauth::flow(other) {
                Ok(Target::OAuth(flow.id))
            } else {
                let mut known = vec!["anthropic", "openai"];
                known.extend(oauth::flows().map(|flow| flow.id));
                bail!(
                    "unknown sign-in provider {other:?}; known providers: {}",
                    known.join(", ")
                )
            }
        }
    }
}

/// Collect every sign-in provider and its current status without touching the network.
pub(crate) fn list_providers(user_dir: &Path) -> Vec<ProviderRow> {
    let sub_dir = subscription_auth_dir(user_dir).ok();
    let anthropic_signed_in = sub_dir
        .as_ref()
        .is_some_and(|dir| anthropic_oauth::signed_in(dir));
    let openai_signed_in = sub_dir
        .as_ref()
        .is_some_and(|dir| chatgpt::resolve_account(dir, None).is_ok());
    let mut rows = vec![
        ProviderRow {
            provider: "anthropic",
            name: "Claude subscription",
            signed_in: anthropic_signed_in,
            identity: None,
            needs_terminal: false,
        },
        ProviderRow {
            provider: "openai",
            name: "ChatGPT subscription",
            signed_in: openai_signed_in,
            identity: None,
            needs_terminal: false,
        },
    ];
    rows.extend(oauth::flows().map(|flow| ProviderRow {
        provider: flow.id,
        name: flow.name,
        signed_in: oauth::signed_in(user_dir, flow.id),
        identity: oauth::identity(user_dir, flow.id),
        needs_terminal: flow.needs_terminal,
    }));
    rows
}

pub(crate) fn render_providers(rows: &[ProviderRow]) -> String {
    let width = rows.iter().map(|r| r.provider.len()).max().unwrap_or(0);
    let mut out = String::new();
    for row in rows {
        let mark = if row.signed_in { "✓" } else { "·" };
        let status = match (&row.identity, row.signed_in) {
            (Some(id), true) => format!("signed in ({})", clean(id)),
            (None, true) => "signed in".to_owned(),
            (_, false) => "not signed in".to_owned(),
        };
        let pad = " ".repeat(width.saturating_sub(row.provider.len()));
        out.push_str(&format!(
            "  {mark} {}{pad}  {}  ({status})\n",
            row.provider, row.name
        ));
    }
    out.push_str("\nRun `dial login <provider>` to sign in.\n");
    out
}

pub(crate) fn json_providers(rows: &[ProviderRow]) -> serde_json::Value {
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "provider": row.provider,
                "name": row.name,
                "signed_in": row.signed_in,
                "identity": row.identity,
                "needs_terminal": row.needs_terminal,
            })
        })
        .collect();
    serde_json::json!({ "providers": items })
}

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

pub(crate) fn run(layout: &Layout, provider: Option<&str>, json: bool) -> Result<()> {
    let user_dir = layout.user_dir.as_path();
    let Some(provider) = provider else {
        let rows = list_providers(user_dir);
        if json {
            println!("{}", serde_json::to_string_pretty(&json_providers(&rows))?);
        } else {
            print!("{}", render_providers(&rows));
        }
        return Ok(());
    };
    ensure!(!json, "--json is only supported when listing providers");
    let target = resolve_target(provider)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let ui = TerminalInteraction {
        cancel: Arc::clone(&cancel),
    };
    let message = match target {
        Target::Anthropic => {
            let dir = subscription_auth_dir(user_dir)?;
            let login = oauth::Login { ui: &ui, cancel };
            anthropic_oauth::login(&dir, &login)?
        }
        Target::OpenAi => {
            let dir = subscription_auth_dir(user_dir)?;
            let login = oauth::Login { ui: &ui, cancel };
            chatgpt::login(&dir, None, &login)?
        }
        Target::OAuth(id) => oauth::login(user_dir, id, &ui, cancel)?,
    };
    println!("{message}");
    Ok(())
}

struct TerminalInteraction {
    cancel: Arc<AtomicBool>,
}

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode().context("enabling terminal raw mode for secret input")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn format_prompt(message: &str, placeholder: Option<&str>, allow_empty: bool) -> String {
    let mut out = message.to_owned();
    if let Some(hint) = placeholder.filter(|h| !h.is_empty()) {
        out.push_str(&format!(" [{hint}]"));
    } else if allow_empty {
        out.push_str(" (optional)");
    }
    out.push_str(": ");
    out
}

fn read_line_from<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    message: &str,
    placeholder: Option<&str>,
    allow_empty: bool,
) -> Result<String> {
    write!(writer, "{}", format_prompt(message, placeholder, allow_empty))?;
    writer.flush()?;
    let mut line = String::new();
    let bytes = reader.read_line(&mut line)?;
    if bytes == 0 && !allow_empty {
        bail!("sign-in input cancelled (EOF)");
    }
    let trimmed = line.trim().to_owned();
    if trimmed.is_empty() && !allow_empty {
        bail!("{message}: input cannot be empty");
    }
    Ok(trimmed)
}

fn read_secret_tty(
    message: &str,
    placeholder: Option<&str>,
    allow_empty: bool,
    cancel: &AtomicBool,
) -> Result<String> {
    let mut err = io::stderr();
    write!(err, "{}", format_prompt(message, placeholder, allow_empty))?;
    err.flush()?;
    let _guard = RawModeGuard::enter()?;
    let mut buf = String::new();
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = write!(err, "\r\n");
            let _ = err.flush();
            bail!("sign-in was cancelled");
        }
        match event::read().context("reading secret input from terminal")? {
            Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            }) => match code {
                KeyCode::Enter => {
                    let _ = write!(err, "\r\n");
                    let _ = err.flush();
                    break;
                }
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    cancel.store(true, Ordering::SeqCst);
                    let _ = write!(err, "\r\n");
                    let _ = err.flush();
                    bail!("sign-in was cancelled");
                }
                KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                    let _ = write!(err, "\r\n");
                    let _ = err.flush();
                    if buf.is_empty() {
                        if allow_empty {
                            break;
                        }
                        bail!("sign-in input cancelled");
                    }
                    break;
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(ch) if !modifiers.contains(KeyModifiers::CONTROL) => {
                    buf.push(ch);
                }
                _ => {}
            },
            Event::Paste(text) => {
                buf.push_str(text.trim_end_matches(['\r', '\n']));
            }
            _ => {}
        }
    }
    let trimmed = buf.trim().to_owned();
    if trimmed.is_empty() && !allow_empty {
        bail!("{message}: input cannot be empty");
    }
    Ok(trimmed)
}

impl oauth::Interaction for TerminalInteraction {
    fn auth(&self, url: &str, instructions: Option<&str>) {
        let mut err = io::stderr();
        match instructions {
            Some(text) => {
                let _ = writeln!(err, "{text}");
                let _ = writeln!(err, "Open in your browser: {url}");
            }
            None => {
                let _ = writeln!(err, "Continue sign-in in your browser: {url}");
            }
        }
        let _ = err.flush();
    }

    fn progress(&self, message: &str) {
        let mut err = io::stderr();
        let _ = writeln!(err, "{message}");
        let _ = err.flush();
    }

    fn prompt(
        &self,
        message: &str,
        placeholder: Option<&str>,
        allow_empty: bool,
    ) -> Result<String> {
        let stdin = io::stdin();
        let mut lock = stdin.lock();
        let mut err = io::stderr();
        read_line_from(&mut lock, &mut err, message, placeholder, allow_empty)
    }

    fn prompt_secret(
        &self,
        message: &str,
        placeholder: Option<&str>,
        allow_empty: bool,
    ) -> Result<String> {
        if io::stdin().is_terminal() {
            read_secret_tty(message, placeholder, allow_empty, &self.cancel)
        } else {
            self.prompt(message, placeholder, allow_empty)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{Cli, Command};
    use clap::Parser;
    use std::io::Cursor;

    #[test]
    fn cli_parses_login_with_and_without_provider() {
        let bare = Cli::try_parse_from(["dial", "login"]).unwrap();
        assert!(matches!(
            bare.command,
            Some(Command::Login {
                provider: None,
                json: false
            })
        ));

        let listed = Cli::try_parse_from(["dial", "login", "--json"]).unwrap();
        assert!(matches!(
            listed.command,
            Some(Command::Login {
                provider: None,
                json: true
            })
        ));

        let target = Cli::try_parse_from(["dial", "login", "github-copilot"]).unwrap();
        assert!(matches!(
            target.command,
            Some(Command::Login {
                provider: Some(ref p),
                json: false
            }) if p == "github-copilot"
        ));
    }

    #[test]
    fn resolve_target_handles_subscriptions_aliases_and_oauth_flows() {
        assert_eq!(resolve_target("anthropic").unwrap(), Target::Anthropic);
        assert_eq!(resolve_target("claude").unwrap(), Target::Anthropic);
        assert_eq!(resolve_target("openai").unwrap(), Target::OpenAi);
        assert_eq!(resolve_target("chatgpt").unwrap(), Target::OpenAi);
        assert_eq!(resolve_target("codex").unwrap(), Target::OpenAi);
        for flow in oauth::flows() {
            assert_eq!(
                resolve_target(flow.id).unwrap(),
                Target::OAuth(flow.id),
                "flow {}",
                flow.id
            );
        }
        let err = resolve_target("nonexistent-provider").unwrap_err().to_string();
        assert!(err.contains("anthropic"), "{err}");
        assert!(err.contains("openai"), "{err}");
    }

    #[test]
    fn list_and_render_providers_cover_subscriptions_and_oauth_flows() {
        let dir = tempfile::tempdir().unwrap();
        let rows = list_providers(dir.path());
        assert!(rows.iter().any(|r| r.provider == "anthropic" && !r.signed_in));
        assert!(rows.iter().any(|r| r.provider == "openai" && !r.signed_in));
        assert!(rows.len() >= 2 + oauth::flows().count());

        let text = render_providers(&rows);
        assert!(text.contains("anthropic"), "{text}");
        assert!(text.contains("Run `dial login <provider>`"), "{text}");

        let json = json_providers(&rows);
        assert_eq!(json["providers"].as_array().unwrap().len(), rows.len());
    }

    #[test]
    fn read_line_from_enforces_allow_empty_and_trims() {
        let mut out = Vec::new();
        let mut input = Cursor::new(b"  token-123  \n");
        let answer =
            read_line_from(&mut input, &mut out, "Paste token", Some("tok_..."), false).unwrap();
        assert_eq!(answer, "token-123");
        assert_eq!(String::from_utf8(out).unwrap(), "Paste token [tok_...]: ");

        let mut out = Vec::new();
        let mut empty = Cursor::new(b"\n");
        assert!(read_line_from(&mut empty, &mut out, "Paste token", None, false).is_err());

        let mut out = Vec::new();
        let mut empty_ok = Cursor::new(b"\n");
        assert_eq!(
            read_line_from(&mut empty_ok, &mut out, "Region", None, true).unwrap(),
            ""
        );
    }
}
