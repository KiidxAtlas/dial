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
