use super::*;

/// This test doubles as a subprocess entry point, running the real relay bind/startup path.
#[test]
fn relay_readiness_child() {
    let Some(bind) = std::env::var_os("DIAL_TEST_RELAY_BIND") else {
        return;
    };
    if let Err(error) = crate::collab::server::serve(bind.to_string_lossy().parse().unwrap()) {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

#[test]
fn relay_readiness_requires_successful_bind_in_the_launched_child() {
    use std::process::{Child, Command, Stdio};
    struct TestChild(Child);
    impl Drop for TestChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    // Reserving a real listener proves a bind failure cannot be mistaken for readiness from
    // some other process already listening there. Port zero selects a free bind for success.
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    for (bind, ready) in [
        ("127.0.0.1:0".to_owned(), true),
        (reserved.local_addr().unwrap().to_string(), false),
    ] {
        let mut child = TestChild(
            crate::platform::process::spawn(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "app::commands::maintenance::tests::relay_readiness_child",
                        "--nocapture",
                    ])
                    .env("DIAL_TEST_RELAY_BIND", bind)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped()),
            )
            .unwrap(),
        );
        let result = await_relay_ready(&mut child.0);
        assert_eq!(result.is_ok(), ready, "{result:?}");
        if !ready {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("binding collab relay")
            );
        }
    }
}

#[test]
fn the_weights_driver_picker_offers_the_current_model_then_other_chat_models() {
    use crate::llm::model_config::ModelSelection;
    let selection = |provider: &str, model: &str| ModelSelection {
        provider: provider.into(),
        model: model.into(),
        ..Default::default()
    };
    let mut config = crate::config::types::Config {
        model: "local-qwen".into(),
        provider: Some("mlx".into()),
        ..Default::default()
    };
    config
        .model_roles
        .insert("coach".into(), selection("anthropic", "claude-opus"));
    config
        .model_roles
        .insert("embedding".into(), selection("ollama", "nomic-embed"));
    config.model_favorites = vec![
        selection("anthropic", "claude-opus"),
        selection("mlx", "local-qwen"),
        selection("openai", "gpt-5"),
    ];

    let spec = driver_choices(&config, "write cleaner code");
    let values: Vec<&str> = spec.items.iter().map(|item| item.value.as_str()).collect();
    assert_eq!(
        values,
        [
            "--with @main write cleaner code",
            "--with anthropic/claude-opus write cleaner code",
            "--with openai/gpt-5 write cleaner code",
        ],
        "current first, duplicates and non-chat roles left out"
    );
    assert!(spec.items[0].current && !spec.items[1].current);
    assert_eq!((spec.command.as_str(), spec.verb), ("weights", "improve"));
}
