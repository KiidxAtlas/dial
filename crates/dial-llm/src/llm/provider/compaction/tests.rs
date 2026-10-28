use super::*;
use llm::profiles::ModelProfile;

#[test]
fn compaction_summary_preserves_explicit_chatgpt_override_errors() {
    let mut backend = Backend::new(
        ProviderKind::ChatGpt {
            model: "account-model".into(),
            user_dir: Default::default(),
        },
        Default::default(),
        5,
    );
    assert!(backend.compaction_summary_profile(64).is_none());
    let request = Chat {
        messages: vec![json!({"role":"user","content":"Summarize."})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    for settings in ["max_tokens=128", "temperature=0.5", "top_p=0.9"] {
        let profile: ModelProfile = toml::from_str(&format!(
            "provider='chatgpt'\nmodel='account-model'\n{settings}\n[capabilities]\ntemperature=true\ntop_p=true"
        ))
        .unwrap();
        backend.next_request_profile(Some(profile));
        let profile = backend.compaction_summary_profile(request.max_tokens);
        let error =
            llm::responses::request_codex("account-model", &request, profile.as_ref()).unwrap_err();
        assert!(
            error.to_string().contains("ChatGPT plan usage"),
            "{error:#}"
        );
    }
}

#[test]
fn v2_compaction_retains_rejected_usage_without_duplicating_success_or_inventing_usage() {
    use crate::llm::fake::{self, Script};
    for (accepted, reported, streamed_item) in [
        (true, true, true),
        (false, true, true),
        (false, true, false),
        (false, false, true),
        (true, false, true),
    ] {
        let item = if accepted {
            json!({"type":"compaction","encrypted_content":"opaque-state"})
        } else {
            json!({"type":"compaction"})
        };
        let mut terminal = json!({"type":"response.completed","response":{"status":"completed","output":[item.clone()]}});
        if reported {
            terminal["response"]["usage"] = json!({"input_tokens":31,"output_tokens":4,"input_tokens_details":{"cached_tokens":7}});
        }
        let mut body = String::new();
        if streamed_item {
            body.push_str(&format!(
                "data: {}\n\n",
                json!({"type":"response.output_item.done","item":item})
            ));
        }
        body.push_str(&format!("data: {terminal}\n\n"));
        let server = fake::serve(vec![Script::Sse(body)]);
        let mut backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: server.url.clone(),
                model: "test-model".into(),
                api_key: None,
            },
            Default::default(),
            5,
        );
        backend.protocol = Some(llm::model_config::ApiProtocol::Responses);
        backend.set_usage_scope(Some(llm::usage::Scope::default().next_turn(0).unwrap()));
        let request = Chat {
            messages: vec![json!({"role":"user","content":"retain this user message"})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 64,
            seed: None,
        };
        let result = backend.compact_native(
            NativeCompactionRequest {
                request: &request,
                capability: Some(NativeCompactionCapability::ResponsesV2),
                streaming_v2: true,
                retained_user_budget: 128,
                first_kept: 0,
                instructions: "compact",
            },
            5,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(result.is_ok(), accepted);
        let groups = backend.usage_scope_snapshot();
        let counts = groups.workers;
        assert_eq!(counts.transport_attempts, 1);
        assert_eq!(counts.input, if reported { 31 } else { 0 });
        assert_eq!(counts.output, if reported { 4 } else { 0 });
        assert_eq!(counts.cache_read, if reported { 7 } else { 0 });
        assert_eq!(
            counts.partial_reported_attempts,
            u64::from(reported && !accepted)
        );
        assert_eq!(counts.reported_responses, u64::from(reported && accepted));
        assert_eq!(
            counts.unreported_attempts,
            u64::from(!reported || !accepted)
        );
        assert_eq!(counts.failed_requests, u64::from(!accepted));
        assert_eq!(counts.observed_cost_usd, None);
        assert_eq!(
            groups.by_purpose[&llm::usage::Purpose::Compaction].input,
            counts.input
        );
        assert_eq!(server.requests().len(), 1);
    }
}
