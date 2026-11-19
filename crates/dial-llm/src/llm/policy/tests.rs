use super::*;

#[test]
fn only_a_stated_output_maximum_is_reported_as_stated() {
    let unknown = ResolvedModel::resolve(
        "endpoint",
        "no-such-model",
        None,
        &ModelMetadata::default(),
        64_000,
    );
    assert_eq!(unknown.stated_max_output_tokens, None);
    assert_eq!(unknown.max_output_tokens, 8000, "context-derived fallback");

    let live = ModelMetadata {
        max_output_tokens: Some(128_000),
        ..Default::default()
    };
    let advertised = ResolvedModel::resolve("endpoint", "no-such-model", None, &live, 400_000);
    assert_eq!(advertised.stated_max_output_tokens, Some(128_000));
    assert_eq!(advertised.max_output_tokens, 128_000);

    let profile = ModelProfile {
        max_tokens: Some(16_000),
        ..Default::default()
    };
    let operator =
        ResolvedModel::resolve("endpoint", "no-such-model", Some(&profile), &live, 400_000);
    assert_eq!(
        operator.stated_max_output_tokens,
        Some(16_000),
        "profile wins"
    );
}
