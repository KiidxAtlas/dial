use super::*;
use clap::Parser;

#[derive(Parser, Debug)]
struct Cli {
    #[command(subcommand)]
    command: Option<Top>,
}

#[derive(Subcommand, Debug)]
enum Top {
    #[command(alias = "plugins")]
    Plugin(PluginArgs),
}

fn parse(args: &[&str]) -> Option<PluginCmd> {
    let mut argv = vec!["dial"];
    argv.extend_from_slice(args);
    match Cli::try_parse_from(argv).unwrap().command.unwrap() {
        Top::Plugin(p) => p.command,
    }
}

#[test]
fn parses_marketplace_enable_and_disable() {
    let mp = |args: &[&str]| match parse(&[&["plugin", "marketplace"][..], args].concat()) {
        Some(PluginCmd::Marketplace { command }) => command,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(
        mp(&["enable", "anthropic-skills"]),
        Some(MarketplaceCmd::Enable {
            name: "anthropic-skills".into()
        })
    );
    assert_eq!(
        mp(&["disable", "anthropic-skills"]),
        Some(MarketplaceCmd::Disable {
            name: "anthropic-skills".into()
        })
    );
}

#[test]
fn bare_plugin_has_no_command_and_alias_works() {
    assert_eq!(parse(&["plugin"]), None);
    assert_eq!(parse(&["plugins"]), None);
}

#[test]
fn parses_plugin_lifecycle_commands() {
    assert_eq!(
        parse(&[
            "plugin",
            "new",
            "./example",
            "--name",
            "demo",
            "--python",
            "python3.12"
        ]),
        Some(PluginCmd::New {
            directory: "./example".into(),
            name: Some("demo".into()),
            python: "python3.12".into()
        })
    );
    assert_eq!(
        parse(&["plugin", "check", "./example"]),
        Some(PluginCmd::Check {
            directory: "./example".into(),
            python: "python3".into()
        })
    );
    assert_eq!(
        parse(&["plugin", "install", "./p"]),
        Some(PluginCmd::Install {
            source: "./p".into()
        })
    );
    assert_eq!(
        parse(&["plugin", "list", "--json"]),
        Some(PluginCmd::List { json: true })
    );
    assert_eq!(
        parse(&["plugin", "remove", "x"]),
        Some(PluginCmd::Remove {
            name: "x".into(),
            purge_state: false
        })
    );
    assert_eq!(
        parse(&["plugin", "uninstall", "x"]),
        Some(PluginCmd::Remove {
            name: "x".into(),
            purge_state: false
        })
    );
    assert_eq!(
        parse(&["plugin", "enable", "x"]),
        Some(PluginCmd::Enable { name: "x".into() })
    );
    assert_eq!(
        parse(&["plugin", "disable", "x"]),
        Some(PluginCmd::Disable { name: "x".into() })
    );
    assert_eq!(
        parse(&["plugin", "update"]),
        Some(PluginCmd::Update { name: None })
    );
    assert_eq!(
        parse(&["plugin", "update", "x"]),
        Some(PluginCmd::Update {
            name: Some("x".into())
        })
    );
    assert_eq!(
        parse(&["plugin", "check-updates", "--json"]),
        Some(PluginCmd::CheckUpdates { json: true })
    );
}

#[test]
fn parses_marketplace_commands() {
    let mp = |args: &[&str]| match parse(&[&["plugin", "marketplace"][..], args].concat()) {
        Some(PluginCmd::Marketplace { command }) => command,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(mp(&[]), None);
    assert_eq!(
        mp(&["add", "core", "https://x/index.json"]),
        Some(MarketplaceCmd::Add {
            name: "core".into(),
            index: "https://x/index.json".into()
        })
    );
    assert_eq!(
        mp(&["rm", "core"]),
        Some(MarketplaceCmd::Remove {
            name: "core".into()
        })
    );
    assert_eq!(mp(&["list"]), Some(MarketplaceCmd::List));
    assert_eq!(
        mp(&["refresh"]),
        Some(MarketplaceCmd::Refresh { name: None })
    );
    assert_eq!(
        mp(&["entries", "core", "--json"]),
        Some(MarketplaceCmd::Entries {
            name: Some("core".into()),
            json: true
        })
    );
    assert_eq!(
        mp(&["install", "p@core"]),
        Some(MarketplaceCmd::Install {
            plugin: "p@core".into()
        })
    );
    assert_eq!(
        mp(&["upgrade"]),
        Some(MarketplaceCmd::Upgrade { name: None })
    );
}

#[test]
fn rejects_unknown_subcommand() {
    assert!(Cli::try_parse_from(["dial", "plugin", "bogus"]).is_err());
}

#[test]
fn settings_and_preset_commands_persist_only_valid_public_values() {
    use serde_json::json;
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let layout = Layout::with_user_dir(&project, temp.path().join("private")).unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let manifest = json!({"name":"demo","contributions":{"settings":{"type":"object","properties":{"count":{"type":"integer","default":2,"minimum":1},"token":{"type":"string","writeOnly":true}},"additionalProperties":false}}});
    std::fs::write(source.join("dial-plugin.json"), manifest.to_string()).unwrap();
    PluginStore::new(layout.user_dir.join("plugins"))
        .install(&format!("file:{}", source.display()))
        .unwrap();
    settings(
        &layout,
        "demo",
        SettingsCmd::Set {
            key: "count".into(),
            value: "3".into(),
        },
        false,
    )
    .unwrap();
    assert!(
        settings(
            &layout,
            "demo",
            SettingsCmd::Set {
                key: "count".into(),
                value: "0".into()
            },
            false
        )
        .is_err()
    );
    let path = temp.path().join("preset.json");
    presets(
        &layout,
        "demo",
        PresetCmd::Export { path: path.clone() },
        false,
    )
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap(),
        json!({"count":3})
    );
    std::fs::write(&path, r#"{"token":"never-save"}"#).unwrap();
    assert!(presets(&layout, "demo", PresetCmd::Import { path }, false).is_err());
    settings(&layout, "demo", SettingsCmd::Reset, false).unwrap();
    let (store, schema) = settings_store(&layout, "demo").unwrap();
    assert_eq!(store.load("demo", &schema).unwrap(), json!({"count":2}));
}

#[test]
fn parses_explicit_purge_and_settings_scopes() {
    assert_eq!(
        parse(&["plugin", "remove", "demo", "--purge-state"]),
        Some(PluginCmd::Remove {
            name: "demo".into(),
            purge_state: true
        })
    );
    assert_eq!(
        parse(&[
            "plugin",
            "settings",
            "demo",
            "--workspace",
            "set",
            "count",
            "3"
        ]),
        Some(PluginCmd::Settings {
            name: "demo".into(),
            workspace: true,
            command: Some(SettingsCmd::Set {
                key: "count".into(),
                value: "3".into()
            })
        })
    );
    assert_eq!(
        parse(&["plugin", "dev", "./demo", "--once"]),
        Some(PluginCmd::Dev {
            directory: "./demo".into(),
            once: true,
            interval_ms: 500
        })
    );
}
