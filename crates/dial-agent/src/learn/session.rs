//! Explicit A -> B teaching from retained, completed session requests.
use crate::{
    config::{paths::Layout, types::Config},
    llm::model_config::ModelSelection,
    storage::{
        session::Session,
        store::{SessionApproval, SessionTrainingExample, Store},
    },
};
use anyhow::{Context, Result, ensure};

/// Resolve the target independently of the model that produced the session. A session-only
/// switch must not reuse the source model's database or its default dataset paths.
pub fn target_config(
    layout: &Layout,
    saved: &Config,
    selection: &ModelSelection,
) -> Result<Config> {
    let mut target = saved.with_selection(selection);
    target.state_dir = target.learning_dir(layout);
    if target.state_dir != saved.state_dir {
        for (path, name) in [
            (&mut target.training.train_data, "train.jsonl"),
            (&mut target.training.valid_data, "valid.jsonl"),
        ] {
            // Explicit custom datasets remain explicit; only model-default paths move.
            if *path == saved.state_dir.join("grounded").join(name) {
                *path = target.state_dir.join("grounded").join(name);
            }
        }
    }
    target.validate()?;
    Ok(target)
}

/// Preserve an existing negative source verdict. Source paths in journals are untrusted;
/// only a direct model-state child of this project's private models directory may be read.
pub(super) fn source_approved(config: &Config, example: &SessionTrainingExample) -> Result<bool> {
    match (&example.source_state, example.source_trajectory) {
        (None, None) => Ok(true),
        (Some(state), Some(trajectory)) => {
            let models = config
                .state_dir
                .parent()
                .context("target has no model-state directory")?
                .canonicalize()
                .context("model-state directory is unavailable")?;
            let state = state
                .canonicalize()
                .context("source learning state is unavailable")?;
            ensure!(
                state.parent() == Some(models.as_path()),
                "source learning state is outside this project's models"
            );
            let store = Store::open_read_only(&state)?;
            Ok(store
                .trajectory_score(trajectory)?
                .is_none_or(|score| score >= 100.0))
        }
        _ => anyhow::bail!("session training source reference is incomplete"),
    }
}

/// Approve the active branch's retained snapshots for the selected target. No whole-chat
/// reconstruction or synthetic target-model trajectories are used.
pub fn approve(config: &Config, store: &Store, session: &Session) -> Result<SessionApproval> {
    let records = session.custom_records("training_turn");
    if records.is_empty() {
        // Compatibility for sessions recorded by the pre-snapshot local teaching path.
        return store.approve_session(session.id());
    }
    let mut report = SessionApproval::default();
    let mut examples = Vec::new();
    for (_, record) in records {
        let example: SessionTrainingExample =
            serde_json::from_value(record).context("invalid retained session training snapshot")?;
        if source_approved(config, &example)? {
            examples.push(example);
        } else {
            report.judged += 1;
        }
    }
    (report.approved, report.already) = store.approve_session_examples(&examples)?;
    Ok(report)
}

#[cfg(test)]
mod tests;
