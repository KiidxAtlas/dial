use super::*;
#[test]
fn visual_work_is_conditional_and_test_or_backend_edits_do_not_trigger_it() {
    assert!(useful("fix the spacing on this page"));
    assert!(useful("compare this picture with the rendered UI"));
    assert!(!useful("fix Docker image creation"));
    assert!(!useful("fix database query performance"));
    let mut review = Review::default();
    review.changed(Path::new("api.rs"));
    review.changed(Path::new("Button.test.tsx"));
    assert!(!review.missing((1, 0)));
    review.changed(Path::new("Button.tsx"));
    assert!(review.missing((2, 0)));
}
#[test]
fn only_rendered_pixels_delivered_after_latest_edit_satisfy_site_review() {
    let mut review = Review::new("fix the site layout");
    review.changed(Path::new("style.css"));
    review.captured((1, 0), false);
    review.delivered();
    assert!(
        review.missing((1, 0)),
        "a source image is not a rendered page"
    );
    review.captured((1, 0), true);
    assert!(
        review.missing((1, 0)),
        "capture and finish in one batch cannot inspect pixels"
    );
    review.delivered();
    assert!(!review.missing((1, 0)));
    assert!(
        review.missing((2, 0)),
        "later edits invalidate visual evidence"
    );
    assert!(
        review.missing((2, 1)),
        "opaque shell changes invalidate it too"
    );
}
#[test]
fn missing_visual_evidence_is_disclosed_in_the_summary() {
    let mut review = Review::new("inspect the screenshot");
    assert!(
        review
            .summary("done".into(), (0, 0))
            .contains("not completed")
    );
    review.captured((0, 0), false);
    review.delivered();
    assert_eq!(review.summary("done".into(), (0, 0)), "done");
}
