use super::*;
use serde_json::json;

#[test]
fn parses_valid_pid_and_rejects_others() {
    assert_eq!(parse_pid(Some("42"), "info").unwrap(), 42);
    assert!(parse_pid(Some("1"), "info").is_err());
    assert!(parse_pid(Some("0"), "info").is_err());
    assert!(parse_pid(Some("web"), "info").is_err());
    assert!(parse_pid(None, "stop").is_err());
}

#[test]
fn finds_row_by_pid() {
    let rows = vec![json!({"pid": 7, "kind": "models"}), json!({"pid": 9})];
    assert_eq!(find_row(&rows, 9).unwrap()["pid"], 9);
    assert!(find_row(&rows, 3).is_err());
}

#[test]
fn formats_bytes_with_units() {
    assert_eq!(format_bytes(None), "-");
    assert_eq!(format_bytes(Some(512)), "512 B");
    assert_eq!(format_bytes(Some(1536)), "1.5 KiB");
    assert_eq!(format_bytes(Some(3 * 1024 * 1024)), "3.0 MiB");
}

#[test]
fn table_has_header_and_aligned_rows() {
    let rows = vec![json!({
        "pid": 42,
        "kind": "models",
        "phase": "running",
        "memory_bytes": 1048576,
        "cpu_percent": 1.5,
        "model": "mlx-model",
    })];
    let table = format_table(&rows);
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with("PID"));
    assert!(lines[1].contains("42"));
    assert!(lines[1].contains("1.0 MiB"));
    assert!(lines[1].ends_with("mlx-model"));
}

#[test]
fn label_prefers_model_then_project() {
    assert_eq!(label(&json!({"model": "m", "project": "p"})), "m");
    assert_eq!(label(&json!({"model": "", "project": "p"})), "p");
    assert_eq!(label(&json!({})), "-");
}
