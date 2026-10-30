use super::*;
use crate::storage::artifact::ArtifactStore;
use crate::tools::testkit;

#[test]
fn intervals_are_explicit_and_bounded() {
    assert!(interval(0, 120_000).is_ok());
    for (start, end) in [
        (0, 0),
        (3, 2),
        (0, 120_001),
        (86_400_000, 86_400_001),
        (u64::MAX, 0),
    ] {
        assert!(interval(start, end).is_err(), "{start}..{end}");
    }
}
#[test]
fn external_media_requires_both_confirmation_and_controller() {
    let (_dir, ws) = testkit::workspace();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        confirm: None,
        mutation_authorizer: None,
    };
    assert!(authorize(&ctx, "audio", "test").is_err());
    let ctx = ToolContext {
        confirm: Some(&|_| true),
        ..ctx
    };
    assert!(authorize(&ctx, "audio", "test").is_err());
    let ctx = ToolContext {
        mutation_authorizer: Some(&|_, _, _| Ok(())),
        ..ctx
    };
    assert!(authorize(&ctx, "audio", "test").is_ok());
}

/// Optional installed-converter tier. The test creates real PCM, not a mocked transcriber.
#[test]
fn installed_ffmpeg_replay_retains_real_clip_and_source_identity() {
    if ffmpeg().is_err() {
        eprintln!("optional ffmpeg replay tier unavailable");
        return;
    }
    let (dir, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let wav = crate::voice::wav::encode_pcm16(&vec![0.0; 16_000], 16_000);
    std::fs::write(dir.path().join("a.wav"), &wav).unwrap();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        confirm: Some(&|_| true),
        mutation_authorizer: Some(&|_, _, _| Ok(())),
    };
    let out = run(
        &ctx,
        Args {
            action: "replay".into(),
            path: "a.wav".into(),
            start_ms: 250,
            end_ms: 750,
            retain_source: true,
            parents: vec![],
            window_ms: None,
            silence_dbfs: None,
            clip_threshold: None,
            transient_db: None,
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    let data = out.data.unwrap();
    assert!(data["clip"].as_str().unwrap().starts_with("artifact://"));
    let record = ws
        .artifacts()
        .unwrap()
        .evidence_record(data["evidence_references"][0].as_str().unwrap())
        .unwrap();
    assert_eq!(
        record.evidence.context.snapshot.unwrap().bytes,
        wav.len() as u64
    );
    assert_eq!(
        record.evidence.location,
        Location::Time {
            start_ms: 250,
            end_ms: 750
        }
    );
    assert_eq!(
        record.evidence.context.parents,
        vec![data["clip"].as_str().unwrap().to_owned()]
    );
    let out = run(
        &ctx,
        Args {
            action: "replay".into(),
            path: "a.wav".into(),
            start_ms: 750,
            end_ms: 1500,
            retain_source: false,
            parents: vec![],
            window_ms: None,
            silence_dbfs: None,
            clip_threshold: None,
            transient_db: None,
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    let evidence = out.evidence().unwrap().unwrap();
    assert_eq!(
        evidence.items[0].location,
        Location::Time {
            start_ms: 750,
            end_ms: 1000
        }
    );
    assert_eq!(
        evidence.items[0].context.coverage.as_ref().unwrap().omitted,
        vec![Location::Time {
            start_ms: 1000,
            end_ms: 1500
        }]
    );
    let error = run(
        &ctx,
        Args {
            action: "replay".into(),
            path: "a.wav".into(),
            start_ms: 0,
            end_ms: 500,
            retain_source: false,
            parents: vec![],
            window_ms: None,
            silence_dbfs: None,
            clip_threshold: None,
            transient_db: None,
        },
        &AtomicBool::new(true),
    )
    .unwrap_err();
    assert!(error.to_string().contains("cancel"), "{error}");
}

#[test]
fn denied_or_cancelled_audio_never_retains_original_bytes() {
    let (dir, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    std::fs::write(dir.path().join("a.wav"), b"private original fixture").unwrap();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        confirm: None,
        mutation_authorizer: None,
    };
    let count = || walkdir_count(state.path());
    let before = count();
    for action in ["replay", "analyze"] {
        let args = || {
            serde_json::from_value(json!({"action":action,"path":"a.wav","start_ms":0,"end_ms":100,"retain_source":true})).unwrap()
        };
        assert!(run(&ctx, args(), &AtomicBool::new(false)).is_err());
        assert_eq!(count(), before, "denied {action} retained private bytes");
        assert!(run(&ctx, args(), &AtomicBool::new(true)).is_err());
        assert_eq!(count(), before, "cancelled {action} retained private bytes");
    }
}

fn walkdir_count(path: &std::path::Path) -> usize {
    std::fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walkdir_count(&path)
            } else {
                1
            }
        })
        .sum()
}

#[test]
fn exhausted_budget_refuses_audio_before_source_retention() {
    let (dir, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    std::fs::write(dir.path().join("a.wav"), b"private original fixture").unwrap();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        confirm: Some(&|_| true),
        mutation_authorizer: Some(&|_, _, _| Ok(())),
    };
    let perception = crate::tools::knowledge::perception::tools(&config).remove(0);
    let opened = perception.call(&ctx, &json!({"action":"open","question":"no audio","limits":{"images":0,"pages":0,"frames":0,"audio_ms":0,"operations":1}}));
    assert!(opened.data.unwrap()["budget"].is_object());
    let before = walkdir_count(state.path());
    for action in ["replay", "analyze"] {
        let args = serde_json::from_value(
            json!({"action":action,"path":"a.wav","start_ms":0,"end_ms":100,"retain_source":true}),
        )
        .unwrap();
        let error = run(&ctx, args, &AtomicBool::new(false)).unwrap_err();
        assert!(error.to_string().contains("budget exhausted"), "{error}");
        assert_eq!(
            walkdir_count(state.path()),
            before,
            "over-budget {action} retained original bytes"
        );
    }
    perception.call(&ctx, &json!({"action":"close","reason":"test finished"}));
}

#[test]
fn optional_signal_parameters_preserve_recorded_audio_contracts() {
    use crate::tools::surface::contracts::{identities, validate_history};
    let tools = tools(&Config::default());
    let current = tools[0].schema();
    let mut prior = current.clone();
    let properties = prior["function"]["parameters"]["properties"]
        .as_object_mut()
        .unwrap();
    for property in [
        "window_ms",
        "silence_dbfs",
        "clip_threshold",
        "transient_db",
    ] {
        properties.remove(property);
    }
    properties.get_mut("action").unwrap()["enum"] = json!(["transcribe", "refine", "replay"]);
    let mut registry = crate::tools::integrations::ext::Registry::new();
    registry.register(Arc::clone(&tools[0]));
    let old = identities(&[prior], &registry)["audio"].clone();
    let history = vec![json!({"role":"assistant", "dial_tool_contracts":{"call":old}})];
    validate_history(&history, std::slice::from_ref(&current), &registry).unwrap();
    let mut incompatible = current;
    incompatible["function"]["parameters"]["properties"]["path"]["type"] = json!("integer");
    assert!(validate_history(&history, &[incompatible], &registry).is_err());
}
