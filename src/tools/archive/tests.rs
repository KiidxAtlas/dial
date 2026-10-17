use super::*;

fn tool(home: &Path, project: &Path) -> ArchiveTool {
    let layout = crate::storage::paths::Layout::with_user_dir(project, home.join("dial")).unwrap();
    ArchiveTool {
        user_dir: layout.user_dir.clone(),
        project_root: layout.project_root.clone(),
        sessions_dir: layout.sessions_dir(),
    }
}

#[test]
fn current_project_reads_need_no_approval_but_other_projects_do() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let tool = tool(home.path(), project.path());
    assert!(!tool.needs_approval(&json!({"action":"sessions"})));
    assert!(!tool.needs_approval(&json!({"action":"prompts","query":"x"})));
    assert!(!tool.needs_approval(&json!({"action":"recaps","project": project.path()})));
    assert!(tool.needs_approval(&json!({"action":"projects"})));
    assert!(tool.needs_approval(&json!({"action":"sessions","project":"*"})));
    assert!(tool.needs_approval(&json!({"action":"search","query":"x","project":"/elsewhere"})));
    assert!(!tool.may_modify_workspace(&json!({"action":"projects"})));
}

#[test]
fn errors_name_the_recovery() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let tool = tool(home.path(), project.path());
    assert!(
        tool.run(&json!({"action":"nope"}))
            .unwrap_err()
            .contains("use one of")
    );
    assert!(
        tool.run(&json!({"action":"session"}))
            .unwrap_err()
            .contains("needs `id`")
    );
    assert!(
        tool.run(&json!({"action":"search"}))
            .unwrap_err()
            .contains("non-empty `query`")
    );
    let empty = tool.run(&json!({"action":"sessions"})).unwrap();
    assert!(empty.text.starts_with("No sessions"));
}
