use crate::config::compaction::continue_band;

use crate::config::compaction::CompactionConfig;
use crate::config::compaction::CompactionIdentity;
use crate::config::compaction::CompactionMethod;
use crate::config::compaction::CompactionOptions;

use super::*;

#[test]
fn automatic_compaction_skips_unsupported_native_but_honors_explicit_requests() {
    let mut policy = CompactionConfig::default();
    let automatic = CompactionOptions::default();
    let methods = policy.available_methods(&automatic, false);
    assert!(!methods.contains(&CompactionMethod::Remote));
    assert!(methods.contains(&CompactionMethod::Soft));
    assert!(
        policy
            .available_methods(&automatic, true)
            .contains(&CompactionMethod::Remote)
    );
    let explicit = CompactionOptions {
        method: Some(CompactionMethod::Remote),
        ..Default::default()
    };
    assert_eq!(
        policy.available_methods(&explicit, false),
        vec![CompactionMethod::Remote, CompactionMethod::Soft]
    );
    policy.remote_endpoint = Some("https://example.com/compact".into());
    assert!(
        policy
            .available_methods(&automatic, false)
            .contains(&CompactionMethod::Remote)
    );
    policy.remote_enabled = false;
    assert!(
        !policy
            .available_methods(&automatic, false)
            .contains(&CompactionMethod::Remote)
    );
}

#[test]
fn cut_works_inside_one_user_turn_and_never_splits_a_tool_pair() {
    // One user request, then a long chain of call/result pairs: the shape with no earlier user message.
    let mut m = vec![json!({"role":"user","content":"do the task"})];
    for i in 0..20 {
        m.push(json!({"role":"assistant","content":"","tool_calls":[{"id":format!("c{i}"),"function":{"name":"read","arguments":"{}"}}]}));
        m.push(json!({"role":"tool","tool_call_id":format!("c{i}"),"content":"x".repeat(4000)}));
    }
    let boundary = cut(&m, 3_000).expect("a long single turn must be compactable");
    assert!(
        boundary > 0 && boundary < m.len() - 1,
        "cut {boundary} of {}",
        m.len()
    );
    assert_ne!(
        m[boundary]["role"], "tool",
        "the tail may not start on an orphaned tool result"
    );
    assert!(
        m[boundary..].iter().map(estimate).sum::<u64>() <= 3_000 + 2_200,
        "tail stays near the budget"
    );
    assert_eq!(
        cut(&m[..1], 3_000),
        None,
        "a lone user request has nothing worth summarising"
    );
}

#[test]
fn auto_threshold_uses_the_reserve_subtracted_large_window() {
    let policy = CompactionConfig::default();
    let identity = CompactionIdentity {
        provider: "p",
        model: "m",
        agent: None,
    };
    // 200k window: reserve is 15% (30k), auto trigger uses all of the remaining 170k.
    assert_eq!(
        policy.effective_threshold(identity, 200_000).unwrap(),
        170_000
    );
    assert!(
        !policy
            .should_compact(identity, 200_000, 170_000, false)
            .unwrap()
    );
    assert!(
        policy
            .should_compact(identity, 200_000, 170_001, false)
            .unwrap()
    );
    let mut percent = policy.clone();
    percent.threshold_percent = Some(50.0);
    assert_eq!(
        percent.effective_threshold(identity, 200_000).unwrap(),
        100_000
    );
}

#[test]
fn small_default_windows_reserve_a_share_of_the_window() {
    let policy = CompactionConfig::default();
    // (window, reserve): the 32k and 64k rows were 16384 before; 8k and 16k already used 15%.
    for (window, reserve) in [
        (8_192, 1_228),
        (16_384, 2_457),
        (32_768, 6_553),
        (65_536, 13_107),
        // Large windows keep the existing 15% / fixed 16k rule byte-for-byte.
        (128_000, 19_200),
        (200_000, 30_000),
        (1_000_000, 150_000),
    ] {
        assert_eq!(policy.reserve(window), reserve, "window {window}");
    }
    // Explicit reserve_tokens is never rescaled for small windows.
    let explicit = CompactionConfig {
        reserve_tokens: Some(16_384),
        ..CompactionConfig::default()
    };
    assert_eq!(explicit.reserve(32_768), 16_384);
}

