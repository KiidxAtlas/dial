//! Session listing, export, rename and encrypted sharing.
use crate::args::{Cli, SessionCommand};
use crate::overlay::overlay;
use anyhow::{Result, bail};
use dial::config::{paths::Layout, types::Config};
use dial::storage::session::Session;
use std::path::PathBuf;

pub(crate) fn run(layout: &Layout, all: bool, action: &Option<SessionCommand>) -> Result<()> {
    let mut sessions = if all {
        Session::list_all(layout, usize::MAX)?
    } else {
        Session::list_by_activity(layout, usize::MAX)?
    };
    // Activity order in both views: the most recently written session first.
    sessions.sort_by(|a, b| b.updated.cmp(&a.updated));
    if let Some(action) = action {
        let id = match action {
            SessionCommand::Export { id, .. } | SessionCommand::Rename { id, .. } => id,
        };
        let mut matching = sessions.iter().filter(|s| s.id.starts_with(id));
        let info = matching
            .next()
            .ok_or_else(|| anyhow::anyhow!("no session matching {id:?}"))?;
        if matching.next().is_some() {
            bail!("session prefix {id:?} is ambiguous; use a longer id");
        }
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
        let shown = if all {
            sessions.len()
        } else {
            sessions.len().min(30)
        };
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
        if let Some(notice) = Session::listing_notice(shown, sessions.len()) {
            eprintln!("{notice}");
        }
    }
    Ok(())
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
    let sessions = Session::list_newest(layout, usize::MAX)?;
    let selected = if let Some(id) = id {
        let mut matches = sessions.iter().filter(|session| session.id.starts_with(id));
        let session = matches
            .next()
            .ok_or_else(|| anyhow::anyhow!("no session matching {id:?}"))?;
        if matches.next().is_some() {
            bail!("session prefix {id:?} is ambiguous; use a longer id");
        }
        session
    } else {
        sessions
            .first()
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
