use super::*;

fn ident() -> Identity {
    Identity {
        home: "/Users/alice".into(),
        user: "alice".into(),
        host: "mbp".into(),
    }
}

fn fake_key() -> String {
    format!("ghp_{}", "a".repeat(36))
}

fn fixture() -> String {
    [
        r#"{"type":"session","id":"abc-123","started":"2026-01-01T00:00:00.000Z","cwd":"/Users/alice/dev/proj/src/main.rs","model":"claude-haiku-5-5","version":1}"#.to_string(),
        r#"{"type":"message","id":"m1","parent":null,"ts":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"deploy with token KEY"}}"#.replace("KEY", &fake_key()),
        r#"not json at all"#.to_string(),
        r#"{"type":"message","id":"m2","parent":"m1","ts":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":"","tool_calls":[{"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"cat /Users/alice/secret.txt\"}"}}]}}"#.to_string(),
        r#"{"type":"mystery","id":"x1","note":"hello from /Users/alice"}"#.to_string(),
    ]
    .join("\n")
}

#[test]
fn keeps_record_count_ids_and_parents() {
    let mut a = Anonymizer::new(ident());
    let out = a.session_jsonl(fixture().as_bytes()).unwrap();
    let text = String::from_utf8(out.bytes).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "malformed line dropped, others kept");
    assert_eq!(out.malformed, 1);
    let m2: Value = serde_json::from_str(lines[2]).unwrap();
    assert_eq!(m2["id"], "m2");
    assert_eq!(m2["parent"], "m1");
    let header: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(header["id"], "abc-123");
    assert_eq!(header["model"], "claude-haiku-5-5");
    assert_eq!(header["started"], "2026-01-01T00:00:00.000Z");
}

#[test]
fn redacts_credentials_and_counts_them() {
    let mut a = Anonymizer::new(ident());
    let out = a.session_jsonl(fixture().as_bytes()).unwrap();
    let text = String::from_utf8(out.bytes).unwrap();
    assert!(!text.contains(&fake_key()));
    assert!(out.redacted >= 1);
}

#[test]
fn replaces_home_user_and_unknown_fields() {
    let mut a = Anonymizer::new(ident());
    let out = a.session_jsonl(fixture().as_bytes()).unwrap();
    let text = String::from_utf8(out.bytes).unwrap();
    assert!(!text.contains("alice"), "username leaked: {text}");
    assert!(!text.contains("secret.txt"));
    assert!(!text.contains("hello from"));
    assert!(!text.contains("deploy with token"));
    assert!(text.contains("\"~/dev/proj/src/main.rs\"") || text.contains("~/dev/"));
    assert!(
        text.contains("\"type\":\"mystery\""),
        "unknown type kept as a record"
    );
}

#[test]
fn same_input_yields_same_placeholder() {
    let mut a = Anonymizer::new(ident());
    let first = a.value(&serde_json::json!(["hello", "hello"]));
    assert_eq!(first[0], first[1]);
    let again = a.value(&serde_json::json!(["hello"]));
    assert_eq!(first[0], again[0]);
}

#[test]
fn bare_command_words_are_replaced_while_program_and_flags_stay() {
    let mut a = Anonymizer::new(ident());
    let echo = a.command("echo ConfidentialAcquisition");
    assert!(echo.starts_with("echo "), "{echo}");
    assert!(!echo.contains("Confidential"), "{echo}");
    let curl = a.command("curl -u alice --data=hunter2 /Users/alice/x.txt");
    assert!(curl.starts_with("curl -u "), "{curl}");
    assert!(curl.contains("--data="), "{curl}");
    assert!(!curl.contains("alice"), "{curl}");
    assert!(!curl.contains("hunter2"), "{curl}");
}

#[test]
fn nested_data_keys_and_values_are_anonymized_but_envelope_is_kept() {
    let mut a = Anonymizer::new(ident());
    let input = concat!(
        r#"{"type":"session","id":"s1","started":"2026-01-01T00:00:00.000Z","cwd":"/tmp/work","model":"m1","version":1}"#,
        "\n",
        r#"{"type":"message","id":"m1","parent":null,"ts":"2026-01-01T00:00:01.000Z","message":{"role":"assistant","content":"","tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"customer\":{\"name\":\"AliceSmith\"},\"alice@example.com\":1}"}}]}}"#,
        "\n",
        r#"{"type":"mystery","id":"x1","customer":{"name":"AliceSmith"},"/Users/alice/notes":{"owner":"bob"}}"#,
    );
    let out = a.session_jsonl(input.as_bytes()).unwrap();
    let text = String::from_utf8(out.bytes).unwrap();
    assert!(!text.contains("AliceSmith"), "{text}");
    assert!(!text.contains("alice@example.com"), "{text}");
    assert!(!text.contains("alice"), "{text}");
    assert!(!text.contains("customer"), "{text}");
    assert!(text.contains("\"name\":\"lookup\""), "{text}");
    assert!(text.contains("\"id\":\"call_1\""), "{text}");
    assert!(text.contains("\"role\":\"assistant\""), "{text}");
    assert!(text.contains("\"type\":\"mystery\""), "{text}");
    assert!(text.contains("\"id\":\"x1\""), "{text}");

    let value = a.value(&serde_json::json!({
        "role": "user",
        "content": "hi",
        "customer": {"name": "AliceSmith"},
        "/Users/alice/x": "v",
    }));
    let rendered = value.to_string();
    assert_eq!(value["role"], "user");
    assert!(!rendered.contains("AliceSmith"), "{rendered}");
    assert!(!rendered.contains("alice"), "{rendered}");
}

#[test]
fn path_keeps_extension_and_structure() {
    let mut a = Anonymizer::new(ident());
    let p = a.path("/Users/alice/dev/proj/src/lib.rs");
    assert_eq!(p.split('/').count(), 5);
    assert!(p.starts_with("~/dev/"));
    assert!(p.ends_with(".rs"));
    assert!(!p.contains("alice"));
}

#[test]
fn rejects_non_session_file() {
    let mut a = Anonymizer::new(ident());
    assert!(a.session_jsonl(br#"{"type":"message","id":"m"}"#).is_err());
    assert!(a.session_jsonl(b"garbage\n").is_err());
}

#[test]
fn safe_name_keeps_only_timestamped_names() {
    assert_eq!(
        safe_session_name(Path::new("/x/2026-01-01T00-00-00_abc123.jsonl")),
        "2026-01-01T00-00-00_abc123.jsonl"
    );
    assert_eq!(
        safe_session_name(Path::new("/x/alice secret.jsonl")),
        "session.jsonl"
    );
}
