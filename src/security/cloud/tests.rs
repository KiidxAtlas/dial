use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::thread;

use super::*;

/// Serves exactly `routes.len()` requests on 127.0.0.1:0. Each route is `(path, status, body)`.
/// Requests without `Authorization: Bearer tok` get 401.
fn serve(routes: Vec<(&'static str, u16, String)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        for _ in 0..routes.len() {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let path = request
                .split_whitespace()
                .nth(1)
                .unwrap()
                .split('?')
                .next()
                .unwrap()
                .to_owned();
            let authorized = request
                .to_ascii_lowercase()
                .contains("authorization: bearer tok");
            let (status, body) = if !authorized {
                (401, "{}".to_owned())
            } else {
                routes
                    .iter()
                    .find(|(route, _, _)| *route == path)
                    .map(|(_, status, body)| (*status, body.clone()))
                    .unwrap_or((404, "{}".to_owned()))
            };
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    base
}

fn token_access() -> Access {
    Box::new(|| Ok(("acct-1".to_owned(), "tok".to_owned())))
}

fn configuration_json() -> String {
    json!({
        "items": [{
            "hid": "cfg_1",
            "id": "src_1",
            "scan_input": {
                "repo_id": "repo_1",
                "repo_url": "https://github.com/acme/widgets.git",
                "environment_id": "env_1",
                "state": "enabled"
            },
            "scans_remaining": 3
        }],
        "next_cursor": null
    })
    .to_string()
}

fn finding_summary_json() -> String {
    json!({
        "items": [
            {"hid": "f1", "configured_scan_id": "src_1"},
            {"hid": "other", "configured_scan_id": "elsewhere"}
        ],
        "next_cursor": null
    })
    .to_string()
}

fn finding_detail_json() -> String {
    json!({
        "hid": "f1",
        "criticality": "high",
        "status": "fixed",
        "commit_analysis": {
            "title": "Hardcoded key",
            "rule_id": "R1",
            "category": "secrets",
            "validated": true,
            "validation_report": "confirmed",
            "validation_confidence": 0.9,
            "relevant_lines": [
                {"path": "src/lib.rs", "start_line_number": 10, "end_line_number": 12}
            ]
        }
    })
    .to_string()
}

#[test]
fn lookback_parses_days_and_all() {
    assert_eq!(parse_lookback("all").unwrap(), Lookback::All);
    assert_eq!(parse_lookback("30").unwrap(), Lookback::Days(30));
    assert!(parse_lookback("0").is_err());
    assert!(parse_lookback("-3").is_err());
    assert!(parse_lookback("soon").is_err());
}

#[test]
fn repository_identity_matches_scp_and_https() {
    assert_eq!(
        repository_identity("git@github.com:Acme/Widgets.git"),
        "github.com/acme/widgets"
    );
    assert_eq!(
        repository_identity("https://github.com/acme/widgets.git/"),
        "github.com/acme/widgets"
    );
    assert_eq!(
        repository_identity("ssh://git@GitHub.com/acme/widgets"),
        "github.com/acme/widgets"
    );
}

#[test]
fn normalize_path_rejects_escapes() {
    assert_eq!(
        normalize_path(Some(&json!("./src\\lib.rs"))).as_deref(),
        Some("src/lib.rs")
    );
    assert_eq!(normalize_path(Some(&json!("../x"))), None);
    assert_eq!(normalize_path(Some(&json!("/etc/passwd"))), None);
    assert_eq!(normalize_path(Some(&json!("C:/x"))), None);
}

#[test]
fn normalize_finding_maps_fields() {
    let finding =
        normalize_finding(&serde_json::from_str(&finding_detail_json()).unwrap()).unwrap();
    assert_eq!(finding.rule_id, "R1");
    assert_eq!(finding.category, "secrets");
    assert_eq!(finding.severity, Severity::High);
    assert_eq!(finding.confidence, Confidence::High);
    assert_eq!(finding.disposition, Disposition::Fixed);
    assert_eq!(finding.validation_status, Some(ValidationStatus::Validated));
    assert_eq!(finding.validation_summary.as_deref(), Some("confirmed"));
    assert_eq!(finding.validation_evidence.len(), 1);
    assert_eq!(finding.locations[0].path, "src/lib.rs");
    assert_eq!(finding.locations[0].start_line, Some(10));
    assert_eq!(finding.locations[0].end_line, Some(12));
    assert!(finding.id.starts_with("secf_"));
}

#[test]
fn normalize_finding_without_location_fails() {
    let raw = json!({"hid": "f", "commit_analysis": {"title": "t"}});
    assert!(normalize_finding(&raw).is_err());
}

#[test]
fn jwt_subject_reads_sub_then_user_id() {
    let payload = URL_SAFE_NO_PAD.encode(br#"{"user_id":"user-9"}"#);
    let token = format!("h.{payload}.s");
    assert_eq!(jwt_subject(&token).unwrap(), "user-9");
    assert!(jwt_subject("not-a-jwt").is_err());
}

#[test]
fn unauthorized_twice_reports_refresh_failure() {
    let base = serve(vec![
        ("/scan_configurations", 200, configuration_json()),
        ("/scan_configurations", 200, configuration_json()),
    ]);
    let client = CloudClient::with_access(
        &base,
        Box::new(|| Ok(("acct".to_owned(), "wrong".to_owned()))),
    );
    let error = client.all_configurations().unwrap_err().to_string();
    assert_eq!(error, "Codex Security cloud authentication refresh failed");
}

#[test]
fn server_error_names_endpoint() {
    let base = serve(vec![("/scan_configurations", 500, "{}".to_owned())]);
    let client = CloudClient::with_access(&base, token_access());
    let error = client.all_configurations().unwrap_err().to_string();
    assert_eq!(
        error,
        "Codex Security cloud request failed (500) at scan_configurations"
    );
}

#[test]
fn pull_imports_attributed_findings_over_loopback() {
    let base = serve(vec![
        ("/scan_configurations", 200, configuration_json()),
        (
            "/scan_configurations/cfg_1/stats",
            200,
            json!({
                "pending_commits": 0,
                "finished_commits": 4,
                "failed_commits": 0,
                "high_findings": 1,
                "last_scanned_commit_hash": "abc123",
                "current_step": "done"
            })
            .to_string(),
        ),
        ("/scan-findings", 200, finding_summary_json()),
        ("/scan-findings/f1", 200, finding_detail_json()),
    ]);
    let client = CloudClient::with_access(&base, token_access());

    let repo = tempfile::tempdir().unwrap();
    let root = repo.path().to_string_lossy().into_owned();
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo.path())
        .status()
        .unwrap();
    assert!(init.success());
    let remote = Command::new("git")
        .args(["remote", "add", "origin", "git@github.com:acme/widgets.git"])
        .current_dir(repo.path())
        .status()
        .unwrap();
    assert!(remote.success());

    let user_dir = tempfile::tempdir().unwrap();
    let store = Store::open(user_dir.path(), &root).unwrap();
    let scan = pull(&client, &store, "src_1").unwrap();

    assert_eq!(scan.status, ScanStatus::Completed);
    assert_eq!(scan.target_kind, TargetKind::Imported);
    assert_eq!(scan.revision.as_deref(), Some("abc123"));
    assert_eq!(scan.findings.len(), 1);
    assert_eq!(scan.cloud.as_ref().unwrap().configuration_id, "cfg_1");
    let reloaded = store.load_scan(&scan.id).unwrap();
    assert_eq!(reloaded.findings, scan.findings);
    let dir = store.dir().join("scans").join(&scan.id);
    assert!(dir.join("results.sarif").is_file());
    assert!(dir.join("report.md").is_file());
}
