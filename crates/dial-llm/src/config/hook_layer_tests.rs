use super::*;

#[test]
fn global_hook_survives_project_hooks() {
    let project_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project_dir.path(), home.path().to_owned()).unwrap();
    let global: toml::Table =
        toml::from_str("[[hooks]]\nevent='pre_tool_use'\ncommand=['global-policy']\n").unwrap();
    let project: toml::Table =
        toml::from_str("[[hooks]]\nevent='post_tool_use'\ncommand=['fmt']\n").unwrap();
    let config = Config::from_layers(&layout, &global, &project, &toml::Table::new()).unwrap();
    let commands: Vec<&str> = config
        .hooks
        .iter()
        .map(|hook| hook.command[0].as_str())
        .collect();
    assert_eq!(commands, ["global-policy", "fmt"]);
}
