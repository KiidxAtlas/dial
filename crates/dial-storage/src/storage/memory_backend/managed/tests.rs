use super::*;

#[test]
fn committed_skill_at_body_limit_loads_completely() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let directory = root(&layout).unwrap();
    let db = database(&directory).unwrap();
    let body = "é".repeat(skills::MAX_BODY_BYTES / 2);
    let document = format!("---\nname: limit\ndescription: Boundary check\n---\n\n{body}\n");
    let generation = directory
        .child("generations")
        .unwrap()
        .child("v1")
        .unwrap()
        .child("limit")
        .unwrap();
    generation.write("SKILL.md", document.as_bytes()).unwrap();
    db.execute(
        "INSERT INTO skills(name,description,body,generated,files,revision,version) VALUES(?1,?2,?3,1,'{}',1,'v1')",
        params!["limit", "Boundary check", body],
    )
    .unwrap();
    drop(db);

    let discovered = discover(&layout).unwrap();
    assert_eq!(discovered.len(), 1);
    let content = read_skill(&layout, &discovered[0], None).unwrap().unwrap();
    assert_eq!(content.text, document);
    assert!(!content.truncated);
    let loaded = skills::skill_tool::call(&layout, &json!({"name": "limit"}));
    assert!(!loaded.is_error, "{}", loaded.text);
    assert!(loaded.text.contains(&body));
    assert!(!loaded.text.contains("[truncated"));
}
