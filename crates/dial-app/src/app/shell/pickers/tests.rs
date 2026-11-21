use super::*;

#[test]
fn only_yolo_rows_state_their_risk() {
    for &value in Approval::NAMES {
        let mode: Approval = serde_json::from_value(serde_json::json!(value)).unwrap();
        let detail = approval_detail(value, mode);
        assert!(detail.starts_with(value), "{detail}");
        assert_eq!(detail.contains("RISK:"), mode == Approval::Yolo, "{detail}");
    }
}
