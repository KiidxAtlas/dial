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

#[test]
fn prompt_request_reads_quoted_argument_values() {
    let request =
        parse_prompt_request(r#"docs review topic="a b c" lang=rust note='x = y' empty="""#)
            .unwrap();
    assert_eq!(request.server, "docs");
    assert_eq!(request.name, "review");
    assert_eq!(request.arguments["topic"], "a b c");
    assert_eq!(request.arguments["lang"], "rust");
    assert_eq!(request.arguments["note"], "x = y");
    assert_eq!(request.arguments["empty"], "");
}

#[test]
fn prompt_request_keeps_equals_inside_values() {
    let request = parse_prompt_request("docs review expr=a=b").unwrap();
    assert_eq!(request.arguments["expr"], "a=b");
}

#[test]
fn prompt_request_rejects_bad_input() {
    let usage = "Usage: /mcp prompt <server> <name> [key=value…]";
    assert_eq!(
        parse_prompt_request("docs").err().unwrap().to_string(),
        usage
    );
    let unclosed = parse_prompt_request(r#"docs review topic="open"#)
        .err()
        .unwrap();
    assert!(unclosed.to_string().starts_with("Unclosed quote"));
    let bare = parse_prompt_request("docs review topic").err().unwrap();
    assert!(
        bare.to_string()
            .starts_with("Prompt argument `topic` must be key=value.")
    );
    let empty_key = parse_prompt_request("docs review =x").err().unwrap();
    assert!(empty_key.to_string().starts_with("Prompt argument `=x`"));
    let twice = parse_prompt_request("docs review a=1 a=2").err().unwrap();
    assert!(
        twice
            .to_string()
            .starts_with("Prompt argument `a` is given more than once.")
    );
}

#[test]
fn composed_draft_appends_below_existing_text() {
    assert_eq!(composed_draft("", "prompt"), "prompt");
    assert_eq!(composed_draft("  \n", "prompt"), "prompt");
    assert_eq!(composed_draft("first", "prompt"), "first\n\nprompt");
}

#[test]
fn prompt_text_renders_messages_and_names_omitted_content() {
    let result = serde_json::json!({
        "messages": [
            {"role": "user", "content": {"type": "text", "text": "Review it"}},
            {"role": "assistant", "content": {"type": "image", "data": "..."}},
            {"role": "user", "content": {"type": "resource", "resource": {"uri": "x", "text": "body"}}}
        ]
    });
    assert_eq!(
        prompt_text(&result).unwrap(),
        "user: Review it\nassistant: [image content omitted]\nuser: body"
    );
}

#[test]
fn prompt_text_fails_when_no_messages_come_back() {
    let empty = serde_json::json!({"messages": []});
    assert_eq!(
        prompt_text(&empty).unwrap_err().to_string(),
        "The prompt returned no messages."
    );
    assert!(prompt_text(&serde_json::json!({})).is_err());
}
