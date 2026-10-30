use super::*;
use crate::tools::testkit;

fn args() -> Args {
    Args {
        path: "v.mp4".into(),
        start_ms: 0,
        end_ms: 1000,
        seeks_ms: vec![0, 500],
        frame_budget: 2,
        retain_source: false,
        parents: vec![],
    }
}
#[test]
fn video_is_absent_by_default_and_runtime_refuses_stale_tool() {
    let mut config = Config::default();
    assert!(tools(&config).is_empty());
    let (_dir, ws) = testkit::workspace();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        confirm: None,
        mutation_authorizer: None,
    };
    let out = Video.call(&ctx, &json!({}));
    assert!(out.text.contains("disabled"));
    config.experimental.video_analysis = true;
    assert_eq!(tools(&config).len(), 1);
}
#[test]
fn video_budget_rejects_unbounded_duplicate_and_outside_seeks() {
    assert!(validate(&args()).is_ok());
    let mut a = args();
    a.frame_budget = 1;
    assert!(validate(&a).is_err());
    let mut a = args();
    a.seeks_ms = vec![];
    assert!(validate(&a).is_err());
    let mut a = args();
    a.seeks_ms = vec![500, 500];
    assert!(validate(&a).is_err());
    let mut a = args();
    a.seeks_ms = vec![1001];
    assert!(validate(&a).is_err());
    let mut a = args();
    a.end_ms = 120_001;
    assert!(validate(&a).is_err());
}

#[test]
fn installed_ffmpeg_frames_have_reachable_citations_and_honest_gaps() {
    let Ok(converter) = audio::ffmpeg() else {
        eprintln!("optional ffmpeg video tier unavailable");
        return;
    };
    let (dir, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(crate::storage::artifact::ArtifactStore::new(state.path()).unwrap());
    let spec = CommandSpec::new(converter)
        .arg("-nostdin")
        .arg("-v")
        .arg("error")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg("color=c=red:s=64x64:r=10:d=1")
        .arg("-c:v")
        .arg("mpeg4")
        .arg(dir.path().join("v.mp4"));
    process::run(
        &spec,
        None,
        &AtomicBool::new(false),
        Duration::from_secs(30),
    )
    .unwrap();
    let mut config = Config::default();
    config.experimental.video_analysis = true;
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        confirm: Some(&|_| true),
        mutation_authorizer: Some(&|_, _, _| Ok(())),
    };
    let out = run(&ctx, args(), &AtomicBool::new(false)).unwrap();
    let evidence = out.evidence().unwrap().unwrap();
    let coverage = evidence.items[0].context.coverage.as_ref().unwrap();
    assert_eq!(coverage.extent, CoverageExtent::Partial);
    assert_eq!(coverage.inspected.len(), 2);
    assert!(!coverage.omitted.is_empty());
    let data = out.data.unwrap();
    for frame in data["frames"].as_array().unwrap() {
        let citation = frame["citation"].as_str().unwrap();
        assert!(
            evidence.items[0]
                .context
                .parents
                .iter()
                .any(|p| p == citation)
        );
        let store = ws.artifacts().unwrap();
        let record = store.evidence_record(citation).unwrap();
        assert!(
            store
                .evidence_bytes(&record, 4 * 1024 * 1024)
                .unwrap()
                .starts_with(b"\x89PNG")
        );
        assert!(frame["actual_pts_ms"].is_null());
    }
}
