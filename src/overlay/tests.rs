use super::*;
use clap::Parser;
use dial::config::paths::Layout;
use dial::config::types::Config;

#[test]
fn explicit_endpoint_replaces_saved_route_and_keeps_an_explicit_provider() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    std::fs::write(home.path().join("config.toml"), "provider='chatgpt'\nmodel='claude-sonnet-4-6'\n[endpoint]\nbase_url='https://chatgpt.com/backend-api'\nprotocol='responses'\n").unwrap();
    for (args, provider) in [
        (
            vec!["dial", "--endpoint", "https://proxy.example/v1"],
            "endpoint",
        ),
        (
            vec![
                "dial",
                "--provider",
                "anthropic",
                "--endpoint",
                "https://proxy.example/v1",
            ],
            "anthropic",
        ),
    ] {
        let cli = Cli::try_parse_from(args).unwrap();
        let loaded = Config::load(&layout, Some(overlay(&cli).unwrap())).unwrap();
        let selected = dial::cli::model_flags::apply(loaded, &cli.model_flags).unwrap();
        assert_eq!(selected.provider.as_deref(), Some(provider));
        let endpoint = selected.endpoint.unwrap();
        assert_eq!(endpoint.base_url, "https://proxy.example/v1");
        assert!(endpoint.account_id.is_none());
        assert!(endpoint.headers.is_none());
        assert_ne!(endpoint.api_key_env.as_deref(), Some("OLD_ROUTE_KEY"));
        assert_ne!(
            endpoint.protocol,
            Some(dial::llm::model_config::ApiProtocol::Responses)
        );
    }
}
