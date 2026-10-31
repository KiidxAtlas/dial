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
    assert!(!review.missing([1, 0, 0]));
    review.changed(Path::new("Button.tsx"));
    assert!(review.missing([2, 0, 0]));
}
#[test]
fn pasted_tracebacks_and_identifiers_do_not_request_visual_work() {
    let paste = "why does this crash\nTraceback (most recent call last):\n  File \"drawing.py\", line 40, in __init__\n    self._build_toolbar()\n  canvas.bind(\"<Button-1>\", self._paint)\nAttributeError: module 'tkinter.ttk' has no attribute 'Checkbar'";
    assert!(!useful(paste));
    assert!(useful("build a page with a button"));
}
#[test]
fn pagination_wording_does_not_request_visual_work() {
    // Benchmark follow-up: "page" as pagination plus an ordinary edit verb is not UI work.
    assert!(!useful(
        "Correction: keep the page size at 25 and undo any change to it. What I actually need is page_count(None) returning 0. Verify it."
    ));
    assert!(!useful("Raise the pagination default to 50 rows per page."));
    assert!(!useful("fix the page cache eviction"));
    assert!(useful("fix the page layout"));
    assert!(useful("the page looks broken on mobile"));
}
#[test]
fn explicit_visual_inspection_requests_require_delivered_pixels() {
    for prompt in ["do visual inspection", "visually verify the game"] {
        let mut review = Review::new(prompt);
        assert_eq!(review.status([0, 0, 0]), "missing", "{prompt}");
        review.captured([0, 0, 0], false);
        review.delivered();
        assert_eq!(review.status([0, 0, 0]), "observed", "{prompt}");
    }
    assert!(!useful("fix the visual studio build error"));
}
#[test]
fn only_rendered_pixels_delivered_after_latest_edit_satisfy_site_review() {
    let mut review = Review::new("fix the site layout");
    review.changed(Path::new("style.css"));
    review.captured([1, 0, 0], false);
    review.delivered();
    assert!(
        review.missing([1, 0, 0]),
        "a source image is not a rendered page"
    );
    review.captured([1, 0, 0], true);
    assert!(
        review.missing([1, 0, 0]),
        "capture and finish in one batch cannot inspect pixels"
    );
    review.delivered();
    assert!(!review.missing([1, 0, 0]));
    assert!(
        review.missing([2, 0, 0]),
        "later edits invalidate visual evidence"
    );
    assert!(
        review.missing([2, 1, 0]),
        "opaque shell changes invalidate it too"
    );
    assert!(
        review.missing([1, 0, 1]),
        "extension effects invalidate pixels too"
    );
}
#[test]
fn missing_visual_evidence_is_disclosed_in_the_summary() {
    let mut review = Review::new("inspect the screenshot");
    assert!(
        review
            .summary("done".into(), [0, 0, 0])
            .contains("not completed")
    );
    review.captured([0, 0, 0], false);
    review.delivered();
    assert_eq!(review.summary("done".into(), [0, 0, 0]), "done");
}
