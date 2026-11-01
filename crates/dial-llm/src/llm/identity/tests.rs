use super::{Axis, Family, Identity};

fn loop_family(name: &str) -> Option<Family> {
    Identity::new(name).family(Axis::LoopDetectors)
}

fn snapcompact_family(name: &str) -> Option<Family> {
    Identity::new(name).snapcompact_family()
}

#[test]
fn loop_detectors_use_only_the_basename_prefix() {
    assert_eq!(loop_family("google/gemini-2.5-pro"), Some(Family::Gemini));
    assert_eq!(
        loop_family("openrouter/deepseek-r1"),
        Some(Family::DeepSeek)
    );
    assert_eq!(loop_family("xai/grok-4"), Some(Family::Xai));
    assert_eq!(loop_family("anthropic/claude-sonnet"), None);
}

#[test]
fn name_containment_does_not_grant_loop_detectors() {
    assert_eq!(loop_family("vertex-gemini-2"), None);
    assert_eq!(loop_family("claude-gemini-proxy"), None);
}

#[test]
fn devin_payload_accepts_name_containment_or_authoritative_uid() {
    let devin = |name, uid: Option<&str>| {
        let identity = Identity::new(name);
        let identity = match uid {
            Some(uid) => identity.with_uid(uid),
            None => identity,
        };
        identity.family(Axis::DevinPayload)
    };
    assert_eq!(devin("vertex-gemini-2", None), Some(Family::Gemini));
    assert_eq!(
        devin("swe-1-6", Some("MODEL_GOOGLE_GEMINI_2_5_PRO")),
        Some(Family::Gemini)
    );
    assert_eq!(
        devin("swe-1-6", Some("model_google_gemini_2_5_pro")),
        None,
        "uid prefix is case-sensitive"
    );
    assert_eq!(devin("gpt-5", None), None);
}

#[test]
fn cloud_code_claude_label_uses_the_exact_leading_prefix() {
    let claude = |name| Identity::new(name).family(Axis::CloudCodeClaude);
    assert_eq!(claude("claude-sonnet-4-5"), Some(Family::Claude));
    assert_eq!(
        claude("claude-gemini-proxy"),
        Some(Family::Claude),
        "a custom name is not rejected"
    );
    assert_eq!(
        claude("anthropic/claude-sonnet"),
        None,
        "leading prefix, as written"
    );
    assert_eq!(claude("Claude-Sonnet"), None, "case-sensitive, as before");
}

#[test]
fn a_name_matching_two_families_resolves_by_declared_precedence_not_an_error() {
    // Devin: an authoritative uid outranks a conflicting name substring.
    let resolved = Identity::new("claude-gemini-proxy")
        .with_uid("MODEL_GOOGLE_GEMINI_X")
        .family(Axis::DevinPayload);
    assert_eq!(resolved, Some(Family::Gemini));
    assert_eq!(
        Identity::new("claude-gemini-proxy").family(Axis::DevinPayload),
        Some(Family::Gemini)
    );
}

#[test]
fn snapcompact_family_is_case_insensitive_and_ignores_gateway_prefixes() {
    assert_eq!(snapcompact_family("CLAUDE-OPUS-4-7"), Some(Family::Claude));
    assert_eq!(
        snapcompact_family("Google/Gemini-2.5-Pro"),
        Some(Family::Gemini)
    );
    assert_eq!(
        snapcompact_family("openrouter/anthropic/claude-opus-4-7"),
        Some(Family::Claude)
    );
    assert_eq!(
        snapcompact_family("anthropic.claude-3-5-sonnet"),
        Some(Family::Claude)
    );
}

#[test]
fn snapcompact_rejects_lookalike_names_and_middle_path_segments() {
    for name in [
        "claudeish-model",
        "claude_opus",
        "notgpt-5",
        "gpt5",
        "my-gemini-2",
        "anthropic/claude-x/private-model",
        "gpt-proxy/private-model",
    ] {
        assert_eq!(snapcompact_family(name), None, "{name}");
    }
}

#[test]
fn snapcompact_does_not_trust_the_upstream_uid() {
    let identity = Identity::new("private-model").with_uid("MODEL_GOOGLE_GEMINI_X");
    assert_eq!(identity.family(Axis::Snapcompact), None);
    assert_eq!(identity.family(Axis::DevinPayload), Some(Family::Gemini));
}

#[test]
fn opus_4_7_or_later_compares_the_version_not_the_text() {
    let opus = |name| Identity::new(name).opus_4_7_or_later();
    assert!(opus("claude-opus-4-7"));
    assert!(opus("claude-opus-4-8"));
    assert!(opus("claude-opus-4.7-20260101"));
    assert!(opus("claude-opus-5"));
    assert!(opus("claude-opus-4-10"), "a two-digit minor is numeric");
    assert!(!opus("claude-opus-4-6"));
    assert!(!opus("claude-opus-4"));
    assert!(
        !opus("claude-opus-4-20250514"),
        "a date after the major is not a minor version"
    );
    assert!(!opus("claude-sonnet-4-7"));
    assert!(!opus("private-claude-opus-5"));
}

#[test]
fn opus_4_7_or_later_accepts_gateway_and_bedrock_regional_ids() {
    let opus = |name| Identity::new(name).opus_4_7_or_later();
    assert!(opus("openrouter/anthropic/claude-opus-4-8"));
    assert!(opus("anthropic.claude-opus-4-7-v1:0"));
    assert!(
        opus("us.anthropic.claude-opus-4-7-v1:0"),
        "regional Bedrock id should keep the Claude vendor prefix"
    );
}

#[test]
fn fable_and_mythos_match_only_the_codename_segment() {
    let codename = |name| Identity::new(name).fable_or_mythos();
    assert!(codename("claude-fable-latest"));
    assert!(codename("claude-mythos-preview"));
    assert!(codename("anthropic.claude-mythos-preview"));
    assert!(
        !codename("claude-fableproxy"),
        "a proxy name that merely starts with the codename is not Fable"
    );
    assert!(
        !codename("claude-mythosproxy"),
        "a proxy name that merely starts with the codename is not Mythos"
    );
    assert!(
        !codename("fable-latest"),
        "the claude- family prefix is required"
    );
    assert!(!codename("claude-opus-4-7"));
}

#[test]
fn gpt_5_4_or_later_compares_gpt_and_codex_versions() {
    let gpt = |name| Identity::new(name).gpt_5_4_or_later();
    assert!(gpt("gpt-5.4"));
    assert!(gpt("gpt-5-4-mini"));
    assert!(gpt("codex-5.4"));
    assert!(gpt("gpt-6"));
    assert!(!gpt("gpt-5.3"));
    assert!(!gpt("gpt-4.1"));
    assert!(
        !gpt("gpt-5-20250514"),
        "a date after the major is not a minor version"
    );
    assert!(!gpt("my-gpt-5.4"), "the stem must lead the model leaf");
}
