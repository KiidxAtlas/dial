use super::parse_schedule_add;

#[test]
fn slash_schedule_normalizes_permissions_and_preserves_quoted_prompt() {
    let entry = parse_schedule_add(
        r#"--every 1h --allow read,edit --allow bash --id review "review the project""#,
    )
    .unwrap();
    entry.validate().unwrap();
    assert_eq!(entry.id, "review");
    assert_eq!(entry.prompt, "review the project");
    assert_eq!(entry.allow, ["read", "edit", "bash"]);
    assert_eq!(entry.approval, "readonly");
    assert_eq!(entry.access_description(), "read-only + read,edit,bash");
    let auto = parse_schedule_add("--every 1h --yes review").unwrap();
    assert_eq!(auto.approval, "auto");
    assert_eq!(
        parse_schedule_add("--every 1h -- --yes is prompt text")
            .unwrap()
            .prompt,
        "--yes is prompt text"
    );
}

#[test]
fn slash_schedule_rejects_incomplete_or_invalid_arguments() {
    for bad in [
        "--allow",
        "--every nope review",
        "--allow edit, review",
        "\"unterminated",
    ] {
        assert!(parse_schedule_add(bad).is_err(), "{bad}");
    }
}
