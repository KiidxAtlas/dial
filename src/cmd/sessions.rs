//! Session listing, export, rename and encrypted sharing.
use crate::args::{Cli, SessionCommand};
use crate::overlay::overlay;
use anyhow::{Result, bail};
use dial::config::{paths::Layout, types::Config};
use dial::storage::session::{Session, SessionInfo};
use std::io::IsTerminal;
use std::path::PathBuf;

/// The single session `id` names: an id-prefix lookup that parses only matching journals, or the
/// all-projects listing when `all` asks for it. Missing and ambiguous prefixes are errors.
fn resolve_prefix(layout: &Layout, all: bool, id: &str) -> Result<SessionInfo> {
    let mut matching = if all {
        Session::list_all(layout, usize::MAX)?
            .into_iter()
            .filter(|s| s.id.starts_with(id))
            .collect()
    } else {
        Session::find_prefix(layout, id)?
    }
    .into_iter();
    let info = matching.next().ok_or_else(|| {
        let scope = if all { "" } else { " in this project" };
        anyhow::anyhow!("no session{scope} starts with {id:?}; run `dial sessions` to list ids")
    })?;
    if let Some(other) = matching.next() {
        bail!(
            "session prefix {id:?} matches {} and {}; use a longer id",
            info.id,
            other.id
        );
    }
    Ok(info)
}

pub(crate) fn run(layout: &Layout, all: bool, action: &Option<SessionCommand>) -> Result<()> {
    if let Some(action) = action {
        let id = match action {
            SessionCommand::Export { id, .. } | SessionCommand::Rename { id, .. } => id,
        };
        let info = &resolve_prefix(layout, all, id)?;
        let mut session = match action {
            SessionCommand::Rename { .. } => Session::open(&info.path)?,
            SessionCommand::Export { .. } => Session::read(&info.path)?,
        };
        match action {
            SessionCommand::Rename { title, .. } => {
                session.rename(title)?;
                println!("Renamed {} to {}", info.id, session.title());
            }
            SessionCommand::Export { path, html, .. } => {
                let path = path.clone().unwrap_or_else(|| {
                    PathBuf::from(format!(
                        "session-{}.{}",
                        info.id,
                        if *html { "html" } else { "md" }
                    ))
                });
                let path = if path.is_absolute() {
                    path
                } else {
                    layout.project_root.join(path)
                };
                session.export(&path, *html)?;
                println!("{}", path.display());
            }
        }
    } else {
        let mut sessions = if all {
            Session::list_all(layout, usize::MAX)?
        } else {
            Session::list_by_activity(layout, usize::MAX)?
        };
        // Activity order in both views: the most recently written session first.
        sessions.sort_by(|a, b| b.updated.cmp(&a.updated));
        let shown = if all {
            sessions.len()
        } else {
            sessions.len().min(30)
        };
        if std::io::stdout().is_terminal() {
            let color = std::env::var_os("NO_COLOR").is_none();
            let home = std::env::var("HOME").ok();
            print!(
                "{}",
                table(
                    &sessions[..shown],
                    all,
                    terminal_width(),
                    color,
                    home.as_deref()
                )
            );
        } else {
            // Piped output keeps its stable, script-friendly shape.
            for s in sessions.iter().take(shown) {
                println!(
                    "{}  {}  {:>4} msgs  {}{}",
                    &s.id[..s.id.len().min(8)],
                    s.started,
                    s.messages,
                    s.title,
                    if all {
                        format!("  {}", s.project)
                    } else {
                        String::new()
                    }
                );
            }
        }
        if let Some(notice) = Session::listing_notice(shown, sessions.len()) {
            eprintln!("{notice}");
        }
    }
    Ok(())
}

fn terminal_width() -> usize {
    // Some pseudo-terminals report zero columns; fall back to a common width.
    match crossterm::terminal::size() {
        Ok((cols, _)) if cols >= 40 => usize::from(cols),
        _ => 100,
    }
}

