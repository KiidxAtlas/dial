use super::*;

#[test]
fn cancelled_geometry_refresh_never_opens_a_portal_session() {
    let source = control::CancellationSource::default();
    let token = source.token();
    source.cancel();
    let result = control::with_token_for_test(&token, geometry);
    assert_eq!(result.unwrap_err().code, token.check().unwrap_err().code);
}
