use super::route_request;

#[test]
fn route_request_prefers_fresh_owner() {
    assert_eq!(route_request("/api/items"), "api");
    assert_eq!(route_request("/index.html"), "static");
}
