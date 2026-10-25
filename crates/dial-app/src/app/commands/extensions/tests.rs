use super::*;

#[test]
fn splits_subcommand_and_rest() {
    assert_eq!(
        split_subcommand("  add  docs --url x "),
        ("add", "docs --url x")
    );
    assert_eq!(split_subcommand("list"), ("list", ""));
    assert_eq!(split_subcommand(""), ("", ""));
}

#[test]
fn validates_server_names() {
    assert!(valid_server_name("docs-1_a"));
    assert!(!valid_server_name(""));
    assert!(!valid_server_name("a.b"));
    assert!(!valid_server_name("a b"));
    assert!(!valid_server_name(&"x".repeat(65)));
}

#[test]
fn parses_transport_names() {
    assert!(parse_transport("sse").is_ok());
    assert!(parse_transport("stdio").is_ok());
    assert!(parse_transport("ws").is_err());
}

#[test]
fn parses_mcp_add_scope() {
    let (_, _, scope) = parse_mcp_add("docs -- npx srv").unwrap();
    assert_eq!(scope, Scope::Project);
    let (_, _, scope) = parse_mcp_add("docs --scope user -- npx srv").unwrap();
    assert_eq!(scope, Scope::Global);
    let err = parse_mcp_add("docs --scope workspace -- npx srv").unwrap_err();
    assert_eq!(
        err.to_string(),
        "Invalid --scope value. Use project or user."
    );
}

#[test]
fn mcp_list_hides_credentials_and_command_arguments() {
    let config = Config {
        mcp: BTreeMap::from([
            (
                "remote".into(),
                McpServerConfig {
                    url: Some(
                        "https://user:password@example.com/private-token?key=secret#fragment"
                            .into(),
                    ),
                    ..Default::default()
                },
            ),
            (
                "local".into(),
                McpServerConfig {
                    program: PathBuf::from("/private/location/server"),
                    args: vec!["--token".into(), "positional-secret".into()],
                    enabled: false,
                    ..Default::default()
                },
            ),
        ]),
        ..Default::default()
    };
    let listing = mcp_list(&config);
    assert!(listing.contains("remote  enabled  remote MCP endpoint (URL hidden)"));
    assert!(listing.contains("local  disabled  server (2 arguments hidden)"));
    for secret in [
        "user",
        "password",
        "example.com",
        "private-token",
        "secret",
        "fragment",
        "/private",
        "--token",
    ] {
        assert!(!listing.contains(secret), "leaked {secret}: {listing}");
    }
}
