//! Dashboard projection shared by the terminal watch pane and native RPC.
use crate::agent;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Peers {
    /// Why the board is unavailable; the rows below are then empty.
    pub error: Option<String>,
    pub scopes: Vec<String>,
    pub overlapping: Vec<String>,
    pub queued: usize,
    pub rows: Vec<PeerRow>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct PeerRow {
    pub id: String,
    pub summary: String,
    pub phase: String,
    pub scopes: Vec<String>,
    /// The model that peer runs; empty when it does not say.
    #[serde(default)]
    pub model: String,
    pub overlapping: Vec<String>,
    /// The recent conversation with this peer, oldest first, bounded to [`PEER_MESSAGES`].
    #[serde(default)]
    pub messages: Vec<PeerMessage>,
}

/// Messages published per peer; older ones stay out of the pane.
const PEER_MESSAGES: usize = 50;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct PeerMessage {
    /// Sent by this session; otherwise received from the peer.
    pub outgoing: bool,
    pub text: String,
    /// Still queued in the recipient's inbox, not evidence of acknowledgment.
    pub unread: bool,
    /// Whether the recipient inbox could be observed. Old snapshots have no receipt evidence.
    #[serde(default)]
    pub receipt_available: bool,
    /// When it was sent, in Unix milliseconds; 0 when unknown.
    #[serde(default)]
    pub at_ms: u64,
    /// Typed by the human in this pane rather than by this session's model.
    #[serde(default)]
    pub by_user: bool,
}

impl Peers {
    pub(crate) fn from_board(board: agent::PeerBoard, messages: &[agent::PeerMessage]) -> Self {
        Peers {
            error: None,
            scopes: board.scopes,
            overlapping: board.overlapping,
            queued: board.queued,
            rows: board
                .peers
                .into_iter()
                .map(|row| {
                    let mut chat: Vec<PeerMessage> = messages
                        .iter()
                        .filter(|message| message.peer == row.peer.id)
                        .map(|message| PeerMessage {
                            outgoing: message.outgoing,
                            text: message.text.clone(),
                            unread: message.unread,
                            receipt_available: message.receipt_available,
                            at_ms: message.at_ms,
                            by_user: message.by_user,
                        })
                        .collect();
                    chat.drain(..chat.len().saturating_sub(PEER_MESSAGES));
                    PeerRow {
                        id: row.peer.id,
                        summary: row.peer.summary,
                        phase: row.peer.phase,
                        scopes: row.peer.scopes,
                        model: row.peer.model,
                        overlapping: row.overlapping,
                        messages: chat,
                    }
                })
                .collect(),
        }
    }
}
