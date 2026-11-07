//! Explicitly approved teaching examples, separate from the target model's measured runs.
use super::{Store, params};
use anyhow::{Context, Result, ensure};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// A retained source request and its successful targets. Retention alone is not approval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionTrainingExample {
    pub version: u32,
    pub source_session: String,
    pub source_turn: String,
    pub source_model: String,
    pub source_state: Option<PathBuf>,
    pub source_trajectory: Option<i64>,
    pub prompt: String,
    pub payload: Value,
}

impl SessionTrainingExample {
    /// Decode historical expanded snapshots or lossless schema-table snapshots to the same
    /// canonical training representation. References are local to this record, never file paths.
    pub fn from_retained(record: Value) -> Result<Self> {
        Self::from_retained_with_tools(record, &serde_json::Map::new())
    }

    /// Expand session-scoped schema references (v3), while retaining v1/v2 compatibility.
    pub fn from_retained_with_tools(
        record: Value,
        session_tools: &serde_json::Map<String, Value>,
    ) -> Result<Self> {
        use sha2::{Digest, Sha256};
        let mut example: Self = serde_json::from_value(record)?;
        if example.version == 1 {
            return Ok(example);
        }
        ensure!(
            matches!(example.version, 2 | 3),
            "unsupported session training snapshot version"
        );
        let sets = if example.version == 3 {
            ensure!(
                example.payload.get("tool_sets").is_none(),
                "session snapshot mixes schema encodings"
            );
            session_tools.clone()
        } else {
            example
                .payload
                .get("tool_sets")
                .and_then(Value::as_object)
                .context("retained snapshot has no tool schema table")?
                .clone()
        };
        for (key, tools) in &sets {
            ensure!(
                tools.is_array()
                    && *key == format!("{:x}", Sha256::digest(tools.to_string().as_bytes())),
                "retained tool schema identity does not match its contents"
            );
        }
        let entries = example
            .payload
            .get_mut("examples")
            .and_then(Value::as_array_mut)
            .context("retained snapshot has no examples")?;
        let mut expanded_bytes = 0usize;
        for entry in entries {
            let item = entry
                .as_object_mut()
                .context("retained example must be an object")?;
            ensure!(
                !item.contains_key("tools"),
                "retained example mixes tool encodings"
            );
            let reference = item
                .remove("tools_ref")
                .context("retained example has no tool reference")?;
            let tools = reference
                .as_str()
                .and_then(|key| sets.get(key))
                .context("retained example has an unknown tool reference")?;
            item.insert("tools".into(), tools.clone());
            expanded_bytes = expanded_bytes.saturating_add(entry.to_string().len());
            ensure!(
                expanded_bytes <= 2 * 1024 * 1024,
                "expanded training snapshot exceeds 2 MiB"
            );
        }
        example
            .payload
            .as_object_mut()
            .context("snapshot payload must be an object")?
            .remove("tool_sets");
        example.version = 1;
        Ok(example)
    }
}

impl Store {
    /// Atomically approve a batch for this target. Original source identities are immutable;
    /// resuming, forking or approving twice must not duplicate or replace an example.
    pub fn approve_session_examples(
        &self,
        examples: &[SessionTrainingExample],
    ) -> Result<(usize, usize)> {
        let tx = self.transaction()?;
        let mut added = 0;
        for example in examples {
            ensure!(
                example.version == 1,
                "unsupported session training snapshot version"
            );
            ensure!(
                [
                    &example.source_session,
                    &example.source_turn,
                    &example.source_model,
                    &example.prompt
                ]
                .iter()
                .all(|s| !s.trim().is_empty()),
                "session training snapshot has an empty identity or prompt"
            );
            ensure!(
                example.source_state.is_some() == example.source_trajectory.is_some(),
                "session training source reference is incomplete"
            );
            ensure!(
                example.payload["examples"]
                    .as_array()
                    .is_some_and(|a| !a.is_empty()),
                "session training snapshot has no examples"
            );
            let payload = serde_json::to_string(example)?;
            ensure!(
                payload.len() <= 2 * 1024 * 1024,
                "session training snapshot exceeds 2 MiB"
            );
            let existing: Option<String> = tx.query_row(
                "SELECT payload FROM session_examples WHERE source_session=?1 AND source_turn=?2",
                params![example.source_session, example.source_turn], |r| r.get(0)
            ).optional()?;
            if let Some(existing) = existing {
                ensure!(
                    serde_json::from_str::<SessionTrainingExample>(&existing)? == *example,
                    "session training source identity already has different contents"
                );
                continue;
            }
            tx.execute("INSERT INTO session_examples(source_session,source_turn,source_model,payload) VALUES(?1,?2,?3,?4)",
                params![example.source_session, example.source_turn, example.source_model, payload])?;
            added += 1;
        }
        tx.commit()?;
        Ok((added, examples.len() - added))
    }

    /// Approved source snapshots, never counted as target-model performance.
    pub fn session_training_examples(&self) -> Result<Vec<(i64, SessionTrainingExample)>> {
        let mut statement = self
            .conn
            .prepare("SELECT id,payload FROM session_examples ORDER BY id")?;
        let rows =
            statement.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (id, payload) = row?;
            Ok((
                id,
                serde_json::from_str(&payload).context("corrupt approved session example")?,
            ))
        })
        .collect()
    }

    /// The latest source verdict. Absence is not a failure, but an absent trajectory is an error.
    pub fn trajectory_score(&self, trajectory: i64) -> Result<Option<f64>> {
        self.conn.query_row(
            "SELECT o.score FROM trajectories t LEFT JOIN outcomes o ON o.trajectory=t.id WHERE t.id=?1",
            [trajectory], |r| r.get(0)
        ).context("source trajectory is unavailable")
    }
}
