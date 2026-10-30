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
        messages.push(json!({"role":"tool","tool_call_id":format!("c{i}"),"content":"x".repeat(4000)}));
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
        assert!(calls > 0, "window {window}: the summary route was exercised");
        assert!(
            request_tokens_with_policy(&candidate, &policy, identity) + overhead
                <= prompt_budget(&policy, window, backend.output_budget(request.max_tokens)),
            "window {window}: the summary and tail must leave room for commit-time context"
        );
        assert!(complete_boundary(&history, history.len()), "window {window}");
        assert_eq!(
            &history[history.len() - 2..],
            &request.messages[request.messages.len() - 2..],
            "window {window}: preserve the newest call/result group verbatim"
        );
        if window == 32_768 {
            assert!(calls > 1, "an oversized summary requires an incremental fold");
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
