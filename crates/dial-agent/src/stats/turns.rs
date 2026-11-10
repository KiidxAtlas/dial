//! Turn-level views of the ledger: how user turns ended, how long they ran, and what the user
//! did about them afterwards.
//!
//! A request or tool row says whether one call worked. A turn row says whether the unit the user
//! asked for settled, which is the question the request and tool tables cannot answer. Rows hold
//! outcome labels, counts and durations only.
use super::{Db, NamedCount, sql_u64};
use anyhow::Result;
use rusqlite::params;
use serde::Serialize;

/// One finished user turn of a top-level session.
#[derive(Debug, Clone, Default)]
pub struct TurnRecord {
    pub ts_ms: i64,
    pub session: String,
    pub project: String,
    /// The turn's outcome label (`completed`, `answered`, `incomplete`, `needs_input`,
    /// `blocked` or `failed`).
    pub outcome: String,
    pub steps: u64,
    pub duration_ms: u64,
    pub interrupted: bool,
    pub error_class: Option<String>,
    pub changed_files: u64,
}

/// An explicit user verdict on finished turns (`undo`, `rated_good`, `rated_bad`).
#[derive(Debug, Clone, Default)]
pub struct FeedbackRecord {
    pub ts_ms: i64,
    pub session: String,
    pub project: String,
    pub kind: String,
    pub turns: u64,
}

/// Turn outcomes and user verdicts over a range. Worker sessions are excluded.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TurnTotals {
    pub turns: u64,
    /// Turns per outcome label, most frequent first. An interrupted turn keeps its own outcome
    /// here and is also counted in `interrupted`.
    pub outcomes: Vec<NamedCount>,
    pub interrupted: u64,
    /// Failed turns per provider error class.
    pub error_classes: Vec<NamedCount>,
    /// Turns that ended with at least one checkpointed file change.
    pub changed_files_turns: u64,
    /// Turns the user rewound with `/undo` (`/undo 3` counts 3).
    pub undone_turns: u64,
    pub rated_good: u64,
    pub rated_bad: u64,
    /// `completed` and `answered` turns that were not interrupted, as a share of all turns.
    pub settled_rate: Option<f64>,
    pub median_steps: Option<f64>,
    pub duration_p50_ms: Option<u64>,
    pub duration_p90_ms: Option<u64>,
}

/// Nearest-rank percentile of an ascending slice.
fn percentile(sorted: &[u64], percent: usize) -> Option<u64> {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}

const TOP_LEVEL: &str = "session NOT IN (SELECT session FROM worker_sessions)";

impl Db {
    pub fn record_turn(&mut self, r: &TurnRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO turns(ts, session, project, outcome, steps, duration_ms, interrupted, error_class, changed_files)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                r.ts_ms,
                r.session,
                r.project,
                r.outcome,
                sql_u64(r.steps),
                sql_u64(r.duration_ms),
                r.interrupted,
                r.error_class,
                sql_u64(r.changed_files)
            ],
        )?;
        Ok(())
    }

    pub fn record_feedback(&mut self, r: &FeedbackRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO turn_feedback(ts, session, project, kind, turns) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![r.ts_ms, r.session, r.project, r.kind, sql_u64(r.turns)],
        )?;
        Ok(())
    }

    /// Turn outcomes and user verdicts since `since_ms` (None = all time).
    pub fn turn_totals(&self, since_ms: Option<i64>) -> Result<TurnTotals> {
        let since = since_ms.unwrap_or(i64::MIN);
        let counts = |sql: &str| -> Result<Vec<NamedCount>> {
            let mut stmt = self.conn.prepare(sql)?;
            Ok(stmt
                .query_map([since], |r| {
                    Ok(NamedCount {
                        name: r.get(0)?,
                        count: r.get::<_, i64>(1)?.max(0) as u64,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?)
        };
        let outcomes = counts(&format!(
            "SELECT outcome, COUNT(*) FROM turns WHERE ts >= ?1 AND {TOP_LEVEL}
             GROUP BY outcome ORDER BY COUNT(*) DESC, outcome"
        ))?;
        let error_classes = counts(&format!(
            "SELECT error_class, COUNT(*) FROM turns
             WHERE ts >= ?1 AND error_class IS NOT NULL AND {TOP_LEVEL}
             GROUP BY error_class ORDER BY COUNT(*) DESC, error_class"
        ))?;
        let feedback = counts(&format!(
            "SELECT kind, SUM(turns) FROM turn_feedback WHERE ts >= ?1 AND {TOP_LEVEL} GROUP BY kind"
        ))?;
        let verdict = |kind: &str| {
            feedback
                .iter()
                .find(|row| row.name == kind)
                .map_or(0, |row| row.count)
        };
        let mut steps = Vec::new();
        let mut durations = Vec::new();
        let (mut interrupted, mut settled, mut changed) = (0u64, 0u64, 0u64);
        {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT outcome, steps, duration_ms, interrupted, changed_files FROM turns
                 WHERE ts >= ?1 AND {TOP_LEVEL}"
            ))?;
            let rows = stmt.query_map([since], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?.max(0) as u64,
                    r.get::<_, i64>(2)?.max(0) as u64,
                    r.get::<_, bool>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?;
            for row in rows {
                let (outcome, step_count, duration_ms, was_interrupted, changed_files) = row?;
                steps.push(step_count as f64);
                durations.push(duration_ms);
                interrupted += u64::from(was_interrupted);
                changed += u64::from(changed_files > 0);
                settled += u64::from(
                    !was_interrupted && matches!(outcome.as_str(), "completed" | "answered"),
                );
            }
        }
        durations.sort_unstable();
        let turns = durations.len() as u64;
        Ok(TurnTotals {
            turns,
            outcomes,
            interrupted,
            error_classes,
            changed_files_turns: changed,
            undone_turns: verdict("undo"),
            rated_good: verdict("rated_good"),
            rated_bad: verdict("rated_bad"),
            settled_rate: (turns > 0).then(|| settled as f64 * 100.0 / turns as f64),
            median_steps: super::median(&steps),
            duration_p50_ms: percentile(&durations, 50),
            duration_p90_ms: percentile(&durations, 90),
        })
    }
}

#[cfg(test)]
mod tests;
