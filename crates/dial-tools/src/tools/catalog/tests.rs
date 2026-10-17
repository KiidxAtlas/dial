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
    let schema = super::super::schema("read", crate::config::types::EditMode::Replace).unwrap();
    assert!(Catalog::from_schemas(&[schema.clone(), schema], &Registry::new()).is_err());
}
#[test]
fn oversized_grep_context_is_capped_at_runtime_not_rejected() {
    let schema = super::super::schema("grep", crate::config::types::EditMode::Replace).unwrap();
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
fn execution_platform_reason_matches_launch_gate() {
    for name in ["kernel", "eval"] {
        let platform = execution_platform(name);
        assert_eq!(platform.available, platform.reason.is_none());
        assert_eq!(
            super::super::kernel::require_fork_containment(name).err(),
            platform.reason
        );
        #[cfg(target_os = "linux")]
        assert!(!platform.available);
        #[cfg(windows)]
        assert_eq!(platform.available, name == "kernel");
    }
}
#[test]
fn status_and_discovery_report_the_same_execution_platform() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let config = crate::config::types::Config::default();
    let context = super::super::ext::ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let tools = super::super::kernel::tools(&config)
        .into_iter()
        .chain(super::super::eval::tools(&config));
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
