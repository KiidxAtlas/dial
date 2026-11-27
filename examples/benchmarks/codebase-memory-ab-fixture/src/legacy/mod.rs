//! Retired routing path kept for a compatibility shim. Not the current owner.

pub fn route_request_legacy(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "static"
    } else {
        "api"
    }
}
