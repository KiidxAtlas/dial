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
