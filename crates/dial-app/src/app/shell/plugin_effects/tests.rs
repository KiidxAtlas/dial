use super::*;
use crate::storage::session::{Session, SessionMeta};
use serde_json::json;

#[test]
fn model_switch_prompt_names_the_plugin_and_the_exact_change() {
    let action = Action::SetModel {
        model: "openai/gpt-5".into(),
        effort: Some("high".into()),
    };
    let (title, detail) = approval_prompt("reviewer", &action).unwrap().unwrap();
    assert_eq!(
        title,
        "Allow plugin \"reviewer\" to switch the session model?"
    );
    assert!(detail.contains("Model: openai/gpt-5"));
    assert!(detail.contains("Effort: high"));
}

#[test]
fn appended_entries_need_no_approval() {
    let entry = Action::AppendEntry {
        kind: "build".into(),
        text: "ok".into(),
        renderer: None,
    };
    assert!(approval_prompt("p", &entry).unwrap().is_none());
}

#[test]
fn queued_messages_are_gated_and_show_sanitized_text() {
    let action = Action::SendUserMessage {
        text: "run\u{1b}[2J tests".into(),
    };
    let (title, detail) = approval_prompt("p", &action).unwrap().unwrap();
    assert!(title.contains("send a message"));
    assert!(detail.contains("run[2J tests"));
    assert!(!detail.contains('\u{1b}'));
}

#[test]
fn provider_prompt_hides_header_values_and_names_only_the_key_variable() {
    let config = json!({
        "label": "Loop",
        "base_url": "http://127.0.0.1:9/v1",
        "api_key_env": "LOOP_KEY",
        "headers": [{"name": "Authorization", "value": {"literal": "Bearer sk-SECRET"}}]
    });
    let action = Action::RegisterProvider {
        name: "loop".into(),
        config,
    };
    let (title, detail) = approval_prompt("p", &action).unwrap().unwrap();
    assert!(title.contains("register a provider"));
    assert!(detail.contains("Base URL: http://127.0.0.1:9/v1"));
    assert!(detail.contains("API key variable: LOOP_KEY"));
    assert!(detail.contains("Authorization"));
    assert!(!detail.contains("sk-SECRET"));
}

#[test]
fn raw_secret_in_the_key_field_is_refused_without_echo() {
    let config = json!({"base_url": "http://127.0.0.1:9", "api_key_env": "sk-SECRET"});
    let error = parse_provider("loop", &config).unwrap_err();
    assert!(!format!("{error:#}").contains("sk-SECRET"));
}

#[test]
fn wrong_typed_field_error_does_not_quote_the_value() {
    let config = json!({"enabled": "sk-SECRET"});
    let error = parse_provider("loop", &config).unwrap_err();
    assert!(!format!("{error:#}").contains("sk-SECRET"));
}

#[test]
fn provider_registration_targets_the_loopback_endpoint_by_variable_name() {
    let config = json!({
        "label": "Loop",
        "base_url": "http://127.0.0.1:9/v1",
        "api_key_env": "LOOP_KEY"
    });
    let spec = parse_provider("loop", &config).unwrap();
    let changes = provider_changes("loop", &spec);
    assert!(changes.iter().any(|change| matches!(
        change,
        Change::Set { path, value }
            if path == "providers.loop.base_url" && value.as_str() == Some("http://127.0.0.1:9/v1")
    )));
    assert!(changes.iter().any(|change| matches!(
        change,
        Change::Set { path, value }
            if path == "providers.loop.api_key_env" && value.as_str() == Some("LOOP_KEY")
    )));
}

#[test]
fn provider_names_cannot_address_nested_settings() {
    assert!(parse_provider("a.b", &json!({})).is_err());
    assert!(ensure_provider_name("").is_err());
}