#[test]
fn small_window_thresholds_sit_between_reserve_and_window() {
    let policy = CompactionConfig::default();
    let identity = CompactionIdentity {
        provider: "p",
        model: "m",
        agent: None,
    };
    // Windows where the keep-recent budget fits under the trigger must trigger above it;
    // the band is checked for every window in small_default_keep_recent_* tests.
    for (window, keep_fits) in [
        (8_192_u32, false),
        (16_384, false),
        (32_768, true),
        (65_536, true),
    ] {
        let reserve = u64::from(policy.reserve(window));
        let keep = policy.keep_recent_budget(identity, window, 0).unwrap();
        let threshold = policy.effective_threshold(identity, window).unwrap();
        assert!(
            threshold > reserve,
            "window {window}: threshold {threshold}"
        );
        assert!(
            threshold < u64::from(window) - reserve,
            "window {window}: threshold {threshold}"
        );
        let band = continue_band(threshold);
        assert!(band > 0 && band < threshold, "window {window}: band {band}");
        if keep_fits {
            assert!(
                threshold > keep,
                "window {window}: threshold {threshold} must exceed keep-recent"
            );
        }
    }
}

#[test]
fn oversized_single_turn_advances_to_a_harder_cut_before_summarizing() {
    // A single turn whose kept tail alone exceeds the keep budget must be cut further up front;
    // otherwise the summary pass runs twice against the turn's worker budget and fails.
    let mut m = vec![json!({"role":"user","content":"do the task"})];
    for i in 0..20 {
        m.push(json!({"role":"assistant","content":"","tool_calls":[{"id":format!("c{i}"),"function":{"name":"read","arguments":"{}"}}]}));
        m.push(json!({"role":"tool","tool_call_id":format!("c{i}"),"content":"x".repeat(4000)}));
    }
    let keep = cut(&m, 20_000).expect("the first cut must exist");
    let harder = cut(&m, 4_096).expect("the harder cut must exist");
    assert!(
        harder > keep,
        "harder cut {harder} must exceed keep cut {keep}"
    );
    assert_ne!(m[harder]["role"], "tool");
}

#[test]
fn small_window_default_keep_recent_keeps_the_next_request_in_band() {
    // A cut of the default keep budget must leave summary + kept tail (+ fixed system/tool
    // tokens) within continue_band, or continue_allowed is false and the turn pauses.
    let policy = CompactionConfig::default();
    let identity = CompactionIdentity {
        provider: "p",
        model: "m",
        agent: None,
    };
    const FIXED: u64 = 1_000;
    for (window, scaled) in [
        (8_192_u32, true),
        (16_384, true),
        (32_768, true),
        (65_536, false),
    ] {
        let threshold = policy.effective_threshold(identity, window).unwrap();
        let band = continue_band(threshold);
        let keep = policy.keep_recent_budget(identity, window, FIXED).unwrap();
        assert_eq!(keep < 20_000, scaled, "window {window}: keep {keep}");
        let system = json!({"role":"system","content":"s".repeat(4 * FIXED as usize)});
        let mut messages = vec![system, json!({"role":"user","content":"do the task"})];
        let mut call = 0;
        let base = Chat {
            messages: vec![],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 0,
            seed: None,
        };
        loop {
            let request = Chat {
                messages: messages.clone(),
                ..base.clone()
            };
            if request_tokens_with_policy(&request, &policy, identity) > threshold * 5 / 4 {
                break;
            }
            call += 1;
            messages.push(json!({"role":"assistant","content":"","tool_calls":[{"id":format!("c{call}"),"function":{"name":"read","arguments":"{}"}}]}));
            messages.push(
                json!({"role":"tool","tool_call_id":format!("c{call}"),"content":"x".repeat(1600)}),
            );
        }
        let request = Chat {
            messages,
            ..base.clone()
        };
        let cut_index = cut(&request.messages, keep)
            .unwrap_or_else(|| panic!("window {window}: default keep must cut"));
        assert!(
            cut_index > 2,
            "window {window}: cut {cut_index} must summarize older history"
        );
        let summary = u64::from(policy.summary_budget(window));
        let mut kept = vec![json!({"role":"user","content":"s".repeat(4 * summary as usize)})];
        kept.extend(
            request.messages[cut_index..]
                .iter()
                .filter(|m| is_history(m))
                .cloned(),
        );
        let candidate = replacement_request(&request, &kept);
        let after = request_tokens_with_policy(&candidate, &policy, identity);
        assert!(
            after <= band,
            "window {window}: post-compaction {after} exceeds continue band {band}"
        );
    }
}