/// The terminal view of `dial sessions`: a header, relative activity, readable titles fitted to
/// `width`, and the commands that act on a listed id.
fn table(
    sessions: &[SessionInfo],
    all: bool,
    width: usize,
    color: bool,
    home: Option<&str>,
) -> String {
    use dial::ui::session_picker::age;
    use dial::ui::statusline::tilde;
    use dial::ui::text::{ellipsize, ellipsize_start, one_line};
    use std::fmt::Write as _;
    let paint = |code: &str, text: &str| {
        if color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    };
    let mut out = String::new();
    if sessions.is_empty() {
        let scope = if all { "" } else { " in this project" };
        let _ = writeln!(
            out,
            "No sessions{scope} yet. Run {} to start one.",
            paint("1", "dial")
        );
        if !all {
            let _ = writeln!(
                out,
                "{}",
                paint(
                    "2",
                    "List every project's sessions with: dial sessions --all"
                )
            );
        }
        return out;
    }
    let project_width = if all { 28 } else { 0 };
    // Id, age, count and their gaps lead each row; the title gets the rest minus one spare
    // column (at least 16), so rows never touch the terminal's last cell.
    const LEAD: usize = 8 + 2 + 10 + 2 + 5 + 2;
    let title_width = width
        .saturating_sub(LEAD + 1 + project_width + usize::from(all) * 2)
        .max(16);
    let mut header = format!("{:<8}  {:<10}  {:>5}  {}", "ID", "UPDATED", "MSGS", "TITLE");
    if all {
        header = format!("{header:<w$}  PROJECT", w = LEAD + title_width);
    }
    let _ = writeln!(out, "{}", paint("1", &header));
    for s in sessions {
        let empty = s.messages == 0;
        let title = one_line(s.title.trim());
        let title = match (title.is_empty(), empty) {
            (true, true) => "(empty session)".to_owned(),
            (true, false) => "(untitled)".to_owned(),
            (false, _) => title,
        };
        let title = ellipsize(&title, title_width, false);
        let mut row = format!(
            "{}  {:<10}  {:>5}  {}",
            paint("36", &s.id[..s.id.len().min(8)]),
            age(&s.updated),
            s.messages,
            if empty {
                paint("2", &title)
            } else {
                title.clone()
            },
        );
        if all {
            let pad = title_width.saturating_sub(dial::ui::text::width(&title));
            let _ = write!(
                row,
                "{}  {}",
                " ".repeat(pad),
                paint(
                    "2",
                    &ellipsize_start(&tilde(&s.project, home), project_width, false)
                )
            );
        }
        let _ = writeln!(out, "{row}");
    }
    let _ = writeln!(
        out,
        "\n{}",
        paint(
            "2",
            "Resume one with dial -r <id> · continue the latest with dial -c · export with dial sessions export <id>"
        )
    );
    out
}

pub(crate) fn share(
    cli: &Cli,
    layout: &Layout,
    id: &Option<String>,
    endpoint: &Option<String>,
    gist: bool,
    whole_journal: bool,
) -> Result<()> {
    let config = Config::load(layout, Some(overlay(cli)?))?;
    let selected = if let Some(id) = id {
        resolve_prefix(layout, false, id)?
    } else {
        Session::list_newest(layout, 1)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("there are no sessions to share"))?
    };
    // Default: the active branch. Abandoned branches and internal records stay local.
    let scope = if whole_journal {
        dial::storage::share::Scope::WholeJournal
    } else {
        dial::storage::share::Scope::ActiveBranch
    };
    let transcript = dial::storage::share::transcript(&selected.path, scope)?;
    let encrypted = dial::storage::share::encrypt_transcript(&transcript)?;
    let link = if gist {
        dial::storage::share::upload_gist(&encrypted)?
    } else {
        let endpoint = endpoint
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                (!config.share.server_url.is_empty()).then(|| config.share.server_url.clone())
            })
            .or_else(|| std::env::var("DIAL_SHARE_SERVER").ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "set [share].server_url or DIAL_SHARE_SERVER, pass --endpoint, or use --gist"
                )
            })?;
        dial::storage::share::upload(&encrypted, &endpoint)?
    };
    println!("{link}");
    Ok(())
}

#[cfg(test)]
mod tests;
