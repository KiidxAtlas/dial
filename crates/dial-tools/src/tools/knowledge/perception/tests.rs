use super::*;
use crate::tools::testkit;
fn limits() -> Value {
    json!({"images":2,"pages":1,"frames":6,"audio_ms":30000,"operations":8})
}
#[test]
fn budget_requires_human_confirmation_and_cannot_be_replaced_or_escaped_directly() {
    let (_dir, ws) = testkit::workspace();
    let config = Config::default();
    let mut ctx = ToolContext {
        workspace: &ws,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let args = json!({"action":"open","question":"Find the failure","limits":limits()});
    assert!(run(&ctx, &args, &AtomicBool::new(false)).is_err());
    ctx.confirm = Some(&|_| true);
    run(&ctx, &args, &AtomicBool::new(false)).unwrap();
    assert!(run(&ctx, &args, &AtomicBool::new(false)).is_err());
    reserve(
        &ctx,
        Spend {
            frames: 6,
            audio_ms: 30000,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        reserve(
            &ctx,
            Spend {
                frames: 1,
                ..Default::default()
            }
        )
        .is_err()
    );
    assert!(
        reserve(
            &ctx,
            Spend {
                audio_ms: 1,
                ..Default::default()
            }
        )
        .is_err()
    );
    assert_eq!(
        status(&ws)["budget"]["operations"],
        1,
        "failed dimension check must be atomic"
    );
    assert!(require_selected_pdf(&ws).is_err());
    ctx.confirm = None;
    assert!(
        run(
            &ctx,
            &json!({"action":"close","reason":"done"}),
            &AtomicBool::new(false)
        )
        .is_err()
    );
    ctx.confirm = Some(&|_| true);
    run(
        &ctx,
        &json!({"action":"close","reason":"resolved"}),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(status(&ws)["budget"].is_null());
    assert!(
        run(
            &ctx,
            &json!({"action":"step","reason":"look","tool":"video","request":{}}),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}
#[test]
fn concurrent_reservations_never_overdraw_and_other_workspace_is_independent() {
    let (_dir, ws) = testkit::workspace();
    let (_other, other) = testkit::workspace();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        mutation_authorizer: None,
        confirm: Some(&|_| true),
    };
    run(
        &ctx,
        &json!({"action":"open","question":"inspect","limits":limits()}),
        &AtomicBool::new(false),
    )
    .unwrap();
    let success = std::thread::scope(|scope| {
        let jobs = (0..16)
            .map(|_| {
                scope.spawn(|| {
                    reserve_workspace(
                        &ws,
                        Spend {
                            frames: 1,
                            ..Default::default()
                        },
                    )
                    .is_ok()
                })
            })
            .collect::<Vec<_>>();
        jobs.into_iter()
            .map(|job| job.join().unwrap())
            .filter(|ok| *ok)
            .count()
    });
    assert_eq!(success, 6);
    assert_eq!(status(&ws)["budget"]["used"]["frames"], 6);
    assert!(
        reserve_workspace(
            &other,
            Spend {
                frames: 100,
                ..Default::default()
            }
        )
        .is_ok()
    );
    run(
        &ctx,
        &json!({"action":"close","reason":"test complete"}),
        &AtomicBool::new(false),
    )
    .unwrap();
}

#[test]
fn direct_visual_and_retained_png_inspection_share_the_live_budget() {
    let (dir, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(crate::storage::artifact::ArtifactStore::new(state.path()).unwrap());
    image::RgbaImage::new(2, 2)
        .save(dir.path().join("image.png"))
        .unwrap();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        confirm: Some(&|_| true),
        mutation_authorizer: None,
    };
    let cancel = AtomicBool::new(false);
    run(
        &ctx,
        &json!({"action":"open","question":"inspect","limits":limits()}),
        &cancel,
    )
    .unwrap();
    let out = crate::tools::fs::visual::execute(
        &ctx,
        &json!({"action":"inspect","path":"image.png"}),
        &cancel,
    );
    let data = out.data.unwrap();
    let reference = data["frames"][0]["citation"].as_str().unwrap();
    let evidence = super::super::evidence::tools(&config).remove(0);
    let inspected = evidence.call(&ctx, &json!({"action":"inspect","reference":reference}));
    assert!(inspected.data.unwrap()["artifact"].is_object());
    assert_eq!(status(&ws)["budget"]["used"]["images"], 2);
    let crop = evidence.call(&ctx, &json!({"action":"crop","reference":reference,"region":{"x":0,"y":0,"width":500000,"height":500000}}));
    assert!(crop.text.contains("budget exhausted"), "{}", crop.text);
    let direct = crate::tools::fs::visual::execute(
        &ctx,
        &json!({"action":"inspect","path":"image.png"}),
        &cancel,
    );
    assert!(direct.text.contains("budget exhausted"));
    assert_eq!(status(&ws)["budget"]["operations"], 2);
    run(
        &ctx,
        &json!({"action":"close","reason":"test finished"}),
        &cancel,
    )
    .unwrap();
}
