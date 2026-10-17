use super::*;

fn inventory() -> Inventory {
    Inventory::new(&Config::default(), &McpLoad::default())
}

#[test]
fn only_install_needs_approval() {
    let tool = inventory();
    assert!(!tool.needs_approval(&json!({"action": "list"})));
    assert!(!tool.needs_approval(&json!({"action": "search", "query": "git"})));
    assert!(!tool.needs_approval(&json!({})));
    assert!(tool.needs_approval(&json!({"action": "install", "id": "memory"})));
    assert!(!tool.may_modify_workspace(&json!({"action": "install"})));
}

#[test]
fn install_refuses_an_empty_id_and_unknown_ids_without_starting_anything() {
    let config = Config::default();
    let empty = install(&config, "  ");
    assert!(empty.is_error);
    assert!(empty.text.contains("needs `id`"), "{}", empty.text);
}

#[test]
fn install_asks_for_the_values_a_curated_server_needs_instead_of_guessing() {
    let config = Config {
        user_dir: std::path::PathBuf::from("/tmp/dial-test-home"),
        project_root: std::path::PathBuf::from("/tmp"),
        ..Default::default()
    };
    let out = install(&config, "github");
    assert!(!out.is_error);
    let data = out.data.expect("structured");
    assert_eq!(data["status"], "needs_input");
    assert_eq!(data["required"][0]["name"], "GITHUB_PERSONAL_ACCESS_TOKEN");
    assert_eq!(data["required"][0]["secret"], true);
}

#[test]
fn install_refuses_a_server_that_is_already_configured() {
    let mut config = Config {
        user_dir: std::path::PathBuf::from("/tmp/dial-test-home"),
        project_root: std::path::PathBuf::from("/tmp"),
        ..Default::default()
    };
    config.mcp.insert("time".into(), McpServerConfig::default());
    let out = install(&config, "time");
    assert!(out.is_error);
    assert!(out.text.contains("already configured"), "{}", out.text);
}
