use crate::config::tools::integrations::INTEGRATION_TOOLS;
use crate::config::tools::integrations::INTEGRATIONS;
use crate::config::tools::integrations::default_integrations;

use super::*;
use serde_json::json;
#[test]
fn missing_offered_schema_and_duplicate_definitions_fail_before_execution() {
    let call = crate::llm::ToolCall {
        id: "1".into(),
        name: "read".into(),
        arguments: json!({}),
        malformed_arguments: false,
    };
    assert!(validate_offered_call(&call, &[], &["read".into()]).is_some());
    let schema = crate::tools::schema("read", crate::config::types::EditMode::Replace).unwrap();
    assert!(Catalog::from_schemas(&[schema.clone(), schema], &Registry::new()).is_err());
}
#[test]
fn oversized_grep_context_is_capped_at_runtime_not_rejected() {
    let schema = crate::tools::schema("grep", crate::config::types::EditMode::Replace).unwrap();
    let call = crate::llm::ToolCall {
        id: "1".into(),
        name: "grep".into(),
        arguments: json!({"pattern": "fn", "context": 25}),
        malformed_arguments: false,
    };
    assert_eq!(
        validate_offered_call(&call, &[schema], &["grep".into()]),
        None
    );
}
#[test]
fn integration_defaults_and_legacy_registration_share_metadata() {
    let config = crate::config::types::Config::default();
    assert_eq!(config.integrations, default_integrations());
    let names: std::collections::BTreeSet<_> =
        INTEGRATIONS.iter().map(|entry| entry.name).collect();
    assert_eq!(names.len(), INTEGRATIONS.len());
    assert_eq!(names, INTEGRATION_TOOLS.iter().copied().collect());
    for entry in INTEGRATIONS {
        assert_eq!(
            integration_enabled(entry.name, &[]),
            entry.always_registered
        );
    }
}

#[test]
fn static_platform_support_does_not_replace_launch_containment() {
    let name = "eval";
    let platform = execution_platform(name);
    assert_eq!(platform.available, platform.reason.is_none());
    if cfg!(target_os = "macos") {
        assert!(platform.available);
        let expected = crate::tools::exec::sandbox::Sandbox::detect().available().err().map(|error| {
            format!("{name} requires OS process-fork containment even with filesystem sandboxing disabled: {error}")
        });
        assert_eq!(
            crate::tools::exec::kernel::require_fork_containment(name).err(),
            expected
        );
    } else {
        assert_eq!(
            crate::tools::exec::kernel::require_fork_containment(name).err(),
            platform.reason
        );
        assert_eq!(platform.available, cfg!(windows));
    }
}
#[test]
fn status_and_discovery_report_the_same_execution_platform() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let config = crate::config::types::Config::default();
    let context = crate::tools::integrations::ext::ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let tools = crate::tools::exec::eval::tools(&config).into_iter();
    for tool in tools {
        let schema = tool.schema();
        let catalog =
            Catalog::from_schemas(std::slice::from_ref(&schema), &Registry::new()).unwrap();
        let platform = catalog.get(tool.name()).unwrap().platform.as_ref().unwrap();
        let status = tool.call(&context, &json!({"action":"status"}));
        assert!(!status.is_error, "{}", status.text);
        assert_eq!(
            status.data.unwrap()["platform"],
            serde_json::to_value(platform).unwrap()
        );
        if let Some(reason) = &platform.reason {
            assert!(
                schema["function"]["description"]
                    .as_str()
                    .unwrap()
                    .contains(reason)
            );
        }
    }
}

#[test]
fn readiness_tracks_config_without_probing_or_claiming_registry_absence() {
    let registry = Registry::new();
    let mut config = crate::config::types::Config::default();
    assert_eq!(
        readiness("mcp__offline__unknown", &config, &registry),
        Readiness::Unknown
    );
    assert_eq!(
        readiness("dap_unregistered", &config, &registry),
        Readiness::Unknown
    );
    assert!(matches!(
        operation_readiness("computer", &config)["locate"],
        Readiness::MissingPrerequisite { .. }
    ));
    assert_eq!(
        readiness("computer", &config, &registry),
        Readiness::Unknown
    );
    config.vision.grounding = Some(crate::llm::model_config::ModelSelection::new(
        "openai",
        "unreachable-test-model",
    ));
    assert_eq!(
        operation_readiness("computer", &config)["locate"],
        Readiness::Unknown
    );
    config.vision.grounding = None;
    assert!(matches!(
        operation_readiness("computer", &config)["locate"],
        Readiness::MissingPrerequisite { .. }
    ));

    let mut adapter = crate::config::tools::dap::defaults().remove("gdb").unwrap();
    adapter.program.clear();
    adapter.address.clear();
    config.dap.insert("test".into(), adapter);
    assert!(matches!(
        readiness("dap_test", &config, &registry),
        Readiness::MissingPrerequisite { .. }
    ));
    config.dap.get_mut("test").unwrap().address = "127.0.0.1:1".into();
    // A configured address is not proof a server is reachable, and no connection is attempted.
    assert_eq!(
        readiness("dap_test", &config, &registry),
        Readiness::Unknown
    );
}

#[test]
fn catalog_readiness_refresh_is_advisory_and_preserves_contract_identity() {
    let registry = Registry::new();
    let config = crate::config::types::Config::default();
    let schema = crate::tools::schema("read", crate::config::types::EditMode::Replace).unwrap();
    let mut catalog = Catalog::from_schemas(&[schema], &registry).unwrap();
    let before = serde_json::to_value(&catalog.get("read").unwrap().identity).unwrap();
    catalog.refresh_readiness(&config, &registry);
    let definition = catalog.get("read").unwrap();
    assert_eq!(definition.readiness, Readiness::Ready);
    assert_eq!(serde_json::to_value(&definition.identity).unwrap(), before);
    assert_eq!(
        serde_json::to_value(&definition.readiness).unwrap(),
        json!({"state":"ready"})
    );
    assert!(catalog.validate("read", &json!({})).is_err());
}
