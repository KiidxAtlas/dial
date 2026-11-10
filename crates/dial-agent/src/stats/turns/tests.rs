use super::*;
use crate::stats::{ToolRecord, render_text};

fn turn(session: &str, ts_ms: i64, outcome: &str, steps: u64, duration_ms: u64) -> TurnRecord {
    TurnRecord {
        ts_ms,
        session: session.into(),
        project: "proj-0000aaaa".into(),
        outcome: outcome.into(),
        steps,
        duration_ms,
        ..Default::default()
    }
}

fn feedback(session: &str, kind: &str, turns: u64) -> FeedbackRecord {
    FeedbackRecord {
        ts_ms: 10,
        session: session.into(),
        project: "proj-0000aaaa".into(),
        kind: kind.into(),
        turns,
    }
}

fn names(rows: &[NamedCount]) -> Vec<(&str, u64)> {
    rows.iter()
        .map(|row| (row.name.as_str(), row.count))
        .collect()
}

#[test]
fn percentile_is_nearest_rank() {
    assert_eq!(percentile(&[], 50), None);
    assert_eq!(percentile(&[7], 90), Some(7));
    let values: Vec<u64> = (1..=10).collect();
    assert_eq!(percentile(&values, 50), Some(5));
    assert_eq!(percentile(&values, 90), Some(9));
    assert_eq!(percentile(&values, 100), Some(10));
}

#[test]
fn an_empty_ledger_reports_no_turns_and_no_rates() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_default(dir.path()).unwrap();
    assert_eq!(db.turn_totals(None).unwrap(), TurnTotals::default());
    let report = db.report(None, 10).unwrap();
    assert!(!render_text(&report).contains("Turns "));
}

#[test]
fn turn_totals_count_outcomes_verdicts_and_latency() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open_default(dir.path()).unwrap();
    db.record_turn(&TurnRecord {
        changed_files: 2,
        ..turn("s1", 10, "completed", 4, 1_000)
    })
    .unwrap();
    db.record_turn(&turn("s1", 10, "answered", 2, 2_000))
        .unwrap();
    // Interrupted while otherwise complete: keeps its outcome, but is not settled.
    db.record_turn(&TurnRecord {
        interrupted: true,
        ..turn("s1", 10, "completed", 6, 3_000)
    })
    .unwrap();
    db.record_turn(&TurnRecord {
        error_class: Some("rate_limited".into()),
        ..turn("s2", 10, "failed", 8, 4_000)
    })
    .unwrap();
    db.record_feedback(&feedback("s1", "undo", 2)).unwrap();
    db.record_feedback(&feedback("s1", "undo", 1)).unwrap();
    db.record_feedback(&feedback("s2", "rated_good", 1))
        .unwrap();
    db.record_feedback(&feedback("s2", "rated_bad", 1)).unwrap();

    let totals = db.turn_totals(None).unwrap();
    assert_eq!(totals.turns, 4);
    assert_eq!(
        names(&totals.outcomes),
        vec![("completed", 2), ("answered", 1), ("failed", 1)]
    );
    assert_eq!(totals.interrupted, 1);
    assert_eq!(names(&totals.error_classes), vec![("rate_limited", 1)]);
    assert_eq!(totals.changed_files_turns, 1);
    assert_eq!(
        (totals.undone_turns, totals.rated_good, totals.rated_bad),
        (3, 1, 1)
    );
    assert_eq!(totals.settled_rate, Some(50.0));
    assert_eq!(totals.median_steps, Some(5.0));
    assert_eq!(totals.duration_p50_ms, Some(2_000));
    assert_eq!(totals.duration_p90_ms, Some(4_000));

    let report = db.report(None, 20).unwrap();
    assert_eq!(report.turns, totals);
    let text = render_text(&report);
    assert!(
        text.contains("Turns 4 · settled 50.0%"),
        "turn summary missing:\n{text}"
    );
    assert!(text.contains("interrupted 1 · undone 3 · rated 1 good / 1 bad"));
    assert!(text.contains("Failed turns by cause: 1 rate_limited"));
}

#[test]
fn worker_sessions_and_earlier_ranges_are_excluded() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open_default(dir.path()).unwrap();
    db.record_turn(&turn("top", 100, "completed", 1, 10))
        .unwrap();
    db.record_turn(&turn("top", 5_000, "completed", 1, 10))
        .unwrap();
    db.record_turn(&turn("child", 5_000, "failed", 1, 10))
        .unwrap();
    db.record_feedback(&feedback("child", "undo", 4)).unwrap();
    db.record_worker_session("child", "scout", 1).unwrap();

    let all = db.turn_totals(None).unwrap();
    assert_eq!(names(&all.outcomes), vec![("completed", 2)]);
    assert_eq!(all.undone_turns, 0);
    assert_eq!(db.turn_totals(Some(1_000)).unwrap().turns, 1);
}

#[test]
fn tools_list_error_classes_and_legacy_outcome_rows_migrate() {
    let dir = tempfile::tempdir().unwrap();
    let path = crate::stats::db_path(dir.path());
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "CREATE TABLE tool_calls(id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, session TEXT NOT NULL,
               project TEXT NOT NULL, tool TEXT NOT NULL, duration_ms INTEGER NOT NULL, error INTEGER NOT NULL);
             CREATE TABLE tool_outcomes(call_id INTEGER PRIMARY KEY, status TEXT NOT NULL);
             INSERT INTO tool_calls VALUES(1,1,'old','p','verify',1,1);
             INSERT INTO tool_outcomes VALUES(1,'error');",
        )
        .unwrap();
    let mut db = Db::open(&path).unwrap();
    for class in ["check_failed", "check_failed", "verify_refused"] {
        db.record_tool(&ToolRecord {
            status: Some(crate::tools::ToolStatus::Error),
            ts_ms: 2,
            session: "s".into(),
            tool: "verify".into(),
            error: true,
            error_class: Some(class.into()),
            ..Default::default()
        })
        .unwrap();
    }
    let tools = db.tools(None).unwrap();
    let verify = tools.iter().find(|group| group.tool == "verify").unwrap();
    // The legacy row was never rolled up, so it shows in the outcome statuses only.
    assert_eq!(verify.errors, 3);
    assert_eq!(names(&verify.outcomes), vec![("error", 4)]);
    // The pre-class row stays unclassified rather than being guessed.
    assert_eq!(
        names(&verify.error_classes),
        vec![("check_failed", 2), ("verify_refused", 1)]
    );
    let text = render_text(&db.report(None, 10).unwrap());
    assert!(
        text.contains("[2 check_failed, 1 verify_refused]"),
        "class breakdown missing:\n{text}"
    );
}
