use super::*;

#[test]
fn mixed_setbacks_redirect_once_then_stop() {
    let mut ledger = ProgressLedger::default();
    let kinds = [
        Setback::UnusableReply,
        Setback::Truncation,
        Setback::StreamStall,
    ];
    let mut redirects = 0;
    for attempt in 1..STOP_AT {
        match ledger.setback(kinds[attempt as usize % kinds.len()]) {
            Rung::Retry => {}
            Rung::Redirect(note) => {
                redirects += 1;
                assert_eq!(attempt, REDIRECT_AT);
                assert!(note.contains("unusable tool call ×2"), "{note}");
            }
            Rung::Stop(message) => panic!("stopped early at {attempt}: {message}"),
        }
    }
    assert_eq!(redirects, 1, "the redirecting note is sent once");
    match ledger.setback(Setback::Overflow) {
        Rung::Stop(message) => {
            assert!(message.contains("16 recovery attempts"), "{message}");
            assert!(message.contains("context overflow ×1"), "{message}");
        }
        rung => panic!("expected a stop, got {rung:?}"),
    }
}

#[test]
fn progress_starts_the_ladder_over() {
    let mut ledger = ProgressLedger::default();
    for _ in 0..STOP_AT - 1 {
        assert_ne!(
            std::mem::discriminant(&ledger.setback(Setback::NoAnswer)),
            std::mem::discriminant(&Rung::Stop(String::new()))
        );
    }
    ledger.progressed();
    for _ in 1..REDIRECT_AT {
        assert_eq!(ledger.setback(Setback::NoAnswer), Rung::Retry);
    }
    assert!(matches!(
        ledger.setback(Setback::NoAnswer),
        Rung::Redirect(_)
    ));
}
