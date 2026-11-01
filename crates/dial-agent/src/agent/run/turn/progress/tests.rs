use super::*;

#[test]
fn mixed_setbacks_stop_without_a_redirect() {
    let mut ledger = ProgressLedger::default();
    let kinds = [
        Setback::UnusableReply,
        Setback::Truncation,
        Setback::StreamStall,
    ];
    for attempt in 1..STOP_AT {
        assert_eq!(
            ledger.setback(kinds[attempt as usize % kinds.len()]),
            Rung::Retry,
            "attempt {attempt}"
        );
    }
    match ledger.setback(Setback::Overflow) {
        Rung::Stop(message) => {
            assert!(message.contains("16 recovery attempts"), "{message}");
            assert!(message.contains("context overflow ×1"), "{message}");
        }
        rung => panic!("expected a stop, got {rung:?}"),
    }
}

#[test]
fn progress_starts_the_allowance_over() {
    let mut ledger = ProgressLedger::default();
    for _ in 0..STOP_AT - 1 {
        assert_eq!(ledger.setback(Setback::NoAnswer), Rung::Retry);
    }
    ledger.progressed();
    for _ in 1..STOP_AT {
        assert_eq!(ledger.setback(Setback::NoAnswer), Rung::Retry);
    }
    assert!(matches!(ledger.setback(Setback::NoAnswer), Rung::Stop(_)));
}

#[test]
fn failed_batches_and_bookkeeping_cannot_hide_alternating_recovery_debt() {
    use crate::agent::run::engine::RoundProgress;
    let mut ledger = ProgressLedger::default();
    for attempt in 1..=STOP_AT {
        assert_eq!(ledger.tool_round(RoundProgress::Neutral), None);
        let kind = if attempt % 2 == 0 {
            ledger.tool_round(RoundProgress::Stalled).unwrap()
        } else {
            Setback::NoAnswer
        };
        let rung = ledger.setback(kind);
        if attempt == STOP_AT {
            assert!(matches!(rung, Rung::Stop(_)));
        } else {
            assert_eq!(rung, Rung::Retry);
        }
    }
    assert_eq!(ledger.tool_round(RoundProgress::Productive), None);
    let kind = ledger.tool_round(RoundProgress::Stalled).unwrap();
    assert_eq!(ledger.setback(kind), Rung::Retry);
}
