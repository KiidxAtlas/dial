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
