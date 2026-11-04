use crate::agent::Agent;
use crate::config::types::{Config, Version};
use crate::extensions::*;
use crate::llm::provider::{Backend, ProviderKind};
use crate::runtime::tool_output::ToolOutput;
use crate::tools::integrations::ext::{EXTENSION_API_VERSION, Tool, ToolContext};
use serde_json::{Value, json};
use std::sync::Arc;

struct TestTool {
    name: &'static str,
    version: u32,
    valid_schema: bool,
}
impl Tool for TestTool {
    fn name(&self) -> &str {
        self.name
    }
    fn extension_api_version(&self) -> u32 {
        self.version
    }
    fn schema(&self) -> Value {
        json!({"type":"function","function":{"name":self.name,
            "description":"SDK fixture", "parameters": if self.valid_schema {
                json!({"type":"object","properties":{}})
            } else { Value::Null }}})
    }
    fn needs_approval(&self, _: &Value) -> bool {
        true
    }
    fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
        panic!("installation must not execute tools")
    }
}
struct Fixture {
    name: &'static str,
    version: u32,
    tools: Vec<Arc<dyn Tool>>,
    commands: Vec<Command>,
}
impl Default for Fixture {
    fn default() -> Self {
        Self {
            name: "fixture",
            version: SDK_VERSION,
            tools: vec![tool("sdk_valid")],
            commands: vec![command("sdk_command")],
        }
    }
}
impl Extension for Fixture {
    fn name(&self) -> &str {
        self.name
    }
    fn api_version(&self) -> u32 {
        self.version
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }
    fn commands(&self) -> Vec<Command> {
        self.commands.clone()
    }
}
fn tool(name: &'static str) -> Arc<dyn Tool> {
    Arc::new(TestTool {
        name,
        version: EXTENSION_API_VERSION,
        valid_schema: true,
    })
}
fn command(name: &str) -> Command {
    Command {
        name: name.into(),
        description: "SDK fixture".into(),
        execute: Arc::new(|_, _| panic!("installation must not execute commands")),
    }
}
fn with_agent(f: impl FnOnce(&mut Agent<'_>)) {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        Default::default(),
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
    f(&mut agent);
}

#[test]
fn rejected_installations_are_identified_atomic_and_retryable() {
    let cases = [
        (
            Fixture {
                version: SDK_VERSION + 1,
                ..Fixture::default()
            },
            vec!["extension \"fixture\"", "unsupported SDK version"],
        ),
        (
            Fixture {
                tools: vec![
                    tool("sdk_valid"),
                    Arc::new(TestTool {
                        name: "sdk_bad",
                        version: EXTENSION_API_VERSION + 1,
                        valid_schema: true,
                    }),
                ],
                ..Fixture::default()
            },
            vec![
                "extension \"fixture\"",
                "tool \"sdk_bad\"",
                "unsupported extension tool version",
            ],
        ),
        (
            Fixture {
                tools: vec![
                    tool("sdk_valid"),
                    Arc::new(TestTool {
                        name: "sdk_bad",
                        version: EXTENSION_API_VERSION,
                        valid_schema: false,
                    }),
                ],
                ..Fixture::default()
            },
            vec![
                "extension \"fixture\"",
                "tool \"sdk_bad\"",
                "parameters must be a schema object",
            ],
        ),
        (
            Fixture {
                tools: vec![tool("sdk_valid"), tool("")],
                ..Fixture::default()
            },
            vec!["extension \"fixture\"", "tool name is empty"],
        ),
        (
            Fixture {
                tools: vec![tool("sdk_valid"), tool("sdk_valid")],
                ..Fixture::default()
            },
            vec![
                "extension \"fixture\"",
                "tool \"sdk_valid\"",
                "duplicate tool name",
            ],
        ),
        (
            Fixture {
                commands: vec![command("sdk_command"), command("")],
                ..Fixture::default()
            },
            vec!["extension \"fixture\"", "command name is empty"],
        ),
        (
            Fixture {
                commands: vec![command("sdk_command"), command("bad/name")],
                ..Fixture::default()
            },
            vec![
                "extension \"fixture\"",
                "command \"bad/name\"",
                "invalid command name",
            ],
        ),
        (
            Fixture {
                commands: vec![command("sdk_command"), command("sdk_command")],
                ..Fixture::default()
            },
            vec![
                "extension \"fixture\"",
                "command \"sdk_command\"",
                "duplicate command name",
            ],
        ),
    ];
    for (fixture, expected) in cases {
        with_agent(|agent| {
            let mut host = ExtensionHost::default();
            let before = agent.ext.names().map(str::to_owned).collect::<Vec<_>>();
            let error = format!("{:#}", host.install(agent, &fixture).unwrap_err());
            for text in expected {
                assert!(error.contains(text), "missing {text:?}: {error}");
            }
            if fixture.version != SDK_VERSION {
                assert!(error.contains(&format!(
                    "expected {SDK_VERSION}, actual {}",
                    fixture.version
                )));
            }
            if fixture
                .tools
                .iter()
                .any(|tool| tool.extension_api_version() != EXTENSION_API_VERSION)
            {
                assert!(error.contains(&format!(
                    "expected {EXTENSION_API_VERSION}, actual {}",
                    EXTENSION_API_VERSION + 1
                )));
            }
            assert_eq!(agent.ext.names().collect::<Vec<_>>(), before);
            assert_eq!(host.commands().count(), 0);
            host.install(agent, &Fixture::default()).unwrap();
            assert!(agent.ext.contains("sdk_valid"));
            assert_eq!(
                host.commands()
                    .map(|command| command.name.as_str())
                    .collect::<Vec<_>>(),
                ["sdk_command"]
            );
        });
    }
}

#[test]
fn empty_and_existing_names_have_distinct_diagnostics() {
    with_agent(|agent| {
        let mut host = ExtensionHost::default();
        let error = host
            .install(
                agent,
                &Fixture {
                    name: "",
                    ..Fixture::default()
                },
            )
            .unwrap_err()
            .to_string();
        assert_eq!(error, "extension name is empty");
        host.install(agent, &Fixture::default()).unwrap();
        let before = agent.ext.names().map(str::to_owned).collect::<Vec<_>>();
        for (fixture, expected) in [
            (
                Fixture::default(),
                "extension \"fixture\": duplicate extension name",
            ),
            (
                Fixture {
                    name: "second",
                    ..Fixture::default()
                },
                "tool name already registered",
            ),
            (
                Fixture {
                    name: "second",
                    tools: vec![tool("sdk_other")],
                    ..Fixture::default()
                },
                "extension \"second\" command \"sdk_command\": duplicate command name",
            ),
        ] {
            let error = format!("{:#}", host.install(agent, &fixture).unwrap_err());
            assert!(error.contains(expected), "{error}");
            assert_eq!(agent.ext.names().collect::<Vec<_>>(), before);
            assert_eq!(host.commands().count(), 1);
        }
        host.install(
            agent,
            &Fixture {
                name: "second",
                tools: vec![tool("sdk_other")],
                commands: vec![command("sdk_other_command")],
                ..Fixture::default()
            },
        )
        .unwrap();
        assert_eq!(host.commands().count(), 2);
    });
}
