//! Native RPC checkpoint methods served by the actor: `list_checkpoints`, `checkpoint_files`,
//! `checkpoint_diff`, `restore_checkpoint_files` and `undo`. They call the same storage and
//! rewind owners as the terminal's `/review`, `/changes` and `/undo`, through the active agent's
//! checkpoint journal; a separate journal handle would race the running turn's pre-images.
use super::coded;
use crate::storage::checkpoint::Checkpoints;
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde_json::{Value, json};

fn invalid(message: impl std::fmt::Display) -> anyhow::Error {
    coded(ErrorCode::InvalidParams, message)
}

fn journal(checkpoints: Option<&std::sync::Arc<Checkpoints>>) -> Result<&Checkpoints> {
    checkpoints
        .map(|checkpoints| &**checkpoints)
        .ok_or_else(|| {
            coded(
                ErrorCode::NotFound,
                "checkpoints are not enabled for this session",
            )
        })
}

fn turn(params: &Value) -> Result<u64> {
    params["turn"]
        .as_u64()
        .filter(|turn| *turn > 0)
        .ok_or_else(|| invalid("params.turn must be a positive integer"))
}

/// The turn's review, or `not_found` when the turn has no live checkpoint.
fn review(checkpoints: &Checkpoints, turn: u64) -> Result<crate::storage::checkpoint::TurnReview> {
    if !checkpoints.list().iter().any(|info| info.turn == turn) {
        return Err(coded(
            ErrorCode::NotFound,
            format!("turn {turn} has no checkpoint"),
        ));
    }
    checkpoints.review_turn(turn)
}

/// Checkpointed turns that changed files, newest first. A session without checkpoints has none.
pub(super) fn list(checkpoints: Option<&std::sync::Arc<Checkpoints>>) -> Value {
    let turns: Vec<Value> = checkpoints
        .map(|checkpoints| checkpoints.list())
        .unwrap_or_default()
        .into_iter()
        .map(|info| json!({"turn":info.turn,"label":info.label,"files":info.files}))
        .collect();
    json!({"checkpoints": turns})
}

/// Files one turn changed, with writer attribution and overlap signals.
pub(super) fn files(
    checkpoints: Option<&std::sync::Arc<Checkpoints>>,
    params: &Value,
) -> Result<Value> {
    let review = review(journal(checkpoints)?, turn(params)?)?;
    let files: Vec<Value> = review
        .files
        .iter()
        .map(|file| {
            json!({
                "path": file.path,
                "writers": file.writers,
                "later_turns": file.later_turns,
                "overlap": file.overlap(),
            })
        })
        .collect();
    Ok(json!({"turn":review.turn,"label":review.label,"files":files,"skipped":review.skipped}))
}

/// The diff restoring one file would apply and the token that binds it to the current bytes.
pub(super) fn diff(
    checkpoints: Option<&std::sync::Arc<Checkpoints>>,
    params: &Value,
) -> Result<Value> {
    let checkpoints = journal(checkpoints)?;
    let turn = turn(params)?;
    let path = params["path"]
        .as_str()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| invalid("checkpoint_diff requires params.path"))?;
    if !review(checkpoints, turn)?
        .files
        .iter()
        .any(|f| f.path == path)
    {
        return Err(coded(
            ErrorCode::NotFound,
            format!("{path} is not in turn {turn}'s checkpoint"),
        ));
    }
    let (diff, token) = checkpoints.review_file(turn, path)?;
    Ok(json!({"turn":turn,"path":path,"diff":diff,"token":token}))
}

/// Restores reviewed files. Unknown files are `not_found`; a stale token or an unconfirmed
/// overlap is `conflict` and nothing is written.
pub(super) fn restore(
    checkpoints: Option<&std::sync::Arc<Checkpoints>>,
    params: &Value,
) -> Result<Value> {
    let checkpoints = journal(checkpoints)?;
    let turn = turn(params)?;
    let overlap_confirmed = match params.get("overlap_confirmed").filter(|v| !v.is_null()) {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| invalid("overlap_confirmed must be a boolean"))?,
        None => false,
    };
    let entries = params["files"]
        .as_array()
        .filter(|files| !files.is_empty())
        .ok_or_else(|| invalid("restore_checkpoint_files requires a non-empty params.files"))?;
    let mut items = Vec::with_capacity(entries.len());
    for entry in entries {
        let (Some(path), Some(token)) = (entry["path"].as_str(), entry["token"].as_str()) else {
            return Err(invalid("each file needs string path and token"));
        };
        items.push((path.to_owned(), token.to_owned()));
    }
    let review = review(checkpoints, turn)?;
    for (path, token) in &items {
        let file = review
            .files
            .iter()
            .find(|file| file.path == *path)
            .ok_or_else(|| {
                coded(
                    ErrorCode::NotFound,
                    format!("{path} is not in turn {turn}'s checkpoint"),
                )
            })?;
        if let Some(reason) = file.overlap()
            && !overlap_confirmed
        {
            return Err(coded(
                ErrorCode::Conflict,
                format!("{path} overlaps other work ({reason}); confirm with overlap_confirmed"),
            ));
        }
        let (_, current) = checkpoints.review_file(turn, path)?;
        if current != *token {
            return Err(coded(
                ErrorCode::Conflict,
                format!("{path} changed since it was reviewed; read checkpoint_diff again"),
            ));
        }
    }
    // The owner compares every token again under its lock immediately before writing.
    let restored = checkpoints.restore_files(turn, &items, overlap_confirmed)?;
    Ok(json!({"turn":turn,"restored":restored}))
}

/// The number of user turns `undo` drops (default 1).
pub(super) fn undo_turns(params: &Value) -> Result<usize> {
    match params.get("turns").filter(|value| !value.is_null()) {
        None => Ok(1),
        Some(value) => value
            .as_u64()
            .filter(|turns| *turns > 0)
            .and_then(|turns| usize::try_from(turns).ok())
            .ok_or_else(|| invalid("turns must be a positive integer")),
    }
}

/// Wire form of an undo that started, or the coded refusal of one that did not.
pub(super) fn undo_result(
    turns: usize,
    result: std::result::Result<crate::rewind::UndoOutcome, crate::rewind::UndoRefusal>,
) -> Result<Value> {
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(crate::rewind::UndoRefusal::Range(error)) => {
            return Err(coded(ErrorCode::InvalidParams, error));
        }
        Err(crate::rewind::UndoRefusal::Gate(error)) => {
            return Err(coded(
                ErrorCode::Refused,
                format!("{error:#}; files and conversation kept"),
            ));
        }
    };
    let files = outcome.files.as_ref().map(|report| {
        json!({"restored":report.restored,"removed":report.removed,"warnings":report.warnings})
    });
    Ok(json!({
        "turns": turns,
        "files": files,
        "rollback_error": outcome.rollback_error,
        "dropped": outcome.dropped,
        "note": outcome.note,
        "rewind_error": outcome.rewind_error,
        "cleanup_error": outcome.cleanup_error,
        "conversation_kept": outcome.conversation_kept(),
    }))
}

#[cfg(test)]
mod tests;
