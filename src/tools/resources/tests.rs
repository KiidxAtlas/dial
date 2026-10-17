use super::*;
#[test]
fn routing_preserves_local_selectors_and_refuses_unknown_authority() {
    assert_eq!(scheme("src/main.rs:5-10").unwrap(), Scheme::File);
    assert!(scheme("http://example.com").is_err());
    assert!(scheme("artifact://../x").is_err());
    assert_eq!(
        artifact_id(&format!("artifact://{}", "A".repeat(64))).unwrap(),
        "a".repeat(64)
    );
}
