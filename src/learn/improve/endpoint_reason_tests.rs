use super::*;

#[test]
fn endpoint_skip_points_at_an_on_disk_checkpoint_of_the_same_model() {
    assert_eq!(
        endpoint_unavailable("org/model", None),
        "endpoint models cannot be trained"
    );
    let reason = endpoint_unavailable("org/model", Some("/models/org-model"));
    assert!(reason.contains("/models/org-model"));
    assert!(reason.contains("provider = \"mlx\""));
}
