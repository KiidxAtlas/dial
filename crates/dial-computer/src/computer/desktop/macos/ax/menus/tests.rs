use super::*;

#[test]
fn menu_dispatch_refuses_missing_or_sibling_key_window() {
    for focused in [None, Some(42104)] {
        let mut dispatched = false;
        let error = require_exact_key_context(55126, 42103, focused)
            .map(|()| dispatched = true)
            .unwrap_err();
        assert!(!dispatched);
        assert_eq!(
            error.code,
            crate::computer::desktop::error::ErrorCode::BackgroundUnavailable
        );
        assert!(error.message.contains("no command was dispatched"));
        assert!(error.message.contains("takeover:true"));
    }
    assert!(require_exact_key_context(55126, 42103, Some(42103)).is_ok());
}
