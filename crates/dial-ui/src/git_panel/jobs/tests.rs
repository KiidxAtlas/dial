use super::*;

#[test]
fn retained_outcomes_are_latest_per_repository_and_evict_old_records() {
    let mut jobs = Jobs::default();
    let root = PathBuf::from("repository");
    jobs.finish(root.clone(), 1, "old failure".into(), true);
    jobs.finish(root.clone(), 2, "new completion".into(), false);
    let notice = jobs.notice(&root).unwrap();
    assert_eq!(notice.message, "new completion");
    assert!(!notice.error);
    assert!(!notice.running);
    for index in 0..MAX_OUTCOMES {
        jobs.finish(
            PathBuf::from(format!("other-{index}")),
            index as u64 + 3,
            "done".into(),
            false,
        );
    }
    assert!(jobs.notice(&root).is_none(), "oldest outcome is evicted");
    assert_eq!(jobs.completed.len(), MAX_OUTCOMES);
}

#[test]
fn retained_outcomes_bound_utf8_messages_and_total_owned_payload_bytes() {
    let mut jobs = Jobs::default();
    let message = "界".repeat(MAX_MESSAGE_BYTES);
    for index in 0..MAX_OUTCOMES * 2 {
        let root = PathBuf::from(format!("{index}-{}", "p".repeat(MAX_MESSAGE_BYTES)));
        jobs.finish(root.clone(), index as u64, message.clone(), true);
        let notice = jobs.notice(&root).unwrap();
        assert!(notice.message.len() <= MAX_MESSAGE_BYTES);
        assert!(notice.message.capacity() <= MAX_MESSAGE_BYTES);
        assert!(notice.message.ends_with('…'));
        assert!(jobs.completed_bytes <= MAX_OUTCOME_BYTES);
        assert!(jobs.completed.len() <= MAX_OUTCOMES);
    }
    let actual: usize = jobs
        .completed
        .iter()
        .map(|(root, notice)| record_bytes(root, notice))
        .sum();
    assert_eq!(jobs.completed_bytes, actual);
    assert!(
        jobs.completed.len() < MAX_OUTCOMES,
        "byte budget evicts before the record limit"
    );
}
