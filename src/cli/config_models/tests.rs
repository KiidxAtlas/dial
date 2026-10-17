use super::*;

fn words(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn routes_like_omp_models() {
    assert_eq!(route(&[]).unwrap(), ModelsRoute::List(None));
    assert_eq!(route(&words(&["ls"])).unwrap(), ModelsRoute::List(None));
    assert_eq!(
        route(&words(&["ls", "anthropic"])).unwrap(),
        ModelsRoute::List(Some("anthropic".into()))
    );
    assert_eq!(
        route(&words(&["openrouter"])).unwrap(),
        ModelsRoute::List(Some("openrouter".into()))
    );
    assert_eq!(
        route(&words(&["find", "qwen", "coder"])).unwrap(),
        ModelsRoute::Find("qwen coder".into())
    );
    assert!(route(&words(&["find"])).is_err());
    assert_eq!(route(&words(&["refresh"])).unwrap(), ModelsRoute::Refresh);
    assert_eq!(
        route(&words(&["recommend"])).unwrap(),
        ModelsRoute::Recommend
    );
}

#[test]
fn redacts_only_credential_keys() {
    assert!(is_secret_key("endpoint.api_key"));
    assert!(is_secret_key("auth.token"));
    assert!(!is_secret_key("endpoint.api_key_env"));
    assert!(!is_secret_key("model"));
    assert!(is_secret_key("X-Api-Key"));
    assert!(is_secret_key("refresh-token"));
    for key in [
        "harness.max_tokens",
        "efficiency.history_token_cap",
        "max_output_tokens",
        "tokenizer",
        "reasoning.token_budget",
    ] {
        assert!(!is_secret_key(key), "{key}");
        assert_eq!(display_value(key, &4096.into()), "4096");
    }
}

#[test]
fn flattens_and_looks_up_dotted_keys() {
    let table: toml::Table = toml::from_str(
        "model = \"m\"\n[endpoint]\nbase_url = \"http://x\"\n[sandbox]\nmode = \"ro\"\n",
    )
    .unwrap();
    let keys: Vec<String> = flatten(&table).into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, vec!["endpoint.base_url", "model", "sandbox.mode"]);
    assert_eq!(
        lookup(&table, "sandbox.mode").and_then(|v| v.as_str()),
        Some("ro")
    );
    assert!(lookup(&table, "sandbox.missing").is_none());
}

#[test]
fn parses_scalars_and_falls_back_to_string() {
    assert_eq!(parse_value("true"), toml::Value::Boolean(true));
    assert_eq!(parse_value("42"), toml::Value::Integer(42));
    assert_eq!(parse_value("[1, 2]").as_array().map(Vec::len), Some(2));
    assert_eq!(
        parse_value("hello world"),
        toml::Value::String("hello world".into())
    );
}

#[test]
fn table_has_header_and_aligned_rows() {
    let table = format_table(&[]);
    assert!(table.starts_with("PROVIDER"));
    assert_eq!(table.lines().count(), 1);
}

#[test]
fn model_list_exposes_declared_purpose_without_reading_source_prose() {
    let entry: ModelEntry = serde_json::from_value(serde_json::json!({
        "selection": {"provider":"custom", "model":"vectors"},
        "label":"vectors", "source":"operator endpoint", "available":true,
        "context_window":null, "metadata":{"kind":"embedding"}
    }))
    .unwrap();
    let table = format_table(&[entry]);
    assert!(table.lines().next().unwrap().contains("KIND"));
    let cells: Vec<_> = table.lines().nth(1).unwrap().split_whitespace().collect();
    assert_eq!(&cells[..4], &["custom", "vectors", "embedding", "yes"]);
}

#[test]
fn model_kind_cli_filter_accepts_declared_types_and_rejects_typos() {
    for kind in ["chat", "embedding", "rerank", "image", "audio", "unknown"] {
        let matches = <ModelsArgs as clap::Args>::augment_args(clap::Command::new("models"))
            .try_get_matches_from(["models", "find", "custom", "--kind", kind])
            .unwrap();
        let args = <ModelsArgs as clap::FromArgMatches>::from_arg_matches(&matches).unwrap();
        assert_eq!(args.kind.as_deref(), Some(kind));
        assert_eq!(
            route(&args.words).unwrap(),
            ModelsRoute::Find("custom".into())
        );
    }
    assert!(
        <ModelsArgs as clap::Args>::augment_args(clap::Command::new("models"))
            .try_get_matches_from(["models", "--kind", "embeddng"])
            .is_err()
    );
}

#[test]
fn config_display_shows_migrated_role_keys() {
    let legacy: toml::Table = toml::from_str("[model_roles]\nslow = \"big\"\n").unwrap();
    let shown = migrate_for_display(legacy);
    assert!(lookup(&shown, "model_roles.slow").is_none());
    assert_eq!(
        lookup(&shown, "model_roles.coach"),
        Some(&toml::Value::String("big".into()))
    );
}

#[test]
fn nested_credentials_are_redacted_in_text_and_json() {
    let table: toml::Table = toml::from_str(
        r#"
            max_tokens = 4096
            api_key_env = "KEY_NAME"
            [auth]
            access_token = "hidden-token"
            password = "hidden-password"
            [[headers]]
            name = "Authorization"
            value = { literal = "hidden-header" }
        "#,
    )
    .unwrap();
    let value = toml::Value::Table(table);
    for output in [
        display_value("endpoint", &value),
        serde_json::to_string(&redacted_value("endpoint", &value)).unwrap(),
    ] {
        for secret in ["hidden-token", "hidden-password", "hidden-header"] {
            assert!(!output.contains(secret), "{output}");
        }
        assert!(output.contains("4096") && output.contains("KEY_NAME"));
        assert!(output.contains("<redacted>"));
    }
}

#[test]
fn config_display_uses_runtime_defaults_overlay_and_hook_merging() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let global = "[[hooks]]\nevent='pre_tool_use'\ncommand=['global-policy']\n";
    let local = "[[hooks]]\nevent='post_tool_use'\ncommand=['fmt']\n";
    for (path, content) in layout.config_files().iter().zip([global, local]) {
        settings_store::private_dir(path.parent().unwrap()).unwrap();
        settings_store::write_private(path, content.as_bytes()).unwrap();
    }
    let overlay: toml::Table = toml::from_str(
        "model='launch-model'\n[[hooks]]\nevent='pre_tool_use'\ncommand=['global-policy']\n",
    )
    .unwrap();
    let table = merged(&layout, &overlay).unwrap();
    assert_eq!(
        lookup(&table, "model").and_then(toml::Value::as_str),
        Some("launch-model")
    );
    assert_eq!(table["hooks"].as_array().unwrap().len(), 2);
    assert!(lookup(&table, "harness.max_tokens").is_some());
    let runtime = settings_store::load_config(&layout, &overlay).unwrap();
    assert_eq!(table, toml::Table::try_from(runtime).unwrap());
}