#[test]
fn renderer_body_lists_fields_then_markdown_and_falls_back_to_text() {
    let view = EntryRenderer {
        title: Some("Build".into()),
        fields: vec![("status".into(), "green".into())],
        markdown: Some("**done**".into()),
    };
    let stored = json!({
        "plugin": "ci",
        "kind": "build",
        "text": "ignored",
        "renderer": renderer_json(&view),
    });
    assert_eq!(
        entry_note(&stored),
        Some(("Build".to_owned(), "status: green\n**done**".to_owned()))
    );
    let plain = json!({"plugin": "ci", "kind": "build", "text": "plain", "renderer": null});
    assert_eq!(
        entry_note(&plain),
        Some(("ci: build".to_owned(), "plain".to_owned()))
    );
    let empty = json!({
        "plugin": "ci",
        "kind": "build",
        "text": "plain",
        "renderer": renderer_json(&EntryRenderer::default()),
    });
    assert_eq!(
        entry_note(&empty).map(|(_, body)| body),
        Some("plain".to_owned())
    );
}

#[test]
fn stored_records_are_sanitized_and_malformed_ones_are_skipped() {
    let stored = json!({
        "plugin": "ci",
        "kind": "build",
        "text": "a\u{1b}[31mred",
        "renderer": {"title": "T\u{7}itle", "markdown": "ok\u{1b}[0m"},
    });
    let (title, body) = entry_note(&stored).expect("well-formed record");
    assert!(!title.contains('\u{7}') && !title.contains('\u{1b}'));
    assert!(!body.contains('\u{1b}'));
    assert_eq!(entry_note(&json!({"plugin": "ci"})), None);
    assert_eq!(entry_note(&json!("not an object")), None);
}

#[test]
fn long_plugin_messages_are_clipped_in_prompts() {
    let action = Action::SendUserMessage {
        text: "x".repeat(PROMPT_CHARS + 50),
    };
    let (_, detail) = approval_prompt("p", &action).unwrap().unwrap();
    assert!(detail.chars().count() <= PROMPT_CHARS + 20);
    assert!(detail.ends_with('…'));
}

#[test]
fn resumed_transcript_shows_plugin_entries_between_their_messages() {
    let (_home, _project, app, shell) = super::super::testkit::shell();
    let mut session = Session::create(
        &app.layout,
        SessionMeta {
            model: "test-model".into(),
        },
    )
    .unwrap();
    session
        .append(json!({"role": "user", "content": "first question"}))
        .unwrap();
    session
        .append_custom_entry(
            PLUGIN_ENTRY_TYPE,
            json!({
                "plugin": "ci",
                "kind": "build",
                "text": "build passed",
                "renderer": renderer_json(&EntryRenderer {
                    title: Some("Build status".into()),
                    fields: vec![("status".into(), "green".into())],
                    markdown: None,
                }),
            }),
        )
        .unwrap();
    session
        .append(json!({"role": "user", "content": "second question"}))
        .unwrap();
    let path = session.path().expect("file-backed session").to_owned();
    drop(session);

    let resumed = Session::open(&path).unwrap();
    assert!(
        resumed
            .messages()
            .iter()
            .all(|message| !message.to_string().contains("build passed")),
        "plugin entries never enter model context"
    );

    let mut transcript = crate::ui::transcript::Transcript::new();
    crate::app::replay::replay_session(&mut transcript, &resumed);
    let ctx = crate::ui::transcript::TranscriptCtx {
        theme: shell.themes.current(),
        ui: &shell.ui,
        tick: 0,
        tools_expanded: false,
        expand_key: "expand",
    };
    let (rows, _) = transcript.document(100, &ctx);
    let text = rows
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let first = text.find("first question").expect("first message");
    // Notice titles render in the transcript's heading case.
    let status = text
        .to_lowercase()
        .find("build status")
        .expect("replayed entry title");
    let second = text.find("second question").expect("second message");
    assert!(first < status && status < second, "{text}");
    assert!(text.contains("status: green"), "{text}");
}