#[test]
fn explicit_and_large_window_keep_recent_is_unchanged() {
    let identity = CompactionIdentity {
        provider: "p",
        model: "m",
        agent: None,
    };
    let default = CompactionConfig::default();
    for window in [65_537_u32, 200_000, 1_000_000] {
        assert_eq!(
            default.keep_recent_budget(identity, window, 1_000).unwrap(),
            20_000,
            "large window {window}"
        );
    }
    for configured in [0_u32, 3_000, 12_000, 40_000] {
        let policy = CompactionConfig {
            keep_recent_tokens: configured,
            ..CompactionConfig::default()
        };
        for window in [8_192_u32, 32_768, 65_536, 200_000] {
            assert_eq!(
                policy.keep_recent_budget(identity, window, 1_000).unwrap(),
                u64::from(configured),
                "explicit {configured} at window {window}"
            );
        }
    }
}

#[test]
fn supplemental_context_counts_toward_native_budget_but_is_not_history() {
    let dir = tempfile::tempdir().unwrap();
    let policy = CompactionConfig::default();
    let data = "untrusted data ".repeat(1024);
    let context = CompactionContext {
        identity: CompactionIdentity {
            provider: "p",
            model: "m",
            agent: None,
        },
        window: 32_768,
        summary_window: 32_768,
        summary_output_limit: None,
        timeout_secs: 5,
        user_dir: dir.path(),
        native_request: None,
        retained_overhead_tokens: 0,
        supplemental_context: Some(&data),
    };
    let mut request = Chat {
        messages: vec![json!({"role":"user","content":"source"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let before = request_tokens_with_policy(&request, &policy, context.identity);
    let note = context.supplemental_message().unwrap();
    assert_eq!(note["role"], "user");
    assert!(!is_history(&note));
    request.messages.insert(0, note);
    assert!(request_tokens_with_policy(&request, &policy, context.identity) > before + 1000);
    let absent = CompactionContext {
        supplemental_context: None,
        ..context
    };
    assert!(absent.supplemental_message().is_none());
}

#[test]
fn summary_planning_reserves_commit_overhead_and_keeps_complete_tool_groups() {
    let user_dir = tempfile::tempdir().unwrap();
    let policy = CompactionConfig::default();
    let identity = CompactionIdentity {
        provider: "endpoint",
        model: "summarizer",
        agent: None,
    };
    let mut messages = vec![
        json!({"role":"system","content":"s".repeat(4000)}),
        json!({"role":"user","content":"inspect the project"}),
    ];
    for i in 0..40 {
        messages.push(json!({"role":"assistant","content":"","tool_calls":[{"id":format!("c{i}"),"function":{"name":"read","arguments":"{}"}}]}));
        messages
            .push(json!({"role":"tool","tool_call_id":format!("c{i}"),"content":"x".repeat(4000)}));
    }
    let request = Chat {
        messages,
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    for (window, overhead) in [(16_384, 3_000), (32_768, 8_000), (65_536, 20_000)] {
        let context = CompactionContext {
            identity,
            window,
            summary_window: 200_000,
            summary_output_limit: Some(1_000),
            timeout_secs: 5,
            user_dir: user_dir.path(),
            native_request: None,
            retained_overhead_tokens: overhead,
            supplemental_context: None,
        };
        let mut backend = compaction_backend();
        let mut summary_requests = Vec::new();
        let mut calls = 0;
        let prepared = compact_history(
            &mut backend,
            HistoryCompaction {
                request: &request,
                policy: &policy,
                context,
                options: &CompactionOptions {
                    method: Some(CompactionMethod::Soft),
                    require_method: true,
                    keep_recent_tokens: Some(20_000),
                    ..Default::default()
                },
            },
            Arc::new(AtomicBool::new(false)),
            &mut crate::agent::NullUi,
            &mut |_, chat, _| {
                calls += 1;
                summary_requests.push(serde_json::to_vec(&chat.messages).unwrap().len());
                // Exercise recovery when returned text is larger than the output allowance.
                Ok(if window == 32_768 {
                    "S".repeat(16_000)
                } else {
                    "Summary: inspected the older results; continue the task.".into()
                })
            },
            None,
        )
        .unwrap_or_else(|error| panic!("window {window}, overhead {overhead}: {error:#}"));
        let history = prepared.history(&request);
        let candidate = replacement_request(&request, &history);
        assert!(
            calls > 0,
            "window {window}: the summary route was exercised"
        );
        assert!(
            request_tokens_with_policy(&candidate, &policy, identity) + overhead
                <= prompt_budget(&policy, window, backend.output_budget(request.max_tokens)),
            "window {window}: the summary and tail must leave room for commit-time context"
        );
        assert!(
            complete_boundary(&history, history.len()),
            "window {window}"
        );
        assert_eq!(
            &history[history.len() - 2..],
            &request.messages[request.messages.len() - 2..],
            "window {window}: preserve the newest call/result group verbatim"
        );
        if window == 32_768 {
            assert!(
                calls > 1,
                "an oversized summary requires an incremental fold"
            );
            assert!(
                summary_requests.last().unwrap() < summary_requests.first().unwrap(),
                "the fold must not replay the full original prefix"
            );
        }
    }
}

#[test]
fn impossible_retained_context_is_rejected_before_requesting_a_summary() {
    let user_dir = tempfile::tempdir().unwrap();
    let policy = CompactionConfig::default();
    for (label, overhead, newest_bytes) in [
        ("fixed carry cannot fit", 32_768, 100),
        ("newest indivisible group cannot fit", 0, 200_000),
    ] {
        let request = Chat {
            messages: vec![
                json!({"role":"user","content":"older task"}),
                json!({"role":"assistant","content":"old answer"}),
                json!({"role":"assistant","content":"","tool_calls":[{"id":"new","function":{"name":"read","arguments":"{}"}}]}),
                json!({"role":"tool","tool_call_id":"new","content":"x".repeat(newest_bytes)}),
            ],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 64,
            seed: None,
        };
        let result = compact_history(
            &mut compaction_backend(),
            HistoryCompaction {
                request: &request,
                policy: &policy,
                context: CompactionContext {
                    identity: CompactionIdentity {
                        provider: "endpoint",
                        model: "summarizer",
                        agent: None,
                    },
                    window: 32_768,
                    summary_window: 200_000,
                    summary_output_limit: Some(1_000),
                    timeout_secs: 5,
                    user_dir: user_dir.path(),
                    native_request: None,
                    retained_overhead_tokens: overhead,
                    supplemental_context: None,
                },
                options: &CompactionOptions {
                    method: Some(CompactionMethod::Soft),
                    require_method: true,
                    ..Default::default()
                },
            },
            Arc::new(AtomicBool::new(false)),
            &mut crate::agent::NullUi,
            &mut |_, _, _| panic!("{label}: no summary request can recover this context"),
            None,
        );
        assert!(result.is_err(), "{label}: refuse without replacing history");
    }
}

#[test]
fn safe_manual_noop_does_not_reserve_context_that_will_not_be_regenerated() {
    let user_dir = tempfile::tempdir().unwrap();
    let request = Chat {
        messages: vec![json!({"role":"user","content":"a short task"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let result = compact_history(
        &mut compaction_backend(),
        HistoryCompaction {
            request: &request,
            policy: &CompactionConfig::default(),
            context: CompactionContext {
                identity: CompactionIdentity {
                    provider: "endpoint",
                    model: "summarizer",
                    agent: None,
                },
                window: 32_768,
                summary_window: 200_000,
                summary_output_limit: Some(1_000),
                timeout_secs: 5,
                user_dir: user_dir.path(),
                native_request: None,
                retained_overhead_tokens: 32_768,
                supplemental_context: None,
            },
            options: &CompactionOptions::default(),
        },
        Arc::new(AtomicBool::new(false)),
        &mut crate::agent::NullUi,
        &mut |_, _, _| panic!("a safe manual no-op must not request a summary"),
        None,
    );
    assert!(result.err().unwrap().is::<NothingToCompact>());
}

/// A forced compaction of a few short turns cannot shrink anything. It must decline without a
/// summary model call and report `Unprofitable`, which manual callers turn into "unchanged".
#[test]
fn forced_compaction_of_tiny_history_declines_without_a_summary_call() {
    let user_dir = tempfile::tempdir().unwrap();
    let request = Chat {
        messages: vec![
            json!({"role":"system","content":"system"}),
            json!({"role":"user","content":"Remember: the goal is ORCHID-739."}),
            json!({"role":"assistant","content":"Understood."}),
            json!({"role":"user","content":"Why does the distractor not replace it?"}),
            json!({"role":"assistant","content":"It is archival data."}),
            json!({"role":"user","content":"Complete the goal now."}),
        ],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let result = compact_history(
        &mut compaction_backend(),
        HistoryCompaction {
            request: &request,
            policy: &CompactionConfig::default(),
            context: CompactionContext {
                identity: CompactionIdentity {
                    provider: "endpoint",
                    model: "summarizer",
                    agent: None,
                },
                window: 32_768,
                summary_window: 200_000,
                summary_output_limit: Some(1_000),
                timeout_secs: 5,
                user_dir: user_dir.path(),
                native_request: None,
                retained_overhead_tokens: 0,
                supplemental_context: None,
            },
            options: &CompactionOptions {
                keep_recent_tokens: Some(0),
                ..Default::default()
            },
        },
        Arc::new(AtomicBool::new(false)),
        &mut crate::agent::NullUi,
        &mut |_, _, _| panic!("too little history to shrink must not request a summary"),
        None,
    );
    let error = result.err().unwrap();
    assert!(error.is::<Unprofitable>(), "{error:#}");
}

fn compaction_backend() -> Backend {
    Backend::new(
        crate::llm::provider::ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".to_owned(),
            model: "summarizer".to_owned(),
            api_key: None,
        },
        std::path::PathBuf::from("/nonexistent/python"),
        5,
    )
}

fn png_header(width: u32, height: u32) -> String {
    use base64::Engine;
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[test]
fn request_accounting_matches_token_policy_for_tools_replay_images_and_guidance() {
    let policy = CompactionConfig::default();
    let identity = CompactionIdentity {
        provider: "test",
        model: "model",
        agent: None,
    };
    let png = png_header(640, 480);
    let snapcompact = json!({
        "role": "user",
        "dial_snapcompact": {"frames": 1},
        "content": [
            {"type": "text", "text": "archived"},
            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{png}")}}
        ],
    });
    let mut request = Chat {
        messages: vec![
            json!({"role": "system", "content": "rules"}),
            json!({"role": "user", "content": "look", "dial_images": [{"path": "a.png"}]}),
            json!({"role": "assistant", "content": "a", "dial_provider_content": [{"type": "opaque", "data": "x".repeat(500)}]}),
            snapcompact.clone(),
            json!({"role": "tool", "tool_call_id": "c1", "content": "out"}),
        ],
        tools: vec![
            json!({"type": "function", "function": {"name": "bash", "parameters": {"type": "object"}}}),
            json!({"type": "function", "function": {"name": "read"}}),
        ],
        temperature: 0.0,
        max_tokens: 8192,
        seed: None,
    };
    let mut accounting = request_accounting_with_policy(&request, &policy, identity);
    let expected_tools: u64 = request.tools.iter().map(estimate).sum();
    assert_eq!(
        accounting.tokens,
        request_tokens_with_policy(&request, &policy, identity)
    );
    assert_eq!(accounting.messages.len(), request.messages.len());
    assert_eq!(
        accounting.tool_bytes,
        request.tools.iter().map(json_bytes).collect::<Vec<_>>()
    );
    assert!(expected_tools > 0);

    // Snapcompact frame: text at its wire estimate plus the PNG pixel ceiling and fixed overheads.
    let frame_only = Chat {
        messages: vec![snapcompact.clone()],
        tools: vec![],
        ..request.clone()
    };
    let text_part = json!({"type": "text", "text": "archived"});
    assert_eq!(
        request_tokens_with_policy(&frame_only, &policy, identity),
        64 + estimate(&text_part) + 640 * 480 + 16
    );

    // Pressure guidance appended after measurement is priced by the appended message only.
    let guidance = json!({"role": "user", "content": "pressure guidance", "dial_transient": true});
    let updated = accounting.push_message(&guidance, &policy, identity);
    request.messages.push(guidance);
    assert_eq!(
        updated,
        request_tokens_with_policy(&request, &policy, identity)
    );
    assert_eq!(accounting.tokens, updated);
    assert_eq!(accounting.messages.len(), request.messages.len());
}

#[test]
fn request_accounting_preserves_wire_fallbacks_and_retained_output_floors() {
    let policy = CompactionConfig::default();
    let identity = CompactionIdentity {
        provider: "test",
        model: "model",
        agent: None,
    };
    let messages = vec![
        json!({"role":"assistant","content":"small","dial_compaction_output_tokens":9000}),
        json!({"role":"user","dial_snapcompact":{},"content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,malformed"}}]}),
        json!({"role":"user","dial_snapcompact":{},"content":"not an image array"}),
        json!({"role":"assistant","content":"portable","dial_provider_content":[{"opaque":"雪".repeat(200)}]}),
    ];
    let expected = messages.iter().map(estimate).sum::<u64>();
    assert!(expected >= 9000);
    let request = Chat {
        messages,
        tools: vec![],
        temperature: 0.0,
        max_tokens: 1024,
        seed: None,
    };
    assert_eq!(
        request_tokens_with_policy(&request, &policy, identity),
        expected
    );
    let accounting = request_accounting_with_policy(&request, &policy, identity);
    assert_eq!(accounting.tokens, expected);
    assert_eq!(
        accounting.messages,
        request.messages.iter().map(wire_parts).collect::<Vec<_>>()
    );
}

#[test]
fn explicit_openai_accounting_counts_and_unknown_routes_keep_estimates() {
    let policy = CompactionConfig::default();
    let request = Chat {
        messages: vec![
            json!({"role":"system","content":"rules"}),
            json!({"role":"user","content":"雪 café ".repeat(300)}),
        ],
        tools: vec![
            json!({"type":"function","function":{"name":"read","description":"Read a file","parameters":{"type":"object"}}}),
        ],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let unknown = CompactionIdentity {
        provider: "p",
        model: "unknown",
        agent: None,
    };
    let openai = CompactionIdentity {
        provider: "openai",
        model: "gpt-4o",
        agent: None,
    };
    let estimated: u64 = request
        .messages
        .iter()
        .chain(&request.tools)
        .map(estimate)
        .sum();
    assert_eq!(
        request_tokens_with_policy(&request, &policy, unknown),
        estimated
    );
    assert_eq!(
        request_accounting_with_policy(&request, &policy, unknown).tokens,
        estimated
    );
    let counted = crate::llm::wire::counting::request_tokens(
        "openai",
        "gpt-4o",
        &request.messages,
        &request.tools,
    );
    assert_eq!(
        request_tokens_with_policy(&request, &policy, openai),
        counted
    );
    assert_eq!(
        request_accounting_with_policy(&request, &policy, openai).tokens,
        counted
    );
    assert_ne!(
        counted, estimated,
        "a documented tokenizer must not reuse the byte estimate"
    );
}

#[test]
fn known_model_push_message_matches_a_full_recount() {
    let policy = CompactionConfig::default();
    let openai = CompactionIdentity {
        provider: "openai",
        model: "gpt-4o",
        agent: None,
    };
    let mut request = Chat {
        messages: vec![json!({"role":"user","content":"history 雪"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let mut accounting = request_accounting_with_policy(&request, &policy, openai);
    let guidance = json!({"role":"user","content":"pressure guidance","dial_transient":true});
    let updated = accounting.push_message(&guidance, &policy, openai);
    request.messages.push(guidance);
    assert_eq!(
        updated,
        request_tokens_with_policy(&request, &policy, openai)
    );
    assert_eq!(accounting.tokens, updated);
    assert_eq!(accounting.messages.len(), request.messages.len());
}

#[test]
fn cut_with_known_model_counts_tokens_and_never_splits_a_tool_pair() {
    let policy = CompactionConfig::default();
    let openai = CompactionIdentity {
        provider: "openai",
        model: "gpt-4o",
        agent: None,
    };
    let mut m = vec![json!({"role":"user","content":"do the task"})];
    for i in 0..20 {
        m.push(json!({"role":"assistant","content":"","tool_calls":[{"id":format!("c{i}"),"function":{"name":"read","arguments":"{}"}}]}));
        m.push(json!({"role":"tool","tool_call_id":format!("c{i}"),"content":"lorem ipsum dolor ".repeat(400)}));
    }
    let boundary = cut_with_policy(&m, 3_000, &policy, openai)
        .expect("a long single turn must be compactable");
    assert!(
        boundary > 0 && boundary < m.len() - 1,
        "cut {boundary} of {}",
        m.len()
    );
    assert_ne!(
        m[boundary]["role"], "tool",
        "the tail may not start on an orphaned tool result"
    );
    assert_eq!(cut_with_policy(&m[..1], 3_000, &policy, openai), None);
}

#[test]
fn live_handoff_keeps_the_exact_recent_tail_and_portable_fallback_is_non_destructive() {
    use crate::llm::provider::ProviderKind;
    for rejected in [false, true] {
        let request = Chat {
            messages: vec![
                json!({"role":"system","content":"stable rules"}),
                json!({"role":"user","content":"earlier task"}),
                json!({"role":"assistant","content":"earlier evidence ".repeat(1_000)}),
                json!({"role":"user","content":"latest exact request"}),
            ],
            tools: vec![
                json!({"type":"function","function":{"name":"read","parameters":{"type":"object"}}}),
            ],
            temperature: 0.0,
            max_tokens: 64,
            seed: None,
        };
        let original = (
            request.messages.clone(),
            request.tools.clone(),
            request.temperature,
            request.max_tokens,
            request.seed,
        );
        let mut backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: "https://example.com/v1".into(),
                model: "test".into(),
                api_key: None,
            },
            Default::default(),
            5,
        );
        let policy = CompactionConfig::default();
        let options = CompactionOptions {
            method: Some(CompactionMethod::Handoff),
            require_method: true,
            keep_recent_tokens: Some(64),
            ..Default::default()
        };
        let mut calls = 0;
        let prepared = compact_history_with_execution(
            &mut backend,
            HistoryCompaction {
                request: &request,
                policy: &policy,
                options: &options,
                context: CompactionContext {
                    identity: CompactionIdentity {
                        provider: "p",
                        model: "m",
                        agent: None,
                    },
                    window: 32_000,
                    summary_window: 32_000,
                    summary_output_limit: Some(128),
                    timeout_secs: 5,
                    user_dir: std::path::Path::new("."),
                    native_request: Some(&request),
                    retained_overhead_tokens: 0,
                    supplemental_context: None,
                },
            },
            Arc::new(AtomicBool::new(false)),
            &mut crate::agent::NullUi,
            &mut |_, summary_request, _| {
                calls += 1;
                let live = summary_request
                    .messages
                    .iter()
                    .any(|m| m["dial_compaction_handoff"] == true);
                if calls == 1 {
                    assert!(live);
                    assert_eq!(
                        summary_request.messages[..request.messages.len()],
                        request.messages
                    );
                    assert_eq!(summary_request.tools, request.tools);
                    assert!(
                        summary_request.messages.last().unwrap()["content"]
                            .as_str()
                            .unwrap()
                            .contains("remain verbatim")
                    );
                    if rejected {
                        anyhow::bail!("provider rejects tool_choice none");
                    }
                } else {
                    assert!(!live);
                    assert!(summary_request.tools.is_empty());
                }
                Ok("earlier compact context".into())
            },
            None,
            &mut SummaryExecution {
                live_handoff: true,
                parallel: None,
            },
        )
        .unwrap();
        assert_eq!(calls, if rejected { 2 } else { 1 });
        let history = prepared.history(&request);
        assert_eq!(history.last(), request.messages.last());
        assert!(prepared.outcome.tokens_after < prepared.outcome.tokens_before);
        assert_eq!(
            (
                request.messages,
                request.tools,
                request.temperature,
                request.max_tokens,
                request.seed
            ),
            original
        );
    }
}
