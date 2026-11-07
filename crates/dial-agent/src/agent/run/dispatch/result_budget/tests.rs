use super::*;
use serde_json::json;

#[test]
fn a_reply_budget_clips_the_largest_results_first_and_keeps_a_floor() {
    // 10k free tokens at 30% and 4 bytes per token: 12,000 bytes for the whole reply.
    let budget = ResultBudget::new(30, 20_000, 10_000);
    assert_eq!(
        budget.cap(3_000, &[2_000, 1_000]),
        usize::MAX,
        "everything fits"
    );
    // 1,000 + 2,000 fit; the two 20,000-byte results share the remaining 9,000 equally.
    assert_eq!(budget.cap(20_000, &[1_000, 2_000, 20_000]), 4_500);
    // Small results keep their full text while large siblings are being clipped.
    assert_eq!(budget.cap(1_000, &[20_000, 20_000]), 5_500);
    let mut spent = ResultBudget::new(30, 20_000, 10_000);
    spent.spend(11_000);
    assert_eq!(spent.cap(9_000, &[]), MIN_RESULT_SHARE);
    assert_eq!(
        ResultBudget::new(0, 20_000, 19_999).cap(1 << 20, &[1 << 20]),
        usize::MAX,
        "0 disables the budget"
    );
    let mut config = crate::config::types::EfficiencyConfig::default();
    assert_eq!(ResultBudget::percent(&config), 30);
    config.batch_result_percent = 50;
    assert_eq!(ResultBudget::percent(&config), 50);
    // 10k free tokens at 50%: 20,000 bytes, so two 10,000-byte results fit unclipped.
    assert_eq!(
        ResultBudget::new(50, 20_000, 10_000).cap(10_000, &[10_000]),
        usize::MAX
    );
    config.enabled = false;
    assert_eq!(
        ResultBudget::percent(&config),
        0,
        "the economy switch disables it"
    );
}

#[test]
fn errors_and_mutation_receipts_are_exempt_from_the_reply_budget() {
    let call = |name: &str| ToolCall {
        id: "c".into(),
        name: name.into(),
        arguments: json!({}),
        malformed_arguments: false,
    };
    let ok = ToolOutput::ok("text".into());
    assert!(!budget_exempt(&call("read"), &ok));
    assert!(budget_exempt(&call("read"), &ToolOutput::err("missing")));
    assert!(budget_exempt(&call("edit"), &ok));
    let mut changed = ToolOutput::ok("ran".into());
    changed.execution.changed_paths.push("a".into());
    assert!(budget_exempt(&call("bash"), &changed));
}

#[test]
fn pending_sizes_skip_exempt_results_and_respect_the_per_result_cap() {
    let call = |name: &str| ToolCall {
        id: "c".into(),
        name: name.into(),
        arguments: json!({}),
        malformed_arguments: false,
    };
    let later = [call("read"), call("edit"), call("grep")];
    let outputs = [
        ToolOutput::ok("x".repeat(100)),
        ToolOutput::ok("y".repeat(100)),
        ToolOutput::ok("z".repeat(10)),
    ];
    assert_eq!(pending_sizes(&later, &outputs, 50), vec![50, 10]);
    let budget = ResultBudget::new(30, 20_000, 10_000);
    let big = ToolOutput::ok("r".repeat(20_000));
    assert_eq!(
        budget.limit(&call("read"), &big, 48_000, true, false, &[20_000]),
        6_000
    );
    assert_eq!(
        budget.limit(&call("read"), &big, 48_000, false, false, &[20_000]),
        48_000,
        "no retained artifact, no clipping"
    );
    assert_eq!(
        budget.limit(&call("read"), &big, 48_000, true, true, &[20_000]),
        48_000,
        "duplicates keep their own presentation"
    );
}
