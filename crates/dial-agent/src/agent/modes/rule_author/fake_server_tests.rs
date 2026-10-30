use super::*;
use crate::config::types::{Config, Version};
use crate::llm::fake::{self, Script};
use crate::llm::provider::{Backend, ProviderKind};

fn sse_text(text: &str) -> Script {
    let event = json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]});
    Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

/// `/rule` generation is a non-streaming chat call, so its reply is a complete JSON body.
fn json_text(text: &str) -> Script {
    let body =
        json!({"choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}]});
    Script::Http {
        status: 200,
        content_type: "application/json",
        body: body.to_string().into_bytes(),
    }
}

fn rule_reply(condition: &str) -> String {
    json!({
        "name": "no-any",
        "description": "Never use any",
        "condition": condition,
        "scope": "text",
        "body": "Use unknown."
    })
    .to_string()
}

/// Builds a hermetic agent and runs one scripted turn so the session holds an assistant reply.
fn agent_with_history<'a>(
    config: &'a Config,
    prompt: &str,
    scripts: Vec<Script>,
    workspace: &std::path::Path,
) -> (Agent<'a>, fake::Fake) {
    let server = fake::serve(scripts);
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
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
    let mut agent = Agent::hermetic(config, backend, &version, workspace, None, None).unwrap();
    agent.turn(prompt, &mut crate::agent::NullUi).unwrap();
    (agent, server)
}

#[test]
fn generated_rule_matching_session_output_is_accepted() {
    let config = Config::default();
    let workspace = tempfile::tempdir().unwrap();
    let (mut agent, server) = agent_with_history(
        &config,
        "write it",
        vec![
            sse_text("const x: any = 1;"),
            json_text(&rule_reply(": any")),
        ],
        workspace.path(),
    );
    let reply = agent
        .rule_generate(&build_prompt("stop using any", "", None))
        .unwrap();
    match agent.rule_evaluate(&reply) {
        Evaluation::Matched(candidate) => assert_eq!(candidate.name, "no-any"),
        _ => panic!("expected a matching rule"),
    }
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn generated_rule_matching_nothing_in_session_is_unmatched() {
    let config = Config::default();
    let workspace = tempfile::tempdir().unwrap();
    let (mut agent, _server) = agent_with_history(
        &config,
        "say hello",
        vec![
            sse_text("hello there"),
            json_text(&rule_reply("zzz_never_present")),
        ],
        workspace.path(),
    );
    let reply = agent
        .rule_generate(&build_prompt("stop using any", "", None))
        .unwrap();
    assert!(matches!(
        agent.rule_evaluate(&reply),
        Evaluation::Unmatched(_)
    ));
}

#[test]
fn invalid_model_reply_is_reported_as_invalid() {
    let config = Config::default();
    let workspace = tempfile::tempdir().unwrap();
    let (mut agent, _server) = agent_with_history(
        &config,
        "say hello",
        vec![sse_text("hello"), json_text("I cannot do that.")],
        workspace.path(),
    );
    let reply = agent.rule_generate(&build_prompt("c", "", None)).unwrap();
    match agent.rule_evaluate(&reply) {
        Evaluation::Invalid(reason) => assert_eq!(reason, "Missing generated rule JSON object"),
        _ => panic!("expected an invalid rule"),
    }
}

#[test]
fn save_writes_project_rule_file_and_refuses_overwrite() {
    let config = Config::default();
    let workspace = tempfile::tempdir().unwrap();
    let (mut agent, _server) = agent_with_history(
        &config,
        "say hello",
        vec![sse_text("hello")],
        workspace.path(),
    );
    let candidate = candidate_from_reply(&rule_reply(": any")).unwrap();
    let saved = agent.rule_save(&candidate, false, false).unwrap();
    assert!(saved.path.ends_with(".dial/rules/no-any.md"));
    let written = std::fs::read_to_string(&saved.path).unwrap();
    assert!(written.starts_with("---\nname: \"no-any\"\n"));
    assert!(agent.rule_save(&candidate, false, false).is_err());
    assert!(agent.rule_save(&candidate, false, true).is_ok());
}
