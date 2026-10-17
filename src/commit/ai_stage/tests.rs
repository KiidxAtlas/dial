use super::*;

#[test]
fn parse_score_reads_part_of_it_from_fenced_json() {
    let reply = "```json\n{\"0\": 0.1, \"1\": 0.2, \"2\": 0.7}\n```";
    let score = parse_score(reply).expect("score");
    assert!((score - 0.7).abs() < 1e-9);
}

#[test]
fn parse_score_normalizes_probabilities() {
    let score = parse_score("{\"0\": 1, \"1\": 1, \"2\": 2}").expect("score");
    assert!((score - 0.5).abs() < 1e-9);
}

#[test]
fn parse_score_rejects_missing_level() {
    let error = parse_score("{\"0\": 0.5, \"2\": 0.5}").expect_err("missing level");
    assert!(error.contains("\"1\""), "{error}");
}

#[test]
fn parse_score_rejects_reply_without_json() {
    assert!(parse_score("I think it matches").is_err());
}

#[test]
fn parse_probabilities_rejects_zero_sum_and_negatives() {
    assert!(parse_probabilities("{\"0\": 0, \"2\": 0}", &["0", "2"]).is_err());
    assert!(parse_probabilities("{\"0\": -1, \"2\": 1}", &["0", "2"]).is_err());
}

#[test]
fn parse_pick_reads_none_probability() {
    let reply = "{\"none\": 0.8, \"c0\": 0.1, \"c1\": 0.1}";
    let none = parse_pick(reply, 2).expect("pick");
    assert!((none - 0.8).abs() < 1e-9);
}

#[test]
fn parse_pick_requires_every_candidate_key() {
    assert!(parse_pick("{\"none\": 0.5, \"c0\": 0.5}", 2).is_err());
}

#[test]
fn verify_candidates_ranks_thresholds_and_caps() {
    let scores = [
        0.2, 0.9, 0.65, 0.61, 0.95, 0.7, 0.8, 0.99, 0.62, 0.66, 0.6, 0.1,
    ];
    let picked = verify_candidates(&scores);
    assert_eq!(picked, vec![7, 4, 1, 6, 5, 9, 2, 8]);
}

#[test]
fn verify_candidates_empty_when_nothing_clears_threshold() {
    assert!(verify_candidates(&[0.1, 0.59]).is_empty());
}

#[test]
fn changed_lines_keeps_only_edits() {
    let lines: Vec<String> = [
        " context",
        "+added",
        "-removed",
        "\\ No newline at end of file",
    ]
    .iter()
    .map(|line| (*line).to_owned())
    .collect();
    assert_eq!(changed_lines(&lines), "+added\n-removed");
}

#[test]
fn bound_marks_truncation() {
    assert_eq!(bound("abcdef", 10), "abcdef");
    assert_eq!(bound("abcdef", 3), "abc\n…");
}

#[test]
fn map_bounded_caps_concurrency_keeps_order_and_stops_on_cancel() {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    let items: Vec<usize> = (0..120).collect();
    let running = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);
    let cancel = AtomicBool::new(false);
    let results = map_bounded(&items, &cancel, |item| {
        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(2));
        running.fetch_sub(1, Ordering::SeqCst);
        *item * 2
    });
    assert!(peak.load(Ordering::SeqCst) <= WORKERS);
    let values: Vec<usize> = results
        .into_iter()
        .map(|slot| slot.expect("every item runs before cancellation"))
        .collect();
    assert_eq!(
        values,
        items.iter().map(|item| item * 2).collect::<Vec<_>>()
    );

    // Once cancelled, no further item is admitted.
    cancel.store(true, Ordering::SeqCst);
    let results = map_bounded(&items, &cancel, |item| *item);
    assert!(results.iter().all(Option::is_none));
}
