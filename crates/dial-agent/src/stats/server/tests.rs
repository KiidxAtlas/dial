use super::*;
use std::fs;

#[test]
fn percent_round_trip_matches_encode_uri_component() {
    assert_eq!(
        percent_encode("/a b/é+&=?.jsonl"),
        "%2Fa%20b%2F%C3%A9%2B%26%3D%3F.jsonl"
    );
    assert_eq!(percent_encode("(*)!~'-_."), "(*)!~'-_.");
    let raw = "/tmp/x y/%20+é.jsonl";
    assert_eq!(percent_decode(&percent_encode(raw)), raw);
    assert_eq!(percent_decode("100%"), "100%");
    assert_eq!(percent_decode("%zz"), "%zz");
}

#[test]
fn query_param_picks_first_matching_key() {
    assert_eq!(
        query_param("x=1&session=%2Ftmp%2Fa.jsonl", "session").as_deref(),
        Some("/tmp/a.jsonl")
    );
    assert_eq!(query_param("session", "session").as_deref(), Some(""));
    assert_eq!(query_param("other=1", "session"), None);
}

#[test]
fn only_loopback_host_names_reach_the_routes() {
    let request = |host: &str| format!("GET /api/stats HTTP/1.1\r\nHost: {host}\r\n\r\n");
    assert!(loopback_host(&request("127.0.0.1:4100"), 4100));
    assert!(loopback_host(&request("LocalHost:4100"), 4100));
    assert!(!loopback_host(&request("evil.example:4100"), 4100));
    assert!(!loopback_host(&request("127.0.0.1:4101"), 4100));
    assert!(loopback_host(&request("127.0.0.1"), 4100));
    assert!(!loopback_host(&request("evil.example"), 4100));
    assert!(!loopback_host("GET / HTTP/1.1\r\n\r\n", 4100));
    // A header after the blank line is body text, not a header.
    assert!(!loopback_host(
        "GET / HTTP/1.1\r\n\r\nHost: 127.0.0.1:4100\r\n",
        4100
    ));
}

#[test]
fn trace_route_refuses_paths_outside_sessions_tree() {
    let home = tempfile::tempdir().unwrap();
    let user_dir = home.path();
    let project = user_dir.join("projects/demo-1234abcd");
    fs::create_dir_all(project.join("sessions")).unwrap();
    fs::write(project.join("notes.jsonl"), "{}\n").unwrap();
    fs::write(user_dir.join("outside.jsonl"), "{}\n").unwrap();
    for target in [
        format!(
            "/api/trace?session={}",
            percent_encode(&project.join("notes.jsonl").to_string_lossy())
        ),
        format!(
            "/api/trace?session={}",
            percent_encode(&user_dir.join("outside.jsonl").to_string_lossy())
        ),
        format!("/api/trace?session={}", percent_encode("/etc/hosts")),
        format!(
            "/api/trace?session={}",
            percent_encode(&format!(
                "{}/projects/demo-1234abcd/sessions/../../../outside.jsonl",
                user_dir.display()
            ))
        ),
    ] {
        assert_eq!(route(user_dir, &target).0, "403 Forbidden", "{target}");
    }
    assert_eq!(route(user_dir, "/api/trace").0, "400 Bad Request");
    assert_eq!(route(user_dir, "/nope").0, "404 Not Found");
}

fn get(user_dir: &Path, host: &str, target: &str) -> (&'static str, &'static str, String) {
    respond(
        user_dir,
        4100,
        &format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n\r\n"),
    )
}

#[test]
fn foreign_host_is_refused_before_any_route() {
    let home = tempfile::tempdir().unwrap();
    for target in ["/api/version", "/api/models", "/api/request/1"] {
        assert_eq!(
            get(home.path(), "evil.example:4100", target).0,
            "403 Forbidden",
            "{target}"
        );
    }
}

#[test]
fn version_route_reports_build_info_json() {
    let home = tempfile::tempdir().unwrap();
    let (status, content_type, body) = get(home.path(), "127.0.0.1:4100", "/api/version");
    assert_eq!(status, "200 OK");
    assert_eq!(content_type, JSON);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(value["version"].is_string());
}

#[test]
fn dashboard_json_routes_answer_on_an_empty_home() {
    let home = tempfile::tempdir().unwrap();
    for target in [
        "/api/stats",
        "/api/models",
        "/api/tools?range=7d",
        "/api/requests?limit=5",
        "/api/frustration?range=24h",
    ] {
        let (status, content_type, body) = get(home.path(), "127.0.0.1:4100", target);
        assert_eq!(status, "200 OK", "{target}: {body}");
        assert_eq!(content_type, JSON, "{target}");
        serde_json::from_str::<serde_json::Value>(&body).unwrap();
    }
}

#[test]
fn live_route_is_not_found_without_a_session() {
    let home = tempfile::tempdir().unwrap();
    assert_eq!(
        get(home.path(), "127.0.0.1:4100", "/api/live").0,
        "404 Not Found"
    );
}

#[test]
fn request_detail_rejects_non_integer_ids_and_reports_missing_rows() {
    let home = tempfile::tempdir().unwrap();
    assert_eq!(
        get(home.path(), "127.0.0.1:4100", "/api/request/abc").0,
        "400 Bad Request"
    );
    assert_eq!(
        get(home.path(), "127.0.0.1:4100", "/api/request/999999").0,
        "404 Not Found"
    );
}

#[test]
fn list_routes_validate_limit_and_range() {
    let home = tempfile::tempdir().unwrap();
    for target in [
        "/api/requests?limit=0",
        "/api/requests?limit=501",
        "/api/requests?limit=many",
        "/api/models?range=bogus",
        "/api/frustration?range=bogus",
    ] {
        assert_eq!(
            get(home.path(), "127.0.0.1:4100", target).0,
            "400 Bad Request",
            "{target}"
        );
    }
}
