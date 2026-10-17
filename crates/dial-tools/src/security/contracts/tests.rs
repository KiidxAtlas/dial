use super::*;

#[test]
fn canonical_sorts_keys_recursively() {
    let value = serde_json::json!({"b": 1, "a": {"d": [2, {"z": 0, "y": 1}], "c": null}});
    assert_eq!(
        canonical_json(&value),
        r#"{"a":{"c":null,"d":[2,{"y":1,"z":0}]},"b":1}"#
    );
}

#[test]
fn project_key_matches_expected_shape() {
    let key = project_key("/Users/me/dev/my app/");
    assert!(key.starts_with("Users-me-dev-my-app-"), "{key}");
    assert_eq!(key.len(), "Users-me-dev-my-app-".len() + 12);
    assert_eq!(
        project_key("/"),
        format!("project-{}", &sha256_hex(b"")[..12])
    );
}

#[test]
fn fingerprint_ignores_case_and_location_order() {
    let a = Location {
        path: "./src/a.rs".into(),
        start_line: Some(3),
        end_line: None,
        start_column: None,
        end_column: None,
        role: None,
    };
    let b = Location {
        path: "src\\b.rs".into(),
        start_line: Some(1),
        end_line: None,
        start_column: None,
        end_column: None,
        role: None,
    };
    let one = finding_fingerprint(" Rule.X ", "Cat", Some("Anc"), &[a.clone(), b.clone()]);
    let two = finding_fingerprint("rule.x", "cat", Some("anc"), &[b, a]);
    assert_eq!(one, two);
    assert!(one.starts_with("dial-security/v1:sha256:"));
    assert!(finding_id(&one).starts_with("secf_"));
    assert_eq!(finding_id(&one).len(), "secf_".len() + 24);
}
