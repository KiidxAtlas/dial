use super::*;

const NOW: i64 = 1_800_000_000_000;

fn cand(id: &str, cwd: &str, age_days: f64) -> Candidate {
    Candidate {
        path: PathBuf::from(format!("/s/{id}.jsonl")),
        session_id: id.to_string(),
        cwd: cwd.to_string(),
        modified_ms: NOW - (age_days * DAY_MS as f64) as i64 - WRITE_GRACE_MS * 2,
    }
}

fn policy(global: usize, per_cwd: usize, cold: u64) -> RetentionPolicy {
    RetentionPolicy {
        cold_archive_after_days: cold,
        retain_newest_global: global,
        retain_newest_per_cwd: per_cwd,
    }
}

fn verdicts(out: &[(Candidate, Verdict)]) -> Vec<(&str, Verdict)> {
    out.iter()
        .map(|(c, v)| (c.session_id.as_str(), *v))
        .collect()
}

#[test]
fn global_protection_keeps_newest_n_regardless_of_cwd() {
    let input = vec![
        cand("a", "/x", 100.0),
        cand("b", "/y", 90.0),
        cand("c", "/z", 80.0),
    ];
    let out = classify(input, &policy(2, 0, 30), NOW);
    assert_eq!(
        verdicts(&out),
        vec![
            ("c", Verdict::KeptNewestGlobal),
            ("b", Verdict::KeptNewestGlobal),
            ("a", Verdict::Archive),
        ]
    );
}

#[test]
fn per_cwd_protection_counts_within_each_cwd() {
    // Newest-first: a(x), b(y), c(x), d(x). Global protection is off, per-cwd keeps the
    // newest one in each cwd, so only c and d are cold.
    let input = vec![
        cand("a", "/x", 10.0),
        cand("b", "/y", 20.0),
        cand("c", "/x", 30.0),
        cand("d", "/x", 40.0),
    ];
    let out = classify(input, &policy(0, 1, 30), NOW);
    assert_eq!(
        verdicts(&out),
        vec![
            ("a", Verdict::KeptNewestPerCwd),
            ("b", Verdict::KeptNewestPerCwd),
            ("c", Verdict::Archive),
            ("d", Verdict::Archive),
        ]
    );
}

#[test]
fn recent_sessions_do_not_consume_protection_slots() {
    let mut fresh = cand("fresh", "/x", 0.0);
    fresh.modified_ms = NOW - 1_000;
    let input = vec![fresh, cand("old", "/x", 60.0)];
    let out = classify(input, &policy(1, 0, 30), NOW);
    assert_eq!(
        verdicts(&out),
        vec![
            ("fresh", Verdict::Recent),
            ("old", Verdict::KeptNewestGlobal)
        ]
    );
}

#[test]
fn cold_threshold_separates_young_from_archived() {
    let input = vec![cand("young", "/x", 5.0), cand("cold", "/x", 45.0)];
    let out = classify(input, &policy(0, 0, 30), NOW);
    assert_eq!(
        verdicts(&out),
        vec![("young", Verdict::Young), ("cold", Verdict::Archive)]
    );
}

#[test]
fn zero_cold_days_archives_every_unprotected_session() {
    let input = vec![cand("a", "/x", 0.5), cand("b", "/x", 0.1)];
    let out = classify(input, &policy(1, 0, 0), NOW);
    assert_eq!(
        verdicts(&out),
        vec![("b", Verdict::KeptNewestGlobal), ("a", Verdict::Archive)]
    );
}

#[test]
fn store_round_trip_and_settings() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let settings = serde_json::json!({"coldArchiveAfterDays": 30});
    let run = store.begin_run(&settings).unwrap();
    let record = ArchiveRecord {
        session_id: "abc".into(),
        original_path: "/a/sessions/1_abc.jsonl".into(),
        archived_path: "/a/sessions/archive/1_abc.jsonl".into(),
        archived_at_ms: 42,
        cwd: "/proj".into(),
        reason: "cold".into(),
    };
    store.record_archive(run, &record).unwrap();
    assert_eq!(store.archived("abc").unwrap(), Some(record.clone()));
    assert_eq!(store.archived("missing").unwrap(), None);
    assert_eq!(store.last_run_settings().unwrap(), Some(settings));
    // Re-archiving the same session replaces its record.
    let mut moved = record.clone();
    moved.archived_at_ms = 99;
    store.record_archive(run, &moved).unwrap();
    assert_eq!(store.archived("abc").unwrap().unwrap().archived_at_ms, 99);
}
