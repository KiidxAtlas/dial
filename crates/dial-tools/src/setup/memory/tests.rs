use super::*;

#[test]
fn doctor_text_marks_the_current_backend_and_lists_every_backend() {
    let config = Config::default();
    let text = doctor_text(&config);
    assert!(
        text.starts_with("Memory: Provenance facts — Ready"),
        "{text}"
    );
    for (id, _, _) in BACKENDS {
        assert!(text.contains(&format!("  {}", id.as_str())), "{text}");
    }
    assert!(text.contains("(current)"), "{text}");
    let local = text
        .lines()
        .find(|l| l.trim_start().starts_with("local"))
        .unwrap();
    assert!(local.contains("Not ready"), "{local}");
}

#[test]
fn doctor_text_reports_off_and_unmet_steps_of_the_selected_backend() {
    let mut config = Config::default();
    config.memory.enabled = false;
    assert!(doctor_text(&config).starts_with("Memory: off"));
    config.memory.enabled = true;
    config.memory.backend = MemoryBackendId::Semantic;
    let text = doctor_text(&config);
    assert!(
        text.starts_with("Memory: Semantic facts — Ready (optional steps left)"),
        "{text}"
    );
    assert!(
        text.contains("  Optional: Recall is keyword-only"),
        "{text}"
    );
}
