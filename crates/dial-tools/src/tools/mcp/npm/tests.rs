use super::*;
use serde_json::json;

fn user() -> &'static Path {
    Path::new("/home/u/.dial")
}

#[test]
fn a_single_command_runs_pinned_through_npx() {
    let manifest = json!({"name": "context-mode", "version": "1.0.169",
        "description": "MCP plugin", "bin": {"context-mode": "cli.bundle.mjs"},
        "repository": {"type": "git", "url": "git+https://github.com/mksglu/context-mode.git"}});
    let found = from_manifest(&manifest, user()).expect("has a command");
    assert_eq!(found.name, "npm:context-mode");
    assert_eq!(found.sources, vec![Source::Npm]);
    assert_eq!(
        found.repository.as_deref(),
        Some("https://github.com/mksglu/context-mode")
    );
    let Install::Server(server) = found.install else {
        panic!("one command is installable");
    };
    assert_eq!(server.program, PathBuf::from("npx"));
    assert_eq!(server.args, ["-y", "context-mode@1.0.169"]);
    assert!(server.env.contains_key("npm_config_cache"));
}

#[test]
fn several_commands_name_the_server_one_or_block_with_the_list() {
    let named = json!({"name": "@acme/tool", "version": "2.0.0",
        "bin": {"tool-cli": "a.js", "tool-mcp": "b.js"}});
    let Install::Server(server) = from_manifest(&named, user()).unwrap().install else {
        panic!("the mcp command is picked");
    };
    assert_eq!(
        server.args,
        ["-y", "--package=@acme/tool@2.0.0", "tool-mcp"]
    );

    let unclear = json!({"name": "multi", "version": "1.0.0",
        "bin": {"one": "a.js", "two": "b.js"}, "deprecated": "use other"});
    let found = from_manifest(&unclear, user()).unwrap();
    assert!(!found.installable());
    assert!(found.install_summary().contains("one, two"));
    assert_eq!(found.badges, vec!["deprecated".to_owned()]);
}

#[test]
fn libraries_and_unsafe_metadata_are_not_listed() {
    let library = json!({"name": "@modelcontextprotocol/sdk", "version": "1.0.0"});
    assert!(from_manifest(&library, user()).is_none());
    let bad_version = json!({"name": "x", "version": "1 || 2", "bin": "x.js"});
    assert!(from_manifest(&bad_version, user()).is_none());
    let bad_command = json!({"name": "x", "version": "1.0.0", "bin": {"--eval": "x.js"}});
    assert!(from_manifest(&bad_command, user()).is_none());
}

#[test]
fn package_names_are_plain_or_scoped() {
    assert!(valid_package("context-mode"));
    assert!(valid_package("@upstash/context7-mcp"));
    assert!(!valid_package("@scope"));
    assert!(!valid_package("../etc"));
    assert!(!valid_package("a/b"));
    assert!(!valid_package("-y"));
}
