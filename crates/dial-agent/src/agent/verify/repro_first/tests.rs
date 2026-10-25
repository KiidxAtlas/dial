use super::*;

fn targets(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|path| (*path).to_owned()).collect()
}

#[test]
fn applies_only_to_unreproduced_debugging_source_edits_once() {
    let source = targets(&["src/parser.rs"]);
    assert!(applies(
        ReproFirst::Gate,
        Kind::Debugging,
        false,
        false,
        &source
    ));
    assert!(applies(
        ReproFirst::Advise,
        Kind::Debugging,
        false,
        false,
        &source
    ));
    assert!(!applies(
        ReproFirst::Off,
        Kind::Debugging,
        false,
        false,
        &source
    ));
    assert!(!applies(
        ReproFirst::Gate,
        Kind::Editing,
        false,
        false,
        &source
    ));
    assert!(
        !applies(ReproFirst::Gate, Kind::Debugging, true, false, &source),
        "already reproduced"
    );
    assert!(
        !applies(ReproFirst::Gate, Kind::Debugging, false, true, &source),
        "fires once"
    );
    for other in [
        targets(&["tests/parser.rs"]),
        targets(&["src/parser_test.go"]),
        targets(&["web/app.spec.ts"]),
        targets(&["README.md"]),
        targets(&[]),
    ] {
        assert!(
            !applies(ReproFirst::Gate, Kind::Debugging, false, false, &other),
            "{other:?}"
        );
    }
    assert!(applies(
        ReproFirst::Gate,
        Kind::Debugging,
        false,
        false,
        &targets(&["tests/parser.rs", "src/parser.rs"])
    ));
}

#[test]
fn the_configured_mode_fires_once_per_turn_and_only_for_native_edits() {
    use crate::agent::*;
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.experimental.repro_first = ReproFirst::Gate;
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    let mut agent = Agent::hermetic(&config, backend, &version, root.path(), None, None).unwrap();
    agent.task_policy.kind = Kind::Debugging;
    let call = |name: &str, arguments: Value| ToolCall {
        id: "call".into(),
        name: name.into(),
        arguments,
        malformed_arguments: false,
    };
    let write = call(
        "write",
        json!({"path":"src/lib.rs","content":"fn fixed() {}"}),
    );
    assert!(!agent.repro_first_fires(&call("bash", json!({"command":"ls"})), ReproFirst::Gate));
    assert!(
        !agent.repro_first_fires(&write, ReproFirst::Advise),
        "a different mode is inert"
    );
    assert!(agent.repro_first_fires(&write, ReproFirst::Gate));
    assert!(
        !agent.repro_first_fires(&write, ReproFirst::Gate),
        "the retry proceeds"
    );

    let mut agent = Agent::hermetic(
        &config,
        Backend::new(
            ProviderKind::Endpoint {
                base_url: "http://127.0.0.1:1/v1".into(),
                model: "test".into(),
                api_key: None,
            },
            PathBuf::new(),
            5,
        ),
        &version,
        root.path(),
        None,
        None,
    )
    .unwrap();
    agent.task_policy.kind = Kind::Debugging;
    agent.step_checks.reproduced = true;
    assert!(
        !agent.repro_first_fires(&write, ReproFirst::Gate),
        "a failed reproduction satisfies it"
    );
}
