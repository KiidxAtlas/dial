use super::*;
use crate::llm::{InferencePhase, ModelProgress};

#[test]
fn silence_is_visible_in_every_model_phase_and_clears_on_progress() {
    for phase in [
        InferencePhase::Loading,
        InferencePhase::Processing,
        InferencePhase::Generating,
    ] {
        let mut status = StatusData {
            phase: Phase::Working,
            model_progress: Some(ModelProgress::new(phase)),
            model_silence_secs: 14,
            ..StatusData::default()
        };
        assert!(!status.activity_label().contains("no progress"));
        status.model_silence_secs = 15;
        assert!(status.activity_label().starts_with("no progress for 15s"));
        status.model_silence_secs = 0;
        assert!(!status.activity_label().contains("no progress"));
        status.activity = "stopping after bash".into();
        assert_eq!(status.activity_label(), "stopping after bash");
    }
}

#[test]
fn prompt_fraction_needs_real_counts_and_clamps_to_the_total() {
    let mut p = ModelProgress::new(InferencePhase::Processing);
    let mut status = StatusData::default();
    for (done, total, expected) in [
        (None, None, None),
        (Some(5), Some(0), None),
        (Some(5), Some(10), Some(0.5)),
        (Some(20), Some(10), Some(1.0)),
    ] {
        p.processed_tokens = done;
        p.prompt_tokens = total;
        status.model_progress = Some(p.clone());
        assert_eq!(status.prompt_fraction(), expected);
    }
    status.activity = "stopping".into();
    assert_eq!(status.prompt_fraction(), None);
    status.activity.clear();
    p.phase = InferencePhase::Generating;
    status.model_progress = Some(p);
    assert_eq!(status.prompt_fraction(), None);
}

#[test]
fn generation_keeps_the_current_operation_and_sanitizes_it() {
    let mut status = StatusData {
        model_progress: Some(ModelProgress::new(InferencePhase::Generating)),
        ..StatusData::default()
    };
    for activity in ["reasoning", "writing", "preparing read"] {
        status.activity = activity.into();
        assert!(status.activity_label().starts_with(activity));
        assert!(status.activity_label().contains("generating"));
    }
    status.activity = "preparing read\nfile\x1b[31m".into();
    let label = status.activity_label();
    assert!(!label.contains('\n') && !label.contains('\x1b'));
}

#[test]
fn eta_and_cached_prompt_tokens_show_only_when_known_and_positive() {
    let mut p = ModelProgress::new(InferencePhase::Processing);
    p.prompt_tokens = Some(10_000);
    p.processed_tokens = Some(2_000);
    let mut status = StatusData {
        model_progress: Some(p.clone()),
        ..StatusData::default()
    };
    let plain = status.activity_label();
    assert!(
        !plain.contains("left") && !plain.contains("cached"),
        "{plain}"
    );
    p.cached_tokens = Some(0);
    p.eta_secs = Some(f64::NAN);
    status.model_progress = Some(p.clone());
    let label = status.activity_label();
    assert!(
        !label.contains("left") && !label.contains("cached"),
        "{label}"
    );
    p.cached_tokens = Some(3_900);
    p.eta_secs = Some(64.2);
    status.model_progress = Some(p.clone());
    let label = status.activity_label();
    assert!(label.ends_with(" · 3.9k cached · ~1m05s left"), "{label}");
    p.eta_secs = Some(0.0);
    status.model_progress = Some(p.clone());
    assert!(!status.activity_label().contains("left"));

    let mut g = ModelProgress::new(InferencePhase::Generating);
    g.output_tokens = Some(40);
    g.cached_tokens = Some(5_000);
    g.eta_secs = Some(12.0);
    status.model_progress = Some(g.clone());
    let label = status.activity_label();
    assert!(
        label.ends_with("generating 40 tokens · ~12s left"),
        "{label}"
    );
    assert!(!label.contains("cached"), "{label}");
    g.eta_secs = Some(f64::INFINITY);
    status.model_progress = Some(g);
    assert!(!status.activity_label().contains("left"));
}
