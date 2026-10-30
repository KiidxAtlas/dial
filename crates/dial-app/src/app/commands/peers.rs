//! `/peers`: bare `/peers` opens the board panel (`ui::overlay::peers`); the subcommands are a
//! text adapter over the agent's existing coordination API. The panel's requests (refresh, clear
//! this session's claims, send a message) come back through `Shell::open`, run here against the
//! same API, and reopen the panel on a fresh snapshot.
use super::CommandSpec;
use crate::agent::{Agent, PeerBoard};
use crate::app::overlay::Which;
use crate::app::{App, Shell, Step};
use crate::ui::overlay::peers::{Claim, PeerAction, PeerRow, PeersView};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

const USAGE: &str = "Usage: /peers [list|send <id> <text>|claim <paths...>|clear]";

pub(super) const COMMANDS: &[CommandSpec] = &[CommandSpec {
    name: "peers",
    description: "show same-checkout sessions, claims and overlaps; message peers or claim paths",
    args_hint: "[list|send <id> <text>|claim <paths...>|clear]",
    subcommands: &["list", "send", "claim", "clear"],
}];

/// What the panel opens on: the snapshot, the peer to select and an outcome to show.
pub(in crate::app) struct PeersPanel {
    pub(in crate::app) view: PeersView,
    pub(in crate::app) selected: Option<String>,
    pub(in crate::app) banner: Option<(String, bool)>,
}

pub(super) fn run(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    if name != "peers" {
        return None;
    }
    if args.trim().is_empty() {
        return Some(match panel(agent, None, None) {
            Ok(which) => shell.open(app, agent, which),
            Err(error) => {
                shell.note("Project peers", error);
                Ok(Step::Done)
            }
        });
    }
    let text = match arguments(args) {
        Ok(args) => agent.peer_coordination(&args).text,
        Err(error) => error.to_string(),
    };
    shell.note("Project peers", text);
    Some(Ok(Step::Done))
}

/// Run a panel request, then the panel to reopen. `Err` carries a message for the transcript when
/// coordination became unavailable, so there is no board to show.
pub(in crate::app) fn act(
    agent: &mut Agent<'_>,
    action: PeerAction,
    selected: Option<String>,
) -> Result<Which, String> {
    let banner = match &action {
        PeerAction::Refresh => None,
        PeerAction::Clear => Some(outcome(
            agent,
            json!({"action": "clear"}),
            "Cleared this session's claims".to_owned(),
        )),
        PeerAction::Send { to, text } => Some(outcome(
            agent,
            json!({"action": "send", "to": to, "text": text}),
            format!(
                "Queued for {}; it reaches that session at its next model request",
                to.get(..8).unwrap_or(to)
            ),
        )),
    };
    panel(agent, selected, banner)
}

fn outcome(agent: &mut Agent<'_>, args: Value, success: String) -> (String, bool) {
    let output = agent.peer_coordination(&args);
    if output.is_error {
        (output.text, true)
    } else {
        (success, false)
    }
}

fn panel(
    agent: &mut Agent<'_>,
    selected: Option<String>,
    banner: Option<(String, bool)>,
) -> Result<Which, String> {
    let board = agent.peer_board().map_err(|error| format!("{error:#}"))?;
    Ok(Which::Peers(Box::new(PeersPanel {
        view: view(board),
        selected,
        banner,
    })))
}

/// The panel's data: claims are marked where they overlap a claim on the other side.
fn view(board: PeerBoard) -> PeersView {
    let claims = |scopes: Vec<String>, overlapping: &[String]| {
        scopes
            .into_iter()
            .map(|path| Claim {
                overlaps: overlapping.contains(&path),
                path,
            })
            .collect()
    };
    PeersView {
        own: PeerRow {
            id: board.id,
            phase: board.phase,
            summary: board.summary,
            claims: claims(board.scopes, &board.overlapping),
        },
        peers: board
            .peers
            .into_iter()
            .map(|peer| PeerRow {
                claims: claims(peer.peer.scopes, &peer.overlapping),
                id: peer.peer.id,
                phase: peer.peer.phase,
                summary: peer.peer.summary,
            })
            .collect(),
        queued: board.queued,
    }
}

fn split_word(text: &str) -> (&str, &str) {
    let text = text.trim();
    match text.find(char::is_whitespace) {
        Some(index) => (&text[..index], text[index..].trim_start()),
        None => (text, ""),
    }
}

fn arguments(args: &str) -> Result<Value> {
    let (action, rest) = split_word(args);
    match action {
        "" | "list" if rest.is_empty() => Ok(json!({"action": "list"})),
        "clear" if rest.is_empty() => Ok(json!({"action": "clear"})),
        "send" => {
            let (to, text) = split_word(rest);
            if to.is_empty() || text.is_empty() {
                bail!(USAGE);
            }
            Ok(json!({"action": "send", "to": to, "text": text}))
        }
        "claim" => {
            let scopes = shlex::split(rest)
                .ok_or_else(|| anyhow!("Unclosed quote in /peers claim paths. {USAGE}"))?;
            if scopes.is_empty() || scopes.iter().any(String::is_empty) {
                bail!(USAGE);
            }
            Ok(json!({"action": "claim", "scopes": scopes}))
        }
        _ => bail!(USAGE),
    }
}

#[cfg(test)]
mod tests;
