use super::{Axis, Family, Identity};

fn loop_family(name: &str) -> Option<Family> {
    Identity::new(name).family(Axis::LoopDetectors)
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
