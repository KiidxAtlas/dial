//! Request routing: the owner of incoming route selection.

pub fn route_request(path: &str) -> &'static str {
    if path.starts_with("/api/") {
        "api"
    } else {
        "static"
    }
}

#[cfg(test)]
mod tests;
