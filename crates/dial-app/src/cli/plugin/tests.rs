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
        Some(PluginCmd::Remove { name: "x".into() })
    );
    assert_eq!(
        parse(&["plugin", "uninstall", "x"]),
        Some(PluginCmd::Remove { name: "x".into() })
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
