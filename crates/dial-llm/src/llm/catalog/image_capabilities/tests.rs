use super::*;
use crate::llm::catalog::ModelMetadata;

fn selection(provider: &str, model: &str, base_url: Option<&str>) -> ModelSelection {
    ModelSelection {
        provider: provider.into(),
        model: model.into(),
        base_url: base_url.map(str::to_owned),
        ..Default::default()
    }
}

fn entry(
    provider: &str,
    model: &str,
    base_url: Option<&str>,
    kind: ModelKind,
    image_generation: Option<ImageGenerationApi>,
) -> ModelEntry {
    ModelEntry {
        selection: selection(provider, model, base_url),
        label: model.into(),
        source: "test".into(),
        available: true,
        metadata: ModelMetadata {
            kind,
            image_generation,
            ..Default::default()
        },
    }
}

#[test]
fn discovery_caches_capable_and_candidate_models_and_preserves_verified() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![
        entry(
            "openrouter",
            "google/gemini-3.1-flash-image",
            Some("https://openrouter.ai/api/v1"),
            ModelKind::Chat,
            Some(ImageGenerationApi::ChatCompletions),
        ),
        entry(
            "openrouter",
            "anthropic/claude-opus",
            Some("https://openrouter.ai/api/v1"),
            ModelKind::Chat,
            Some(ImageGenerationApi::Disabled),
        ),
        entry(
            "openai",
            "gpt-image-1.5",
            Some("https://api.openai.com/v1"),
            ModelKind::Image,
            Some(ImageGenerationApi::Images),
        ),
        entry(
            "chatgpt",
            "gpt-6.2-astra",
            Some(chatgpt::BASE_URL),
            ModelKind::Unknown,
            None,
        ),
    ];

    update_from_discovery(dir.path(), &entries).unwrap();
    let cached = cached_capable(dir.path());
    assert_eq!(cached.len(), 3);

    let astra = lookup(dir.path(), &entries[3].selection).unwrap();
    assert_eq!(astra.status, ImageCapabilityStatus::Candidate);
    assert_eq!(astra.source, ImageCapabilitySource::Candidate);

    let gpt_img = lookup(dir.path(), &entries[2].selection).unwrap();
    assert_eq!(gpt_img.status, ImageCapabilityStatus::Capable);
    assert_eq!(gpt_img.source, ImageCapabilitySource::Documented);
    assert_eq!(gpt_img.api, Some(ImageGenerationApi::Images));

    // Recording a verified generation upgrades the candidate and survives a later discovery.
    record_verified(
        dir.path(),
        &entries[3].selection,
        ImageGenerationApi::Responses,
    )
    .unwrap();
    update_from_discovery(dir.path(), &entries).unwrap();
    let verified = lookup(dir.path(), &entries[3].selection).unwrap();
    assert_eq!(verified.status, ImageCapabilityStatus::Capable);
    assert_eq!(verified.source, ImageCapabilitySource::Verified);
    assert_eq!(verified.api, Some(ImageGenerationApi::Responses));

    // apply_cached enriches fresh catalog rows with verified image_generation.
    let mut fresh = vec![entries[3].clone()];
    assert_eq!(fresh[0].metadata.image_generation, None);
    apply_cached(dir.path(), &mut fresh);
    assert_eq!(
        fresh[0].metadata.image_generation,
        Some(ImageGenerationApi::Responses)
    );
}

#[test]
fn persisted_account_refusal_disables_chatgpt_entries_until_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let mut row = entry(
        "chatgpt",
        "gpt-6.2-astra",
        Some(chatgpt::BASE_URL),
        ModelKind::Unknown,
        None,
    );
    row.selection.account_id = Some("acct_free".into());

    assert!(row.accepts_image_role());
    record_account_refused(dir.path(), "chatgpt", "acct_free", "plan lacks hosted tool").unwrap();
    assert!(account_refused(dir.path(), "chatgpt", "acct_free"));
    assert!(!account_refused(dir.path(), "chatgpt", "acct_paid"));

    let mut rows = vec![row.clone()];
    apply_cached(dir.path(), &mut rows);
    assert_eq!(
        rows[0].metadata.image_generation,
        Some(ImageGenerationApi::Disabled)
    );
    assert!(!rows[0].accepts_image_role());

    clear_refusals(dir.path()).unwrap();
    assert!(!account_refused(dir.path(), "chatgpt", "acct_free"));
    let mut rows_after = vec![row];
    apply_cached(dir.path(), &mut rows_after);
    assert_eq!(rows_after[0].metadata.image_generation, None);
    assert!(rows_after[0].accepts_image_role());
}
