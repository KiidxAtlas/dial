//! `session_tree` and `tree_navigate`: the nodes the terminal's `/tree` picker lists for the open
//! session and the leaf move it makes. Both go through the session's own tree owner, so the move
//! is the same appended branch marker: nothing is deleted and the branch being left stays
//! reachable.
use super::coded;
use crate::agent::Agent;
use crate::storage::session::{TreeNode, TreeNodeKind};
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde::Deserialize;
use serde_json::{Value, json};

fn kind(node: &TreeNode) -> &'static str {
    match node.kind {
        TreeNodeKind::User => "user",
        TreeNodeKind::Assistant => "assistant",
        TreeNodeKind::Summary => "summary",
        TreeNodeKind::Other => "other",
    }
}

/// One row per navigable node, in display (depth-first) order.
pub(super) fn nodes(nodes: &[TreeNode]) -> Value {
    Value::Array(
        nodes
            .iter()
            .map(|node| {
                json!({
                    "id": node.id,
                    "parent": node.parent,
                    "depth": node.depth,
                    "kind": kind(node),
                    "text": node.text,
                    "label": node.label,
                    "ts": node.ts,
                    "on_active_branch": node.on_active_branch,
                    "current": node.current,
                    "children": node.children,
                })
            })
            .collect(),
    )
}

/// `session_tree`: the open session's navigable nodes.
pub(super) fn tree(agent: &Agent<'_>) -> Value {
    json!({"session_id": agent.session().id(), "nodes": nodes(&agent.session().tree())})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NavigateRequest {
    entry_id: String,
}

/// `tree_navigate`: continue the open session from node `entry_id`. A user message comes back
/// as `prefill` for the client to put in its composer; it is not sent.
pub(super) fn navigate(agent: &mut Agent<'_>, params: &Value) -> Result<Value> {
    let params: NavigateRequest = serde_json::from_value(params.clone())
        .map_err(|error| coded(ErrorCode::InvalidParams, error))?;
    if params.entry_id.trim().is_empty() {
        return Err(coded(
            ErrorCode::InvalidParams,
            "entry_id must not be empty",
        ));
    }
    if !agent
        .session()
        .tree()
        .iter()
        .any(|node| node.id == params.entry_id)
    {
        return Err(coded(
            ErrorCode::NotFound,
            format!(
                "entry {} is not a node of this session's tree",
                params.entry_id
            ),
        ));
    }
    agent.cancel_background_compaction();
    let navigation = agent
        .session_mut()
        .navigate_to(&params.entry_id)
        .map_err(|error| coded(ErrorCode::Refused, format!("{error:#}")))?;
    if navigation.moved {
        crate::collab::room::sync_host(agent.session());
    }
    Ok(json!({
        "session_id": agent.session().id(),
        "moved": navigation.moved,
        "prefill": navigation.prefill,
    }))
}

#[cfg(test)]
mod tests;
