use super::*;
use crate::agent::modes::goal::State;
use serde_json::json;

fn goal() -> Goal {
    let mut state = State::default();
    state.start(
        "Ship the parser\n\n## Success criteria\n- tests pass",
        Some(1000),
    );
    state.account(250, 42);
    state.goal.unwrap()
}

#[test]
fn render_mirrors_the_goal_todos_and_working_state_and_skips_empty_sections() {
    let mut todo = TodoList::new();
    assert!(
        !todo
            .call(&json!({"op":"append","tasks":[{"content":"Write the lexer"},{"content":"Add tests"}]}))
            .is_error
    );
    let state = json!({
        "goal":"Parser ships with tests",
        "deliverables":[{"id":"lexer","description":"Lexer handles comments","status":"satisfied","evidence_refs":["check:unit"]}],
        "open_work":["error recovery"],
        "decisions":[],
    });
    let text = render(&goal(), &todo, &state);
    assert!(text.starts_with("# Goal progress\n"), "{text}");
    assert!(
        text.contains("Status: active. 250 of 1000 tokens used (750 remaining); 42 seconds."),
        "{text}"
    );
    assert!(
        text.contains("## Objective\n\nShip the parser\n\n## Success criteria\n- tests pass\n"),
        "{text}"
    );
    assert!(text.contains("## Todos\n\n```text\n"), "{text}");
    assert!(
        text.contains("Write the lexer") && text.contains("Add tests"),
        "{text}"
    );
    assert!(
        text.contains("- [satisfied] lexer: Lexer handles comments (evidence: check:unit)"),
        "{text}"
    );
    assert!(text.contains("### Open work\n\n- error recovery"), "{text}");
    assert!(
        !text.contains("### Decisions"),
        "empty sections are left out: {text}"
    );

    let bare = render(&goal(), &TodoList::new(), &json!(null));
    assert!(
        !bare.contains("## Todos") && !bare.contains("## Working state"),
        "{bare}"
    );
}

#[test]
fn write_creates_the_file_inside_the_workspace_and_skips_unchanged_content() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    assert!(write(&workspace, ".dial/progress.md", "one\n").unwrap());
    let path = dir.path().join(".dial/progress.md");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\n");
    assert!(
        !write(&workspace, ".dial/progress.md", "one\n").unwrap(),
        "unchanged"
    );
    assert!(write(&workspace, ".dial/progress.md", "two\n").unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "two\n");
}

#[test]
fn write_refuses_paths_outside_the_workspace() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let workspace = Workspace::new(&root).unwrap();
    assert!(write(&workspace, "../escape.md", "x").is_err());
    let absolute = outer.path().join("absolute.md");
    assert!(write(&workspace, absolute.to_str().unwrap(), "x").is_err());
    assert!(!outer.path().join("escape.md").exists() && !absolute.exists());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outer.path(), root.join("link")).unwrap();
        assert!(write(&workspace, "link/through.md", "x").is_err());
        assert!(!outer.path().join("through.md").exists());
    }
}
