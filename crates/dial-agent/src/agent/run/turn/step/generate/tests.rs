use super::*;

#[test]
fn generation_discard_precedence_is_deterministic_and_respects_interrupt() {
    assert!(matches!(
        classify_generation_discard(true, Some(anyhow::anyhow!("err")), true, None, None, false),
        Some(GenerationDiscard::DeadlineCancelled)
    ));
    assert!(matches!(
        classify_generation_discard(false, Some(anyhow::anyhow!("err")), true, None, None, true),
        Some(GenerationDiscard::SteeringError(_))
    ));
    assert!(matches!(
        classify_generation_discard(false, None, true, None, None, false),
        Some(GenerationDiscard::SteeringSuperseded)
    ));
    let paths = std::collections::BTreeSet::from(["src/lib.rs".to_owned()]);
    assert!(matches!(
        classify_generation_discard(false, None, false, None, Some(paths), false),
        Some(GenerationDiscard::PortableRule { .. })
    ));
    // External interrupt suppresses deadline/steering/rule discards so the interrupt handler runs.
    assert!(classify_generation_discard(true, None, true, None, None, true).is_none());
}
