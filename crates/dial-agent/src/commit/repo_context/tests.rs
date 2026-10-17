use super::*;

#[test]
fn common_scopes_counts_and_orders_by_frequency() {
    let subjects = [
        "feat(git): one",
        "fix(ui): two",
        "fix(git): three",
        "chore: no scope",
        "not a conventional subject (x)",
        "docs(ui): four",
        "fix(ui): five",
    ];
    assert_eq!(common_scopes(&subjects), "ui (3), git (2)");
}

#[test]
fn common_scopes_empty_when_no_scoped_subjects() {
    assert_eq!(common_scopes(&["plain subject", "chore: none"]), "");
}

#[test]
fn common_scopes_ignores_empty_and_non_conventional_prefixes() {
    let subjects = [
        "feat(): empty scope",
        "Update docs (readme): fixed",
        "fix(ui)!: breaking",
    ];
    assert_eq!(common_scopes(&subjects), "ui (1)");
}

#[test]
fn common_scopes_capped_to_budget() {
    let subjects: Vec<String> = (0..COMMON_SCOPES + 2)
        .map(|index| format!("feat(s{index}): x"))
        .collect();
    let refs: Vec<&str> = subjects.iter().map(String::as_str).collect();
    assert_eq!(common_scopes(&refs).split(", ").count(), COMMON_SCOPES);
}

#[test]
fn generation_context_without_git_history_is_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A bare repository with no commits is the boundary: temp directories can live inside
    // another checkout, and git would otherwise discover that repository's history.
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .expect("git init");
    assert!(init.success());
    let context = generation_context(dir.path());
    assert_eq!(context.recent_commits, None);
    assert_eq!(context.common_scopes, None);
}

#[test]
fn members_count_counts_quoted_entries() {
    let toml = "[workspace]\nmembers = [\n  \"a\",\n  \"b\",\n  \"c\",\n]\n";
    assert_eq!(members_count(toml), Some(3));
}

#[test]
fn format_context_renders_workspace_and_framework() {
    let context = RepositoryContext {
        language: Some("Rust".to_owned()),
        framework: Some("Tokio async runtime".to_owned()),
        package_manager: Some("cargo".to_owned()),
        is_monorepo: true,
        package_count: Some(2),
    };
    assert_eq!(
        format_context(&context).as_deref(),
        Some(
            "Language: Rust (workspace, 2 packages)\nFramework: Tokio async runtime\nPackage manager: cargo"
        )
    );
}

#[test]
fn format_context_none_without_language() {
    assert_eq!(format_context(&RepositoryContext::default()), None);
}

#[test]
fn node_dependencies_fall_back_to_quoted_keys() {
    let content = r#"{"name": "x", "vue": "3"}"#;
    let data: serde_json::Value = serde_json::from_str(content).expect("json");
    assert!(node_dependency_names(&data, content).contains(&"vue".to_owned()));
}
