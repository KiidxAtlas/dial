use super::*;
#[test]
fn baseline_records_real_samples_and_serializes_finite_metrics() {
    assert!(measure(0).is_err());
    let baseline = measure(100).unwrap();
    assert_eq!((baseline.schema_version, baseline.samples), (1, 100));
    assert!(
        baseline.raw_append_ns > 0
            && baseline.journaled_append_ns > 0
            && baseline.render_encode_p95_ns > 0
    );
    assert!(baseline.journal_overhead_ratio.is_finite());
    let encoded = serde_json::to_value(baseline).unwrap();
    assert_eq!(encoded["samples"], 100);
    assert!(encoded["journal_overhead_ratio"].is_number());
}
