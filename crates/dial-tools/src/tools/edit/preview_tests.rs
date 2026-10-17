use crate::tools::*;
#[test]
fn compact_previews_require_retrievable_successful_observations() {
    assert_eq!(
        preview_limit(128000, "read", false, true),
        MODEL_OUTPUT_LIMIT
    );
    assert_eq!(
        preview_limit(128000, "read", false, false),
        MODEL_OUTPUT_LIMIT
    );
    assert_eq!(
        preview_limit(128000, "bash", true, true),
        MODEL_OUTPUT_LIMIT
    );
    assert_eq!(
        preview_limit(128000, "edit", false, true),
        MODEL_OUTPUT_LIMIT
    );
    assert_eq!(preview_limit(8000, "read", false, true), 4096);
    assert_eq!(
        preview_limit(128000, "grep", false, true),
        MODEL_OUTPUT_LIMIT
    );
}
