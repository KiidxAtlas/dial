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

#[test]
fn malformed_project_and_launch_hooks_are_rejected() {
    let project_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project_dir.path(), home.path().to_owned()).unwrap();
    let global: toml::Table =
        toml::from_str("[[hooks]]\nevent='pre_tool_use'\ncommand=['global-policy']\n").unwrap();
    for malformed in [
        "hooks='invalid'",
        "hooks={event='pre_tool_use'}",
        "hooks=42",
    ] {
        let hooks: toml::Table = toml::from_str(malformed).unwrap();
        let project_rejected =
            Config::from_layers(&layout, &global, &hooks, &toml::Table::new()).is_err();
        let launch_rejected =
            Config::from_layers(&layout, &global, &toml::Table::new(), &hooks).is_err();
        assert!(
            project_rejected && launch_rejected,
            "{malformed}: project rejected={project_rejected}, launch rejected={launch_rejected}"
        );
    }
}

#[test]
fn hooks_append_and_deduplicate_across_all_layers() {
    let project_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project_dir.path(), home.path().to_owned()).unwrap();
    let global: toml::Table =
        toml::from_str("[[hooks]]\nevent='pre_tool_use'\ncommand=['global-policy']\n").unwrap();
    let project: toml::Table = toml::from_str(
        "[[hooks]]\nevent='pre_tool_use'\ncommand=['global-policy']\n[[hooks]]\nevent='post_tool_use'\ncommand=['fmt']\n",
    )
    .unwrap();
    let overlay: toml::Table = toml::from_str(
        "[[hooks]]\nevent='post_tool_use'\ncommand=['fmt']\n[[hooks]]\nevent='pre_tool_use'\ncommand=['launch-policy']\n",
    )
    .unwrap();
    let config = Config::from_layers(&layout, &global, &project, &overlay).unwrap();
    let commands: Vec<&str> = config
        .hooks
        .iter()
        .map(|hook| hook.command[0].as_str())
        .collect();
    assert_eq!(commands, ["global-policy", "fmt", "launch-policy"]);
}

#[test]
fn provider_layers_preserve_inferred_routes_and_replace_changed_routes() {
    let base: toml::Table = toml::from_str("model='chat'\n[endpoint]\nbase_url='https://proxy.example/v1/'\nheaders=[{name='X-Route',value={literal='custom'}}]\n[providers.proxy]\nbase_url='https://proxy.example/v1'\n").unwrap();
    for (provider, retains_endpoint) in [("proxy", true), ("openai", false)] {
        let mut merged = base.clone();
        merge_overlay(
            &mut merged,
            toml::from_str(&format!("provider='{provider}'")).unwrap(),
        );
        assert_eq!(
            merged.get("endpoint").is_some(),
            retains_endpoint,
            "{provider}"
        );
        if retains_endpoint {
            assert_eq!(merged.get("endpoint"), base.get("endpoint"));
        }
    }
}
